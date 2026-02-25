use std::{
    collections::BTreeMap,
    ops::Deref,
    str::FromStr,
    sync::{Arc, LazyLock, Mutex},
};

use color_eyre::eyre::eyre;
use itertools::Itertools;
use move_trace_format::{format::MoveTraceBuilder, interface::Tracer};
use move_vm_runtime::move_vm::MoveVM;
use movy_sui::{compile::SuiCompiledPackage, database::cache::ObjectSuiStoreCommit};
use movy_types::{error::MovyError, input::MoveAddress, object::MoveOwner};
use sui_adapter_latest::execution_mode::{ExecutionMode, Normal};
use sui_move_natives_latest::all_natives;
use sui_types::{
    Identifier, TypeTag,
    base_types::{ObjectID, SuiAddress},
    committee::ProtocolVersion,
    digests::TransactionDigest,
    effects::{TransactionEffects, TransactionEffectsAPI},
    gas::SuiGasStatus,
    inner_temporary_store::InnerTemporaryStore,
    metrics::LimitsMetrics,
    move_package::{MovePackage, UpgradePolicy},
    object::Owner,
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    storage::{BackingStore, ObjectStore, WriteKind},
    supported_protocol_versions::{Chain, ProtocolConfig},
    transaction::{
        Argument, CallArg, CheckedInputObjects, Command, InputObjectKind, ObjectArg,
        ObjectReadResult, ObjectReadResultKind, ProgrammableTransaction, TransactionData,
        TransactionDataAPI, TransactionKind,
    },
};
use tracing::{debug, trace, warn};

use crate::{
    db::{ObjectStoreInfo, ObjectStoreMintObject},
    tracer::{MovySuiTracerExt, MovySuiTracerWrapper, NopTracer},
};

pub fn testing_proto() -> ProtocolConfig {
    ProtocolConfig::get_for_version(ProtocolVersion::max(), Chain::Mainnet)
}

fn random_digest() -> TransactionDigest {
    TransactionDigest::from_str("8thja5nUwaEw7L5ji9tnhCurpkjHdMunRffxwx1H9HsT").unwrap()
}

#[derive(Clone)]
pub struct SuiExecutor<T> {
    pub db: T,
    pub protocol_config: ProtocolConfig,
    pub metrics: Arc<LimitsMetrics>,
    pub registry: prometheus::Registry,
    pub movevm: Arc<MoveVM>,
    pub deploy_ids: u64,
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
    pub fn new_with_cheats_storage(db: T) -> Result<Self, MovyError> {
        let protocol_config = testing_proto();
        let registry = prometheus::Registry::new();
        let metrics = Arc::new(LimitsMetrics::new(&registry));
        let movevm = Arc::new(
            MoveVM::new(
                all_natives(false, &protocol_config).into_iter(), // .chain(cheats.into_iter()),
            )
            .map_err(|e| eyre!("move vm err: {}", e))?,
        );
        Ok(Self {
            db,
            protocol_config,
            metrics,
            registry,
            movevm,
            deploy_ids: 0,
        })
    }
    pub fn new(db: T) -> Result<Self, MovyError> {
        Self::new_with_cheats_storage(db)
    }

    fn run_tx_trace_inner<R: Tracer>(
        &self,
        mut tx_data: TransactionData,
        epoch: u64,
        epoch_ms: u64,
        mut tracer: Option<R>,
        target_deployment_id: Option<ObjectID>,
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

        let (store, gas_status, effects, _timing, result) =
            if let Some(target) = target_deployment_id {
                let _deploy = GlobalDeployment::new(&mut tx_data, target);
                sui_adapter_latest::execution_engine::execute_transaction_to_effects::<SuiFuzzMode>(
                    &self.db,
                    CheckedInputObjects::new_for_replay(objects.into()),
                    tx_data.gas_data().clone(),
                    gas,
                    tx_data.kind().clone(),
                    tx_data.sender(),
                    tx_data.digest(),
                    &self.movevm,
                    &epoch,
                    epoch_ms,
                    &self.protocol_config,
                    self.metrics.clone(),
                    false,
                    Ok(()),
                    &mut move_tracer,
                )
            } else {
                sui_adapter_latest::execution_engine::execute_transaction_to_effects::<Normal>(
                    &self.db,
                    CheckedInputObjects::new_for_replay(objects.into()),
                    tx_data.gas_data().clone(),
                    gas,
                    tx_data.kind().clone(),
                    tx_data.sender(),
                    tx_data.digest(),
                    &self.movevm,
                    &epoch,
                    epoch_ms,
                    &self.protocol_config,
                    self.metrics.clone(),
                    false,
                    Ok(()),
                    &mut move_tracer,
                )
            };

        drop(move_tracer);
        tracing::debug!("Result is {:?}", &result);
        Ok(ExecutionTracedResults {
            results: ExecutionResults {
                effects,
                store,
                gas: gas_status,
            },
            tracer,
        })
    }

    pub fn run_tx_trace<R: Tracer>(
        &self,
        tx_data: TransactionData,
        epoch: u64,
        epoch_ms: u64,
        tracer: Option<R>,
    ) -> Result<ExecutionTracedResults<R>, MovyError> {
        self.run_tx_trace_inner(tx_data, epoch, epoch_ms, tracer, None)
    }

    pub fn run_ptb_with_movy_tracer_gas<R: MovySuiTracerExt>(
        &self,
        ptb: ProgrammableTransaction,
        epoch: u64,
        epoch_ms: u64,
        sender: SuiAddress,
        gas: ObjectID,
        tracer: Option<R>,
    ) -> Result<ExecutionTracedResults<R>, MovyError> {
        let tracer = tracer.map(|v| MovySuiTracerWrapper::from(v));
        let v = self.run_ptb_with_gas(ptb, epoch, epoch_ms, sender, gas, tracer)?;
        Ok(ExecutionTracedResults {
            results: v.results,
            tracer: v.tracer.map(|t| t.tracer),
        })
    }

    fn run_ptb_with_gas_inner<R: Tracer>(
        &self,
        ptb: ProgrammableTransaction,
        epoch: u64,
        epoch_ms: u64,
        sender: SuiAddress,
        gas: ObjectID,
        tracer: Option<R>,
        target_deployment: Option<ObjectID>,
    ) -> Result<ExecutionTracedResults<R>, MovyError> {
        let gas = self.db.get_move_object_info(gas.into())?.sui_reference();
        let tx_kind = TransactionKind::ProgrammableTransaction(ptb.clone());
        let tx_data = TransactionData::new(tx_kind, sender, gas, 1_000_000_000, 1);

        self.run_tx_trace_inner(tx_data, epoch, epoch_ms, tracer, target_deployment)
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
        self.run_ptb_with_gas_inner(ptb, epoch, epoch_ms, sender, gas, tracer, None)
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

    pub fn upgrade_contract(
        &mut self,
        epoch: u64,
        epoch_ms: u64,
        deployer: SuiAddress,
        gas: ObjectID,
        original_id: ObjectID,
        upgrade_cap: ObjectID,
        project: SuiCompiledPackage,
    ) -> Result<ObjectID, MovyError> {
        if self.db.get_object(&original_id).is_none() {
            return Err(eyre!("the package {} being upgraded is missing", original_id).into());
        }

        let Some(cap) = self.db.get_object(&upgrade_cap) else {
            return Err(eyre!("upgrade cap {} missing", upgrade_cap).into());
        };

        let package_id = project.package_id;
        let (modules, dependencies) = project.into_deployment();
        if let Some(m) = modules
            .iter()
            .find(|v| ObjectID::from(*v.address()) != ObjectID::ZERO)
        {
            return Err(eyre!("can not upgrade modules with non-zero id: {}", m.address()).into());
        }

        let mut modules_bytes = vec![];
        for module in &modules {
            let mut buf = vec![];
            module.serialize_with_version(module.version, &mut buf)?;
            modules_bytes.push(buf);
        }

        let digest = MovePackage::compute_digest_for_modules_and_deps(
            modules_bytes.iter(),
            dependencies.iter(),
            true,
        )
        .to_vec();

        let target = if package_id != ObjectID::ZERO {
            Some(package_id)
        } else {
            None
        };

        let mut builder = ProgrammableTransactionBuilder::new();

        let policy = builder.pure(UpgradePolicy::Compatible as u8)?;
        let digest = builder.pure(digest)?;
        let cap = builder.obj(ObjectArg::ImmOrOwnedObject(cap.compute_object_reference()))?;
        let upgrade_ticket = builder.programmable_move_call(
            MoveAddress::two().into(),
            Identifier::from_str("package").unwrap(),
            Identifier::from_str("authorize_upgrade").unwrap(),
            vec![],
            vec![cap, policy, digest],
        );
        let upgrade_receipt =
            builder.upgrade(original_id, upgrade_ticket, dependencies, modules_bytes);
        builder.programmable_move_call(
            MoveAddress::two().into(),
            Identifier::from_str("package").unwrap(),
            Identifier::from_str("commit_upgrade").unwrap(),
            vec![],
            vec![cap, upgrade_receipt],
        );
        let ptb = builder.finish();

        let out = self.run_ptb_with_gas_inner::<NopTracer>(
            ptb, epoch, epoch_ms, deployer, gas, None, target,
        )?;
        let ExecutionResults { effects, store, .. } = out.results;
        // look for new objects
        let mut new_package = None;
        debug!(
            "all changed: {:?}, status is {:?}",
            effects.all_changed_objects(),
            effects.status()
        );
        for t in effects.all_changed_objects() {
            if matches!(&t.2, WriteKind::Create) && matches!(&t.1, Owner::Immutable) {
                let object = store.written.get(&t.0.0).unwrap();
                if object.is_package() {
                    new_package = Some(t.0);
                }
            }
        }
        if let Some(new_package) = new_package {
            if let Some(target) = target
                && new_package.0 != target
            {
                return Err(eyre!(
                    "failed to deployed at target {} but at {}",
                    new_package.0,
                    target
                )
                .into());
            }
            self.db.commit_store(store, &effects)?;
            Ok(new_package.0)
        } else {
            Err(eyre!("fail to upgrade with {:?}", effects.status()).into())
        }
    }

    pub fn deploy_contract(
        &mut self,
        epoch: u64,
        epoch_ms: u64,
        admin: SuiAddress,
        gas: ObjectID,
        project: SuiCompiledPackage,
    ) -> Result<(ObjectID, ObjectID), MovyError> {
        let package_id = project.package_id;
        let (modules, dependencies) = project.into_deployment();

        debug!(
            "Deploying package with original id {} and dependencies {:?}, modules are [{}]",
            package_id,
            dependencies,
            modules
                .iter()
                .map(|v| {
                    let id = v.self_id();
                    format!("{}:{}", id.address(), id.name())
                })
                .join(",")
        );

        if let Some(m) = modules
            .iter()
            .find(|v| ObjectID::from(*v.address()) != ObjectID::ZERO)
        {
            return Err(eyre!("can not deploy modules with non-zero id: {}", m.address()).into());
        }

        let mut modules_bytes = vec![];
        for module in &modules {
            let mut buf = vec![];
            module.serialize_with_version(module.version, &mut buf)?;
            modules_bytes.push(buf);
        }

        let target = if package_id != ObjectID::ZERO {
            Some(package_id)
        } else {
            None
        };

        let ptb = ProgrammableTransaction {
            inputs: vec![CallArg::Pure(bcs::to_bytes(&admin)?)],
            commands: vec![
                Command::Publish(modules_bytes, dependencies.clone()),
                Command::TransferObjects(vec![Argument::Result(0)], Argument::Input(0)),
            ],
        };

        let out = self
            .run_ptb_with_gas_inner::<NopTracer>(ptb, epoch, epoch_ms, admin, gas, None, target)?;
        let ExecutionResults { effects, store, .. } = out.results;
        // look for new objects
        let mut new_object = None;
        let mut upgrade_cap = None;
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

            if matches!(&t.2, WriteKind::Create) && matches!(&t.1, Owner::AddressOwner(_admin)) {
                let object = store.written.get(&t.0.0).unwrap();
                let is_cap = object
                    .type_()
                    .map(|ty| ty.is_upgrade_cap())
                    .unwrap_or_default();
                if is_cap {
                    upgrade_cap = Some(t.0);
                }
            }
        }
        if let Some(new_object) = new_object {
            if let Some(target) = target
                && new_object.0 != target
            {
                return Err(eyre!(
                    "failed to deployed at target {} but at {}",
                    new_object.0,
                    target
                )
                .into());
            }
            if let Some(cap) = upgrade_cap {
                debug!(
                    "Contract deployed at {}, original id: {}, cap: {}",
                    new_object.0, package_id, cap.0
                );

                self.db.commit_store(store, &effects)?;
                Ok((new_object.0, cap.0))
            } else {
                Err(eyre!(
                    "no upgrade cap is produced for {}, which shall not happen",
                    new_object.0
                )
                .into())
            }
        } else {
            Err(eyre!("fail to deploy with {:?}", effects.status()).into())
        }
    }
}

static TARGET_DEPLOYMENT: LazyLock<Mutex<BTreeMap<TransactionDigest, ObjectID>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

struct GlobalDeployment {
    digest: TransactionDigest,
}

impl GlobalDeployment {
    pub fn new(tx: &mut TransactionData, target: ObjectID) -> Self {
        loop {
            let digest = tx.digest();
            let mut _lock = TARGET_DEPLOYMENT.lock().unwrap();
            if let std::collections::btree_map::Entry::Vacant(e) = _lock.entry(digest) {
                tracing::debug!("Deployment digest {} => {}", digest, target);
                e.insert(target);
                return Self { digest };
            } else {
                tx.gas_data_mut().budget += 1;
                continue;
            }
        }
    }
}

impl Drop for GlobalDeployment {
    fn drop(&mut self) {
        TARGET_DEPLOYMENT.lock().unwrap().remove(&self.digest);
    }
}

struct SuiFuzzMode;

impl ExecutionMode for SuiFuzzMode {
    type ArgumentUpdates = <Normal as ExecutionMode>::ArgumentUpdates;
    type ExecutionResults = <Normal as ExecutionMode>::ExecutionResults;
    const TRACK_EXECUTION: bool = Normal::TRACK_EXECUTION;

    fn add_argument_update(
        resolver: &impl sui_adapter_latest::type_resolver::TypeTagResolver,
        acc: &mut Self::ArgumentUpdates,
        arg: Argument,
        new_value: &sui_adapter_latest::execution_value::Value,
    ) -> Result<(), sui_types::error::ExecutionError> {
        Normal::add_argument_update(resolver, acc, arg, new_value)
    }
    fn add_argument_update_v2(
        acc: &mut Self::ArgumentUpdates,
        arg: Argument,
        bytes: Vec<u8>,
        type_: TypeTag,
    ) -> Result<(), sui_types::error::ExecutionError> {
        Normal::add_argument_update_v2(acc, arg, bytes, type_)
    }

    fn allow_arbitrary_function_calls() -> bool {
        Normal::allow_arbitrary_function_calls()
    }
    fn allow_arbitrary_values() -> bool {
        Normal::allow_arbitrary_values()
    }
    fn empty_arguments() -> Self::ArgumentUpdates {
        Normal::empty_arguments()
    }
    fn empty_results() -> Self::ExecutionResults {
        Normal::empty_results()
    }
    fn finish_command(
        resolver: &impl sui_adapter_latest::type_resolver::TypeTagResolver,
        acc: &mut Self::ExecutionResults,
        argument_updates: Self::ArgumentUpdates,
        command_result: &[sui_adapter_latest::execution_value::Value],
    ) -> Result<(), sui_types::error::ExecutionError> {
        Normal::finish_command(resolver, acc, argument_updates, command_result)
    }
    fn finish_command_v2(
        acc: &mut Self::ExecutionResults,
        argument_updates: Vec<(Argument, Vec<u8>, TypeTag)>,
        command_result: Vec<(Vec<u8>, TypeTag)>,
    ) -> Result<(), sui_types::error::ExecutionError> {
        Normal::finish_command_v2(acc, argument_updates, command_result)
    }
    fn targeted_deployment(digest: &TransactionDigest) -> Option<ObjectID> {
        tracing::debug!("Looking for targeted deployment for {}", digest);
        TARGET_DEPLOYMENT.lock().unwrap().get(digest).copied()
    }
    fn packages_are_predefined() -> bool {
        Normal::packages_are_predefined()
    }
    fn skip_conservation_checks() -> bool {
        Normal::skip_conservation_checks()
    }
}
