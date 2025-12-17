use std::{ops::Deref, str::FromStr, sync::Arc, time::Duration};

use color_eyre::eyre::eyre;
use fastcrypto::hash::HashFunction;
use fastcrypto::traits::Signer;
use itertools::Itertools;
use log::{debug, info};
use movy_types::error::MovyError;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use shared_crypto::intent::{Intent, IntentMessage};
use sui_rpc::{
    field::{FieldMask, FieldMaskUtil},
    proto::sui::rpc::v2::{
        CoinMetadata, Epoch, ExecuteTransactionRequest, ExecutedTransaction, GetCheckpointRequest,
        GetCoinInfoRequest, GetEpochRequest, GetObjectRequest, GetTransactionRequest,
        ListDynamicFieldsRequest, ListOwnedObjectsRequest, SignatureScheme, SimpleSignature,
        SimulateTransactionRequest, Transaction, UserSignature,
        ledger_service_client::LedgerServiceClient,
        simulate_transaction_request::TransactionChecks, state_service_client::StateServiceClient,
        subscription_service_client::SubscriptionServiceClient,
        transaction_execution_service_client::TransactionExecutionServiceClient,
        user_signature::Signature,
    },
};

use sui_sdk::{SuiClient, SuiClientBuilder, rpc_types::SuiTransactionBlockResponseOptions};
use sui_types::{
    TypeTag,
    balance_change::BalanceChange,
    base_types::{ObjectID, ObjectRef, SuiAddress},
    crypto::SuiKeyPair,
    digests::TransactionDigest,
    effects::{TransactionEffects, TransactionEvents},
    event::Event,
    messages_checkpoint::{CheckpointContents, CheckpointSummary},
    object::{Data, MoveObject, Object, Owner},
    transaction::{ObjectArg, SenderSignedData, SharedObjectMutability, TransactionData},
};
use sui_types::{crypto::SuiSignature, digests::ObjectDigest};
use tokio::task::JoinSet;
use tonic::{Code, codec::CompressionEncoding};

pub fn sign_tx(
    tx: TransactionData,
    key: &SuiKeyPair,
) -> Result<(IntentMessage<TransactionData>, sui_types::crypto::Signature), MovyError> {
    let intent_msg = IntentMessage::new(Intent::sui_transaction(), tx);
    let raw_tx = bcs::to_bytes(&intent_msg)?;
    let mut hasher = sui_types::crypto::DefaultHash::default();
    hasher.update(raw_tx.clone());
    let digest = hasher.finalize().digest;
    let sui_sig = key.sign(&digest);
    Ok((intent_msg, sui_sig))
}

#[derive(Debug, Clone)]
pub struct RPCCheckpoint {
    pub contents: CheckpointContents,
    pub summary: CheckpointSummary,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QuickNodeAuth(pub Option<String>);

impl tonic::service::Interceptor for QuickNodeAuth {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(s) = &self.0 {
            req.metadata_mut()
                .insert("x-token", s.clone().try_into().unwrap());
        }

        Ok(req)
    }
}

#[derive(Debug, Clone)]
pub struct ExecutionOutcome {
    pub effects: TransactionEffects,
    pub output_objects: Vec<Object>,
    pub events: Vec<Event>,
    pub balance_changes: Vec<BalanceChange>,
}

type Channel =
    tonic::service::interceptor::InterceptedService<tonic::transport::Channel, QuickNodeAuth>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SuiGrpcArg {
    Plain(String),
    QuickNode(String, QuickNodeAuth, Option<String>),
}

impl SuiGrpcArg {
    pub async fn grpc(&self) -> Result<SuiGrpcClient, MovyError> {
        match self {
            Self::Plain(u) => {
                let json = SuiClientBuilder::default().build(u).await?;
                Ok(SuiGrpcClient::new(u)?.with_json(json))
            }
            Self::QuickNode(u, a, j) => {
                let out = SuiGrpcClient::new_auth(u, a.clone())?;
                let out = if let Some(j) = j {
                    let json = SuiClientBuilder::default().build(j).await?;
                    out.with_json(json)
                } else {
                    out
                };
                Ok(out)
            }
        }
    }
}

impl FromStr for SuiGrpcArg {
    type Err = MovyError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let tks: Vec<_> = s.split(",").collect();

        if tks.len() == 3 && tks[0].to_lowercase() == "quicknode" {
            // Old way
            Ok(Self::QuickNode(
                tks[2].to_string(),
                QuickNodeAuth(Some(tks[1].to_string())),
                None,
            ))
        } else if tks.len() == 2 && tks[0].to_lowercase() == "quicknode" {
            let url = url::Url::parse(tks[1]).map_err(|e| eyre!("invalid url {}", tks[1]))?;
            Ok(Self::QuickNode(
                format!(
                    "{}://{}:9000",
                    url.scheme(),
                    url.host_str().ok_or_else(|| eyre!("no host from {}", s))?
                ),
                QuickNodeAuth(Some(url.path().replace("/", ""))),
                Some(tks[1].to_string()),
            ))
        } else {
            Ok(Self::Plain(s.to_string()))
        }
    }
}

#[derive(Clone, Debug)]
pub struct TransactionResponse {
    pub tx: TransactionData,
    pub effects: TransactionEffects,
    pub checkpoint: u64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct FullCheckpoint {
    pub checkpoint: u64,
    pub epoch: u64,
    pub txs: Vec<TransactionResponse>,
}

#[derive(Clone)]
pub struct SuiGrpcClient {
    pub inner: Arc<SuiGrpcClientInner>,
}

impl SuiGrpcClient {
    pub fn new(url: &str) -> Result<Self, MovyError> {
        Self::new_auth(url, QuickNodeAuth::default())
    }

    pub fn with_json(&self, json: SuiClient) -> Self {
        Self {
            inner: Arc::new(self.inner.with_json(json)),
        }
    }

    pub fn new_auth(url: &str, auth: QuickNodeAuth) -> Result<Self, MovyError> {
        Ok(Self {
            inner: Arc::new(SuiGrpcClientInner::new_auth(url, auth)?),
        })
    }

    pub async fn get_objects_parallel(
        &self,
        objects: Vec<ObjectID>,
        parallel: usize,
    ) -> Result<Vec<Option<Object>>, MovyError> {
        let objects = objects
            .into_iter()
            .chunks(parallel)
            .into_iter()
            .map(|v| v.collect_vec())
            .collect_vec();
        let mut out = vec![];
        for group in objects {
            let mut js = JoinSet::new();
            for id in group {
                let rpc = self.clone();
                js.spawn(async move { rpc.get_object_may_empty(id).await });
            }
            out.extend(
                js.join_all()
                    .await
                    .into_iter()
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter(),
            );
        }
        Ok(out)
    }
}

impl Deref for SuiGrpcClient {
    type Target = SuiGrpcClientInner;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[derive(Clone)]
pub struct SuiGrpcClientInner {
    pub channel: tonic::transport::Channel,
    pub auth: QuickNodeAuth,
    // https://github.com/MystenLabs/sui/issues/23565
    pub json: Option<SuiClient>,
}

impl SuiGrpcClientInner {
    pub fn new(url: &str) -> Result<Self, MovyError> {
        Self::new_auth(url, QuickNodeAuth::default())
    }

    pub fn with_json(&self, json: SuiClient) -> Self {
        Self {
            channel: self.channel.clone(),
            auth: self.auth.clone(),
            json: Some(json),
        }
    }

    pub fn new_auth(url: &str, auth: QuickNodeAuth) -> Result<Self, MovyError> {
        let uri = tonic::transport::Uri::from_str(url).map_err(|e| eyre!("invalid url: {}", e))?;
        let mut endpoint = tonic::transport::Endpoint::from(uri.clone());

        if uri.scheme_str().map(|v| v.to_lowercase()) == Some("https".to_string()) {
            endpoint = endpoint
                .tls_config(tonic::transport::ClientTlsConfig::new().with_enabled_roots())
                .map_err(Into::into)
                .map_err(tonic::Status::from_error)?;
        }
        let channel = endpoint
            .connect_timeout(Duration::from_secs(5))
            .http2_keep_alive_interval(Duration::from_secs(5))
            .connect_lazy();
        Ok(Self {
            channel,
            auth,
            json: None,
        })
    }

    pub fn subscription_client(&self) -> SubscriptionServiceClient<Channel> {
        SubscriptionServiceClient::with_interceptor(self.channel.clone(), self.auth.clone())
            .accept_compressed(CompressionEncoding::Zstd)
            .max_decoding_message_size(256 * 1024 * 1024)
    }

    pub fn live_data_client(&self) -> StateServiceClient<Channel> {
        StateServiceClient::with_interceptor(self.channel.clone(), self.auth.clone())
            .accept_compressed(CompressionEncoding::Zstd)
            .max_decoding_message_size(256 * 1024 * 1024)
    }

    pub fn ledger_client(&self) -> LedgerServiceClient<Channel> {
        LedgerServiceClient::with_interceptor(self.channel.clone(), self.auth.clone())
            .accept_compressed(CompressionEncoding::Zstd)
            .max_decoding_message_size(256 * 1024 * 1024)
    }

    pub fn execution_client(&self) -> TransactionExecutionServiceClient<Channel> {
        TransactionExecutionServiceClient::with_interceptor(self.channel.clone(), self.auth.clone())
            .accept_compressed(CompressionEncoding::Zstd)
            .max_decoding_message_size(256 * 1024 * 1024)
    }

    pub async fn get_coin_meta(&self, coin: String) -> Result<Option<CoinMetadata>, MovyError> {
        // let req = GetCoinInfoRequest {
        //     coin_type: Some(coin),
        // };
        debug!("[rpc] get_coin_meta for {}", &coin);
        let mut req = GetCoinInfoRequest::default();
        req.coin_type = Some(coin);

        let resp = self
            .live_data_client()
            .get_coin_info(req)
            .await?
            .into_inner();
        Ok(resp.metadata)
    }

    pub async fn get_coin_decimal(&self, coin: String) -> Result<u32, MovyError> {
        Ok(self
            .get_coin_meta(coin.clone())
            .await?
            .ok_or_else(|| eyre!("no such coin: {:?}", coin))?
            .decimals
            .ok_or_else(|| eyre!("no decimal given"))?)
    }

    pub async fn get_epoch(&self, epoch: Option<u64>) -> Result<Epoch, MovyError> {
        debug!("[rpc] get_epoch for {:?}", &epoch);
        let req = if let Some(epoch) = epoch {
            GetEpochRequest::new(epoch)
        } else {
            GetEpochRequest::latest()
        }
        .with_read_mask(FieldMask::from_str(
            "epoch,reference_gas_price,system_state",
        ));
        let resp = self.ledger_client().get_epoch(req).await?.into_inner();
        Ok(resp.epoch.unwrap())
    }

    pub async fn get_checkpoint_transactions(
        &self,
        ckpt: u64,
    ) -> Result<FullCheckpoint, MovyError> {
        debug!("[rpc] get_checkpoint_transactions for {}", &ckpt);
        let req = GetCheckpointRequest::by_sequence_number(ckpt)
        .with_read_mask(FieldMask::from_str("sequence_number,transactions.transaction.timestamp,transactions.transaction.bcs,transactions.effects.bcs,summary.epoch"));
        let resp = self.ledger_client().get_checkpoint(req).await?.into_inner();

        let mut txs = vec![];
        let ckpt = resp.checkpoint.unwrap();
        for tx in ckpt.transactions {
            let effects = tx.effects.unwrap();
            let ts = chrono::DateTime::from_timestamp(
                tx.timestamp.unwrap().seconds as _,
                tx.timestamp.unwrap().nanos as _,
            )
            .ok_or_else(|| eyre!("invalid timestamp {:?}", &tx.timestamp))?;
            let tx = tx.transaction.unwrap();

            let tx_data: TransactionData = bcs::from_bytes(&tx.bcs.unwrap().value.unwrap())?;
            let effects: TransactionEffects =
                bcs::from_bytes(&effects.bcs.unwrap().value.unwrap())?;
            txs.push(TransactionResponse {
                effects,
                tx: tx_data,
                checkpoint: ckpt.sequence_number.unwrap(),
                timestamp: ts,
            });
        }

        Ok(FullCheckpoint {
            checkpoint: ckpt.sequence_number.unwrap(),
            txs,
            epoch: ckpt.summary.unwrap().epoch.unwrap(),
        })
    }

    pub async fn get_checkpoint(&self, ckpt: Option<u64>) -> Result<RPCCheckpoint, MovyError> {
        // let req = GetCheckpointRequest {
        //     checkpoint_id: ckpt.map(CheckpointId::SequenceNumber),
        //     read_mask: Some(FieldMask::from_str("sequence_number,summary.epoch")),
        // };
        debug!("[rpc] get_checkpoint for {:?}", &ckpt);
        let req = if let Some(ckpt) = ckpt {
            GetCheckpointRequest::by_sequence_number(ckpt)
        } else {
            GetCheckpointRequest::latest()
        }
        .with_read_mask(FieldMask::from_str("contents.bcs,summary.bcs"));

        let resp = self.ledger_client().get_checkpoint(req).await?.into_inner();
        let checkpoint = resp.checkpoint.unwrap();
        let contents: CheckpointContents =
            bcs::from_bytes(&checkpoint.contents.unwrap().bcs.unwrap().value.unwrap())?;
        let summary: CheckpointSummary =
            bcs::from_bytes(&checkpoint.summary.unwrap().bcs.unwrap().value.unwrap())?;
        Ok(RPCCheckpoint { contents, summary })
    }

    pub async fn get_latest_checkpoint(&self) -> Result<u64, MovyError> {
        Ok(self.get_checkpoint(None).await?.summary.sequence_number)
    }

    pub async fn get_reference_gas_price(&self) -> Result<u64, MovyError> {
        Ok(self.get_epoch(None).await?.reference_gas_price.unwrap())
    }

    pub async fn owned_objects(
        &self,
        sender: SuiAddress,
        ty: Option<String>,
    ) -> Result<Vec<Object>, MovyError> {
        debug!("[rpc] List owned objects for {sender} and ty is {:?}", &ty);

        let mut page_token = None;
        let mut client = self.live_data_client();
        let mut out = vec![];
        loop {
            debug!(
                "Request with token {:?}",
                page_token.as_ref().map(const_hex::encode)
            );
            // let req = ListOwnedObjectsRequest {
            //     owner: Some(sender.to_string()),
            //     page_size: None,
            //     page_token,
            //     read_mask: Some(FieldMask::from_str("bcs")),
            //     object_type: ty.clone(),
            // };

            let mut req = ListOwnedObjectsRequest::default();
            req.owner = Some(sender.to_string());
            req.read_mask = Some(FieldMask::from_str("bcs"));
            req.object_type = ty.clone();
            req.page_token = page_token;

            let resp = client.list_owned_objects(req).await?.into_inner();
            let cnt = resp.objects.len();
            for obj in resp.objects {
                let object: Object = bcs::from_bytes(obj.bcs.unwrap().value())?;
                out.push(object);
            }
            if resp.next_page_token.is_none() || cnt == 0 {
                break;
            }
            page_token = resp.next_page_token;
        }
        Ok(out)
    }

    pub async fn coins_from_sender(
        &self,
        sender: SuiAddress,
        coin: Option<String>,
    ) -> Result<Vec<Object>, MovyError> {
        Ok(self
            .owned_objects(sender, Some("0x2::coin::Coin".to_string()))
            .await?
            .into_iter()
            .filter(|t| {
                coin.as_ref()
                    .and_then(|cty| {
                        t.type_()
                            .and_then(|ty| ty.coin_type_maybe())
                            .map(|t| &t.to_canonical_string(true) == cty)
                    })
                    .unwrap_or(true)
            })
            .collect())
    }

    pub async fn gas_coins_from_sender(
        &self,
        sender: SuiAddress,
    ) -> Result<Vec<Object>, MovyError> {
        self.coins_from_sender(
            sender,
            Some(
                "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI"
                    .to_string(),
            ),
        )
        .await
    }

    pub async fn get_object_ref(&self, object_id: ObjectID) -> Result<ObjectRef, MovyError> {
        log::debug!("[RPC] get_object_ref for {}", object_id);
        let req = GetObjectRequest::new(&object_id.into_bytes().into())
            .with_read_mask(FieldMask::from_str("object_id,version,digest"));

        let object = self
            .ledger_client()
            .get_object(req)
            .await?
            .into_inner()
            .object
            .unwrap();
        Ok((
            ObjectID::from_str(&object.object_id.unwrap()).unwrap(),
            object.version.unwrap().into(),
            ObjectDigest::from_str(&object.digest.unwrap()).unwrap(),
        ))
    }

    pub async fn get_move_object_typed<T: DeserializeOwned>(
        &self,
        object_id: ObjectID,
    ) -> Result<T, MovyError> {
        let object = self.get_move_object(object_id).await?;
        Ok(bcs::from_bytes(&object.into_contents())?)
    }

    pub async fn get_move_object(&self, object_id: ObjectID) -> Result<MoveObject, MovyError> {
        let object = self.get_object(object_id).await?;
        match object.into_inner().data {
            Data::Move(mv) => Ok(mv),
            _ => Err(eyre!("expect move object for {} but get package", object_id).into()),
        }
    }

    pub async fn get_move_object_may_empty(
        &self,
        object_id: ObjectID,
    ) -> Result<Option<MoveObject>, MovyError> {
        match self.get_object_may_empty(object_id).await? {
            Some(v) => match v.into_inner().data {
                Data::Move(mv) => Ok(Some(mv)),
                _ => Err(eyre!("expect move object for {} but get package", object_id).into()),
            },
            None => Ok(None),
        }
    }

    pub async fn get_object_may_empty(
        &self,
        object_id: ObjectID,
    ) -> Result<Option<Object>, MovyError> {
        log::debug!("[RPC] get_object_may_empty for {}", object_id);
        let req = GetObjectRequest::new(&object_id.into_bytes().into())
            .with_read_mask(FieldMask::from_str("bcs"));

        let object = self.ledger_client().get_object(req).await;
        match object {
            Ok(v) => Ok(Some(bcs::from_bytes(
                v.into_inner().object.unwrap().bcs.unwrap().value(),
            )?)),
            Err(e) => match e.code() {
                Code::NotFound => Ok(None),
                _ => Err(e.into()),
            },
        }
    }

    pub async fn get_object(&self, object_id: ObjectID) -> Result<Object, MovyError> {
        Ok(self
            .get_object_may_empty(object_id)
            .await?
            .ok_or_else(|| eyre!("{} not exist", object_id))?)
    }

    pub async fn list_dynamic_fields(
        &self,
        object_id: ObjectID,
        ty: Option<TypeTag>,
    ) -> Result<Vec<Object>, MovyError> {
        log::debug!("[RPC] list_dynamic_fields for {}", object_id);
        let mut page_token = None;
        let mut client = self.live_data_client();
        let mut out = vec![];
        loop {
            let mut req = ListDynamicFieldsRequest::default();
            req.parent = Some(object_id.to_canonical_string(true));
            req.page_size = Some(256);
            req.page_token = page_token;
            req.read_mask = Some(FieldMask::from_str("kind,field_object"));
            let resp = client.list_dynamic_fields(req).await?.into_inner();
            let cnt = resp.dynamic_fields.len();
            for fd in resp.dynamic_fields {
                let object: Object =
                    bcs::from_bytes(fd.field_object.unwrap().bcs.unwrap().value())?;
                if let Some(ty) = &ty {
                    let object_ty = object
                        .data
                        .try_as_move()
                        .ok_or_else(|| eyre!("field {} not move obj?!", object.id()))?
                        .type_();
                    if &TypeTag::from(object_ty.clone()) != ty {
                        continue;
                    }
                }
                out.push(object);
            }
            if resp.next_page_token.is_none() || cnt == 0 {
                break;
            }
            page_token = resp.next_page_token;
        }
        Ok(out)
    }

    pub async fn shared_object_arg(
        &self,
        object_id: ObjectID,
        mutable: bool,
    ) -> Result<ObjectArg, MovyError> {
        let object = self.get_object(object_id).await?;
        match object.owner() {
            Owner::Shared {
                initial_shared_version,
            } => Ok(ObjectArg::SharedObject {
                id: object_id,
                initial_shared_version: *initial_shared_version,
                mutability: if mutable {
                    SharedObjectMutability::Mutable
                } else {
                    SharedObjectMutability::Immutable
                },
            }),
            _ => Err(eyre!("unexpected {:?}", object.owner()).into()),
        }
    }

    fn extract_exec_out(tx: ExecutedTransaction) -> Result<ExecutionOutcome, MovyError> {
        let effects: TransactionEffects =
            bcs::from_bytes(tx.effects.unwrap().bcs.unwrap().value())?;
        let evs = if let Some(ev) = tx.events {
            let events: TransactionEvents = bcs::from_bytes(ev.bcs.unwrap().value())?;
            events.data
        } else {
            vec![]
        };
        let mut changes = vec![];
        for bc in tx.balance_changes.into_iter() {
            changes.push(BalanceChange {
                address: SuiAddress::from_str(bc.address())?,
                coin_type: TypeTag::from_str(bc.coin_type())?,
                amount: i128::from_str(bc.amount())
                    .map_err(|e| eyre!("fail to parse change amount {}", e))?,
            });
        }

        let mut output_objects = vec![];
        for obj in tx
            .objects
            .map(|v| v.objects.into_iter())
            .into_iter()
            .flatten()
        {
            let object: Object = bcs::from_bytes(&obj.bcs.unwrap().value.unwrap())?;
            output_objects.push(object);
        }
        Ok(ExecutionOutcome {
            effects,
            output_objects,
            events: evs,
            balance_changes: changes,
        })
    }

    pub async fn sign_and_execute_transaction(
        &self,
        tx: TransactionData,
        kp: &SuiKeyPair,
    ) -> Result<ExecutionOutcome, MovyError> {
        log::debug!("[RPC] sign_and_execute_transaction for {}", tx.digest());
        let (intent, sui_sig) = sign_tx(tx, kp)?;

        let scheme = match kp {
            SuiKeyPair::Ed25519(_) => SignatureScheme::Ed25519,
            SuiKeyPair::Secp256k1(_) => SignatureScheme::Secp256k1,
            SuiKeyPair::Secp256r1(_) => SignatureScheme::Secp256r1,
        };
        let mut sig = UserSignature::default();
        let mut simpl_sig = SimpleSignature::default();
        simpl_sig.scheme = Some(scheme.into());
        simpl_sig.signature = Some(sui_sig.signature_bytes().to_vec().into());
        simpl_sig.public_key = Some(kp.public().as_ref().to_vec().into());
        sig.scheme = Some(scheme.into());
        sig.signature = Some(Signature::Simple(simpl_sig));
        let req = ExecuteTransactionRequest::new(Transaction::from(intent.value))
            .with_signatures(vec![sig])
            .with_read_mask(FieldMask::from_str(
                "transaction,effects,events,balance_changes",
            ));
        // let req = ExecuteTransactionRequest {
        //     transaction: Some(Transaction::from(tx)),
        //     signatures: vec![UserSignature {
        //         bcs: None,
        //         scheme: Some(scheme.into()),
        //         signature: Some(Signature::Simple(SimpleSignature {
        //             scheme: Some(scheme.into()),
        //             // Use signature_bytes() here
        //             signature: Some(sui_sig.signature_bytes().to_vec().into()),
        //             public_key: Some(kp.public().as_ref().to_vec().into()),
        //         })),
        //     }],
        //     read_mask: Some(FieldMask::from_str(
        //         "finality,transaction,transaction.effects,transaction.events,transaction.balance_changes",
        //     )),
        // };

        info!("Going to execute transaction ...");
        let resp = self
            .execution_client()
            .execute_transaction(req)
            .await?
            .into_inner();
        let exec: ExecutedTransaction = resp.transaction.ok_or_else(|| eyre!("no executed"))?;
        let oc = Self::extract_exec_out(exec)?;
        Ok(oc)
    }

    pub async fn dry_run_transaction(
        &self,
        tx: TransactionData,
    ) -> Result<ExecutionOutcome, MovyError> {
        log::debug!("[RPC] dry_run_transaction for {}", tx.digest());
        let req: SimulateTransactionRequest = SimulateTransactionRequest::new(Transaction::from(tx))
            .with_read_mask(FieldMask::from_str(
                "transaction,transaction.effects,transaction.events,transaction.balance_changes,outputs.return_values,transaction.objects.bcs",
            ))
            .with_checks(TransactionChecks::Enabled)
            .with_do_gas_selection(false);
        // let req = SimulateTransactionRequest {
        //     transaction: Some(Transaction::from(tx)),
        //     read_mask: Some(FieldMask::from_str(
        //         "transaction,transaction.effects,transaction.events,transaction.balance_changes,outputs.return_values",
        //     )),
        //     checks: Some(TransactionChecks::Enabled.into()),
        //     do_gas_selection: Some(false),
        // };
        let resp = self
            .execution_client()
            .simulate_transaction(req)
            .await?
            .into_inner();
        let tx = resp.transaction.unwrap();

        let oc = Self::extract_exec_out(tx)?;
        Ok(oc)
    }

    pub async fn get_transaction(
        &self,
        digest: TransactionDigest,
    ) -> Result<Option<TransactionResponse>, MovyError> {
        log::debug!("[RPC] get_transaction for {}", digest);
        if let Some(json) = &self.json {
            match json
                .read_api()
                .get_transaction_with_options(
                    digest,
                    SuiTransactionBlockResponseOptions {
                        show_raw_effects: true,
                        show_raw_input: true,
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(tx) => {
                    let effects: TransactionEffects = bcs::from_bytes(&tx.raw_effects)?;
                    let checkpoint = if let Some(ckpt) = tx.checkpoint {
                        ckpt
                    } else {
                        return Ok(None);
                    };
                    let signed: SenderSignedData = bcs::from_bytes(&tx.raw_transaction)?;
                    let ts = chrono::DateTime::from_timestamp_millis(tx.timestamp_ms.unwrap() as _)
                        .ok_or_else(|| eyre!("timestamp invalid for {}", digest))?;
                    let tx = signed.into_inner().intent_message.value;
                    Ok(Some(TransactionResponse {
                        tx,
                        effects,
                        checkpoint,
                        timestamp: ts,
                    }))
                }
                Err(sui_sdk::error::Error::RpcError(e)) => {
                    debug!("Tx error: {:?}", e);
                    Ok(None)
                }
                Err(e) => Err(e.into()),
            }
        } else {
            let req = GetTransactionRequest::new(&digest.into_inner().into()).with_read_mask(
                FieldMask::from_str("effects,transaction,checkpoint,timestamp"),
            );
            // let req = GetTransactionRequest {
            //     digest: Some(digest.to_string()),
            //     read_mask: Some(FieldMask::from_str("effects,transaction,checkpoint")),
            // };

            match self.ledger_client().get_transaction(req).await {
                Ok(tx) => {
                    let inner = tx.into_inner().transaction.unwrap();
                    let ts = chrono::DateTime::from_timestamp(
                        inner.timestamp.unwrap().seconds as _,
                        inner.timestamp.unwrap().nanos as _,
                    )
                    .ok_or_else(|| eyre!("invalid timestamp {:?}", &inner.timestamp))?;
                    let tx: sui_sdk_types::Transaction =
                        bcs::from_bytes(&inner.transaction.unwrap().bcs.unwrap().value.unwrap())?;

                    let tx = TransactionData::new_with_gas_coins(
                        tx.kind.try_into()?,
                        tx.sender.into(),
                        tx.gas_payment
                            .objects
                            .into_iter()
                            .map(|v| {
                                let (addr, version, digest) = v.into_parts();
                                (
                                    ObjectID::from_address(addr.into_inner().into()),
                                    version.into(),
                                    digest.into(),
                                )
                            })
                            .collect(),
                        tx.gas_payment.budget,
                        tx.gas_payment.price,
                    );
                    let effects =
                        bcs::from_bytes(&inner.effects.unwrap().bcs.unwrap().value.unwrap())?;
                    let checkpoint = inner.checkpoint.unwrap();
                    Ok(Some(TransactionResponse {
                        tx,
                        effects,
                        checkpoint,
                        timestamp: ts,
                    }))
                }
                Err(e) => match e.code() {
                    tonic::Code::NotFound => Ok(None),
                    _ => Err(e.into()),
                },
            }
        }
    }

    pub async fn transaction_checkpoint(&self, digest: String) -> Result<Option<u64>, MovyError> {
        let mut req = GetTransactionRequest::default();
        req.digest = Some(digest);
        req.read_mask = Some(FieldMask::from_str("effects,transaction,checkpoint"));
        // let req = GetTransactionRequest::new(&digest.into_inner().into())
        //         .with_read_mask(FieldMask::from_str("effects,transaction,checkpoint"));
        // let req = GetTransactionRequest {
        //     digest: Some(digest),
        //     read_mask: Some(FieldMask::from_str("transaction,checkpoint")),
        // };
        match self.ledger_client().get_transaction(req).await {
            Ok(tx) => Ok(tx.into_inner().transaction.unwrap().checkpoint),
            Err(e) => match e.code() {
                tonic::Code::NotFound => Ok(None),
                _ => Err(e.into()),
            },
        }
    }
}
