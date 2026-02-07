use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::Deref,
    str::FromStr,
    sync::Arc,
};

use color_eyre::eyre::eyre;
use itertools::Itertools;
use log::{debug, trace, warn};
use move_core_types::account_address::AccountAddress;
use move_trace_format::{format::MoveTraceBuilder, interface::Tracer};
use movy_sui::{compile::SuiCompiledPackage, database::cache::ObjectSuiStoreCommit};
use movy_types::{error::MovyError, object::MoveOwner};
use sui_types::{
    TypeTag,
    base_types::{ObjectID, SuiAddress},
    committee::ProtocolVersion,
    digests::TransactionDigest,
    effects::{TransactionEffects, TransactionEffectsAPI},
    gas::SuiGasStatus,
    inner_temporary_store::InnerTemporaryStore,
    metrics::LimitsMetrics,
    move_package::MovePackage,
    object::{Object, Owner},
    storage::{BackingPackageStore, BackingStore, ObjectStore, WriteKind},
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
    T: ObjectStore
        + BackingStore
        + BackingPackageStore
        + ObjectSuiStoreCommit
        + ObjectStoreMintObject
        + ObjectStoreInfo,
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
        let (store, gas_status, effects, _timing, result) =
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
        let tx_data = TransactionData::new(tx_kind, sender, gas, 100_000_000_000, 1);

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

    fn load_dependency_package(&self, dep: &ObjectID) -> Result<MovePackage, MovyError> {
        let mut obj = self.db.get_object(dep);
        if obj.is_none() {
            if let Ok(Some(pkg)) = self.db.get_package_object(dep) {
                obj = Some(pkg.into());
            }
        }
        let Some(object) = obj else {
            return Err(eyre!("package {} not found", dep).into());
        };
        let Some(pkg) = object.data.try_as_package() else {
            return Err(eyre!("object {} is not a package", dep).into());
        };
        Ok(pkg.clone())
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
        let mut all_deps: BTreeSet<ObjectID> = dependencies.iter().copied().collect();
        let mut queue: VecDeque<ObjectID> = dependencies.iter().copied().collect();
        while let Some(dep) = queue.pop_front() {
            let Some(object) = self.db.get_object(&dep).or_else(|| {
                self.db
                    .get_package_object(&dep)
                    .ok()
                    .flatten()
                    .map(Into::into)
            }) else {
                continue;
            };
            let Some(pkg) = object.data.try_as_package() else {
                continue;
            };
            for info in pkg.linkage_table().values() {
                let id = info.upgraded_id;
                if all_deps.insert(id) {
                    queue.push_back(id);
                }
            }
        }

        let mut canonical_by_original: BTreeMap<ObjectID, (ObjectID, u64)> = BTreeMap::new();
        for dep in all_deps.iter() {
            if let Some(object) = self.db.get_object(dep).or_else(|| {
                self.db
                    .get_package_object(dep)
                    .ok()
                    .flatten()
                    .map(Into::into)
            }) {
                if let Some(pkg) = object.data.try_as_package() {
                    let original_id = pkg.original_package_id();
                    let (upgraded_id, upgraded_version) =
                        if let Some(info) = pkg.linkage_table().get(&original_id) {
                            (info.upgraded_id, info.upgraded_version.value())
                        } else {
                            (*dep, object.version().value())
                        };
                    let entry = canonical_by_original
                        .entry(original_id)
                        .or_insert((upgraded_id, upgraded_version));
                    if upgraded_version > entry.1 {
                        *entry = (upgraded_id, upgraded_version);
                    }
                } else {
                    canonical_by_original
                        .entry(*dep)
                        .or_insert((*dep, object.version().value()));
                }
            } else {
                canonical_by_original.entry(*dep).or_insert((*dep, 0));
            }
        }
        let canonical_deps: Vec<ObjectID> = canonical_by_original.values().map(|v| v.0).collect();
        debug!(
            "Publish deps list (normalized): {}",
            canonical_deps.iter().map(|v| v.to_string()).join(", ")
        );

        let mut modules_bytes = vec![];
        for module in &modules {
            let mut buf = vec![];
            module.serialize_with_version(module.version, &mut buf)?;
            modules_bytes.push(buf);
        }

        let ptb = ProgrammableTransaction {
            inputs: vec![CallArg::Pure(bcs::to_bytes(&admin)?)],
            commands: vec![
                Command::Publish(modules_bytes, canonical_deps.clone()), // This produces an upgrade cap
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

    pub fn force_deploy_contract_at(
        &mut self,
        package_id: ObjectID,
        project: SuiCompiledPackage,
    ) -> Result<ObjectID, MovyError> {
        log::info!("force publish package at {}", package_id);
        let (modules, dependencies) = project.into_deployment();
        let mut dep_packages = Vec::new();
        for dep in dependencies.iter() {
            if *dep == package_id {
                continue;
            }
            dep_packages.push(self.load_dependency_package(dep)?);
        }
        let object = Object::new_package(
            &modules,
            TransactionDigest::genesis_marker(),
            &self.protocol_config,
            dep_packages.iter(),
        )?;
        if object.id() != package_id {
            return Err(eyre!(
                "forced publish id mismatch: expected {}, got {}",
                package_id,
                object.id()
            )
            .into());
        }
        log::info!("force publish success at {}", package_id);
        self.db.commit_single_object(object)?;
        Ok(package_id)
    }

    /// Force redeploy an *upgraded* package at an existing storage ID.
    ///
    /// This is used when the package storage ID differs from the runtime/original ID embedded in
    /// module bytes (Sui upgrade semantics). We reuse the currently stored package as the "previous"
    /// version and bump the package version in-place to keep linkage resolution working.
    pub fn force_redeploy_upgraded_contract_at(
        &mut self,
        storage_id: ObjectID,
        project: SuiCompiledPackage,
    ) -> Result<ObjectID, MovyError> {
        log::info!("force redeploy upgraded package at {}", storage_id);

        let prev_obj = self
            .db
            .get_object(&storage_id)
            .or_else(|| {
                self.db
                    .get_package_object(&storage_id)
                    .ok()
                    .flatten()
                    .map(Into::into)
            })
            .ok_or_else(|| eyre!("previous package {} not found", storage_id))?;
        let prev_pkg = prev_obj
            .data
            .try_as_package()
            .ok_or_else(|| eyre!("object {} is not a package", storage_id))?
            .clone();
        log::debug!(
            "previous package storage_id={} version={} original_id={}",
            storage_id,
            prev_pkg.version().value(),
            prev_pkg.original_package_id()
        );

        let (modules, dependencies) = project.into_deployment();
        let runtime_id = modules
            .first()
            .map(|m| ObjectID::from(*m.address()))
            .unwrap_or(ObjectID::ZERO);
        log::debug!(
            "redeploy upgraded package storage_id={} new_runtime_id={} modules={} deps={}",
            storage_id,
            runtime_id,
            modules.len(),
            dependencies
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );

        let mut dep_packages = Vec::new();
        for dep in dependencies.iter() {
            if *dep == storage_id {
                continue;
            }
            dep_packages.push(self.load_dependency_package(dep)?);
        }

        let object = Object::new_upgraded_package(
            &prev_pkg,
            storage_id,
            &modules,
            TransactionDigest::genesis_marker(),
            &self.protocol_config,
            dep_packages.iter(),
        )?;

        if object.id() != storage_id {
            return Err(eyre!(
                "forced redeploy id mismatch: expected {}, got {}",
                storage_id,
                object.id()
            )
            .into());
        }

        if let Some(pkg) = object.data.try_as_package() {
            log::debug!(
                "redeploy result storage_id={} version={} original_id={}",
                storage_id,
                pkg.version().value(),
                pkg.original_package_id()
            );
        }
        self.db.commit_single_object(object)?;
        Ok(storage_id)
    }
}
