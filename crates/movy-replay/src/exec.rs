use std::{ops::Deref, str::FromStr, sync::Arc};

use color_eyre::eyre::eyre;
use itertools::Itertools;
use log::{debug, trace, warn};
use move_core_types::account_address::AccountAddress;
use move_trace_format::{format::MoveTraceBuilder, interface::Tracer};
use movy_sui::{compile::SuiCompiledPackage, database::cache::ObjectSuiStoreCommit};
use movy_types::{error::MovyError, object::MoveOwner};
use serde::{Deserialize, Serialize};
use sui_types::{
    TypeTag,
    base_types::{ObjectID, SuiAddress},
    committee::ProtocolVersion,
    effects::{TransactionEffects, TransactionEffectsAPI},
    gas::SuiGasStatus,
    inner_temporary_store::InnerTemporaryStore,
    metrics::LimitsMetrics,
    object::Owner,
    storage::{BackingStore, ObjectStore, WriteKind},
    supported_protocol_versions::{Chain, ProtocolConfig},
    transaction::{
        Argument, CallArg, CheckedInputObjects, Command, InputObjectKind, ObjectReadResult,
        ObjectReadResultKind, ProgrammableTransaction, TransactionData, TransactionDataAPI,
        TransactionKind,
    },
};

use crate::{
    db::{ObjectStoreInfo, ObjectStoreMintObject},
    tracer::NopTracer,
};

#[derive(Clone)]
pub struct SuiExecutor<T> {
    pub db: T,
    pub protocol_config: ProtocolConfig,
    pub metrics: Arc<LimitsMetrics>,
    pub registry: prometheus::Registry,
    pub executor: Arc<dyn sui_execution::Executor + Send + Sync>,
}

pub struct ExecutionResults {
    pub effects: TransactionEffects,
    pub store: InnerTemporaryStore,
    pub gas: SuiGasStatus,
}

pub struct ExecutionTracedResults<R> {
    pub results: ExecutionResults,
    pub tracer: Option<R>,
}

impl<R> Deref for ExecutionTracedResults<R> {
    type Target = ExecutionResults;
    fn deref(&self) -> &Self::Target {
        &self.results
    }
}

impl<T> SuiExecutor<T>
where
    T: ObjectStore + BackingStore + ObjectSuiStoreCommit + ObjectStoreMintObject + ObjectStoreInfo,
{
    pub fn new(db: T) -> Result<Self, MovyError> {
        let protocol_config: ProtocolConfig =
            ProtocolConfig::get_for_version(ProtocolVersion::max(), Chain::Mainnet);
        let registry = prometheus::Registry::new();
        let metrics = Arc::new(LimitsMetrics::new(&registry));
        let executor = sui_execution::executor(&protocol_config, false)?;
        Ok(Self {
            db,
            protocol_config,
            metrics,
            registry,
            executor,
        })
    }

    pub fn run_tx_trace<R: Tracer>(
        &self,
        tx_data: TransactionData,
        epoch: u64,
        epoch_ms: u64,
        mut tracer: Option<R>,
    ) -> Result<ExecutionTracedResults<R>, MovyError> {
        let input_objects = match tx_data.input_objects() {
            Ok(v) => v,
            Err(e) => {
                warn!("Input objects have error: {}", e);
                return Err(eyre!("invalid ptb {}", e).into());
            }
        };

        let mut objects = vec![];
        for objref in input_objects {
            match objref {
                InputObjectKind::MovePackage(package) => {
                    let package = self
                        .db
                        .get_package_object(&package)?
                        .ok_or_else(|| eyre!("package {} not found", package))?;
                    objects.push(ObjectReadResult::new(
                        objref,
                        ObjectReadResultKind::Object(package.into()),
                    ));
                }
                InputObjectKind::ImmOrOwnedMoveObject((obj_id, version, _digest)) => {
                    let object = self
                        .db
                        .get_object_by_key(&obj_id, version)
                        .ok_or_else(|| eyre!("object {} {} not found", obj_id, version))?;
                    objects.push(ObjectReadResult::new(
                        objref,
                        ObjectReadResultKind::Object(object),
                    ));
                }
                InputObjectKind::SharedMoveObject {
                    id,
                    initial_shared_version,
                    mutability: _,
                } => match self.db.get_object(&id) {
                    Some(object) => {
                        if initial_shared_version == object.owner.start_version().unwrap() {
                            objects.push(ObjectReadResult::new(
                                objref,
                                ObjectReadResultKind::Object(object),
                            ));
                        } else {
                            return Err(eyre!(
                                "mismatched input: {:?} vs {}",
                                &object,
                                initial_shared_version
                            )
                            .into());
                        }
                    }
                    None => {
                        return Err(eyre!(
                            "Shared object {}:{} not found",
                            id,
                            initial_shared_version
                        )
                        .into());
                    }
                },
            }
        }

        let gas = if tx_data.is_system_tx() {
            SuiGasStatus::new_unmetered()
        } else {
            for gas in tx_data.gas() {
                let object = self
                    .db
                    .get_object_by_key(&gas.0, gas.1)
                    .ok_or_else(|| eyre!("gas {}:{} missing", gas.0, gas.1))?;
                objects.push(ObjectReadResult::new(
                    InputObjectKind::ImmOrOwnedMoveObject(*gas),
                    ObjectReadResultKind::Object(object),
                ));
            }
            SuiGasStatus::new(
                tx_data.gas_budget(),
                tx_data.gas_price(),
                0,
                &self.protocol_config,
            )?
        };

        let mut move_tracer = if let Some(tracer) = &mut tracer {
            let tracer = Box::new(tracer) as Box<dyn Tracer>;
            Some(MoveTraceBuilder::new_with_tracer(tracer))
        } else {
            None
        };
        trace!("Tx digest is {}", tx_data.digest());
        let (store, gas_status, effects, timing, result) =
            self.executor.execute_transaction_to_effects(
                &self.db,
                &self.protocol_config,
                self.metrics.clone(),
                false,
                Ok(()),
                &epoch,
                epoch_ms,
                CheckedInputObjects::new_for_replay(objects.into()),
                tx_data.gas_data().clone(),
                gas,
                tx_data.kind().clone(),
                tx_data.sender(),
                tx_data.digest(),
                &mut move_tracer,
            );
        drop(move_tracer);
        log::debug!("Result is {:?}", &result);
        Ok(ExecutionTracedResults {
            results: ExecutionResults {
                effects,
                store,
                gas: gas_status,
            },
            tracer,
        })
    }

    pub fn run_ptb_with_gas<R: Tracer>(
        &self,
        ptb: ProgrammableTransaction,
        epoch: u64,
        epoch_ms: u64,
        sender: SuiAddress,
        gas: ObjectID,
        tracer: Option<R>,
    ) -> Result<ExecutionTracedResults<R>, MovyError> {
        let gas = self.db.get_move_object_info(gas.into())?.sui_reference();
        let tx_kind = TransactionKind::ProgrammableTransaction(ptb.clone());
        let tx_data = TransactionData::new(tx_kind, sender, gas, 1_000_000_000, 1);

        self.run_tx_trace(tx_data, epoch, epoch_ms, tracer)
    }

    pub fn run_ptb_mint_gas<R: Tracer>(
        &self,
        ptb: ProgrammableTransaction,
        epoch: u64,
        epoch_ms: u64,
        sender: SuiAddress,
        tracer: Option<R>,
    ) -> Result<ExecutionTracedResults<R>, MovyError> {
        let gas_id = ObjectID::random();
        self.db.mint_coin_id(
            TypeTag::from_str("0x2::sui::SUI").unwrap().into(),
            MoveOwner::AddressOwner(sender.into()),
            gas_id.into(),
            10_000_000_000,
        )?;
        let gas_ref = self
            .db
            .get_move_object_info(gas_id.into())
            .unwrap()
            .sui_reference();

        self.run_ptb_with_gas(ptb, epoch, epoch_ms, sender, gas_ref.0, tracer)
    }

    pub fn deploy_contract(
        &mut self,
        epoch: u64,
        epoch_ms: u64,
        admin: SuiAddress,
        gas: ObjectID,
        project: SuiCompiledPackage,
    ) -> Result<ObjectID, MovyError> {
        let package_id = project.package_id;
        let (mut modules, dependencies) = project.into_deployment();

        debug!(
            "Deploying package with original id {} and dependencies {:?}",
            package_id, dependencies
        );
        // rebase to zero address as sui publish requires
        for it in modules.iter_mut() {
            let self_handle = it.self_handle().clone();
            if let Some(address_mut) = it
                .address_identifiers
                .get_mut(self_handle.address.0 as usize)
                && *address_mut != AccountAddress::ZERO
            {
                *address_mut = AccountAddress::ZERO;
            }

            // TODO: Maybe unnecessary?
            if package_id != ObjectID::ZERO {
                for ident in it.address_identifiers.iter_mut() {
                    if ObjectID::from(*ident) == package_id {
                        *ident = AccountAddress::ZERO;
                    }
                }
            }
        }
        let mut modules_bytes = vec![];
        for module in &modules {
            let mut buf = vec![];
            module.serialize_with_version(module.version, &mut buf)?;
            modules_bytes.push(buf);
        }

        let ptb = ProgrammableTransaction {
            inputs: vec![CallArg::Pure(bcs::to_bytes(&admin)?)],
            commands: vec![
                Command::Publish(modules_bytes, dependencies.clone()), // This produces an upgrade cap
                Command::TransferObjects(vec![Argument::Result(0)], Argument::Input(0)),
            ],
        };

        let out = self.run_ptb_with_gas::<NopTracer>(ptb, epoch, epoch_ms, admin, gas, None)?;
        let ExecutionResults { effects, store, .. } = out.results;
        // look for new objects
        let mut new_object = None;
        debug!(
            "all changed: {:?}, status is {:?}",
            effects.all_changed_objects(),
            effects.status()
        );
        for t in effects.all_changed_objects() {
            if matches!(&t.2, WriteKind::Create) && matches!(&t.1, Owner::Immutable) {
                let object = store.written.get(&t.0.0).unwrap();
                if object.is_package() {
                    new_object = Some(t.0);
                }
            }
        }
        if let Some(new_object) = new_object {
            debug!(
                "Contract deployed at {}, original id: {}",
                new_object.0, package_id
            );

            self.db.commit_store(store, &effects)?;
            Ok(new_object.0)
        } else {
            Err(eyre!("fail to deploy").into())
        }
    }
}
