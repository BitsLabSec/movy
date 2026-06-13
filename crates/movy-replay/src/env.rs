use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    path::Path,
    str::FromStr,
};

use color_eyre::eyre::eyre;
use itertools::Itertools;
use move_core_types::account_address::AccountAddress;
use movy_sui::{
    compile::{SuiCompiledPackage, mock_module_address},
    database::{cache::ObjectSuiStoreCommit, graphql::GraphQlDatabase},
    rpc::graphql::{GraphQlClient, OwnerKind},
};
use movy_types::{
    abi::{MOVY_INIT, MoveAbiSignatureToken, MoveFunctionAbi, MovePackageAbi},
    error::MovyError,
    input::{MoveAddress, MoveStructTag, MoveTypeTag},
    object::{MoveObjectInfo, MoveOwner},
};
use sui_types::{
    Identifier,
    base_types::{ObjectID, SequenceNumber},
    digests::TransactionDigest,
    effects::TransactionEffectsAPI,
    execution_status::ExecutionStatus,
    move_package::MovePackage,
    object::{Data, Object},
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    storage::{BackingPackageStore, BackingStore, ObjectStore},
    transaction::{ObjectArg, SharedObjectMutability},
};

use crate::{
    db::{ObjectStoreCachedStore, ObjectStoreInfo},
    exec::SuiExecutor,
    tracer::{SelectiveTracer, TeeTracer, lcov::LineCoverageCollector, tree::TreeTracer},
};

pub struct SuiTestingEnv<T> {
    db: T,
}

fn format_movy_init_failure(status: &ExecutionStatus, trace: Option<&str>) -> String {
    let mut out = String::new();
    match status {
        ExecutionStatus::Success => out.push_str("status: success"),
        ExecutionStatus::Failure { error, command } => {
            if let Some(command) = command {
                let _ = writeln!(out, "command: {command}");
            }
            let _ = write!(out, "error: {error}");
        }
    }
    if let Some(trace) = trace.filter(|trace| !trace.trim().is_empty()) {
        let _ = write!(out, "\n{trace}");
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MovyInitObjectMode {
    OwnedValue,
    ImmutableRef,
    MutableRef,
}

fn movy_init_param_ty_and_mode(
    param: &MoveAbiSignatureToken,
) -> Option<(MoveTypeTag, MovyInitObjectMode)> {
    match param {
        MoveAbiSignatureToken::Struct { .. } | MoveAbiSignatureToken::StructInstantiation(_, _) => {
            Some((
                param.subst(&BTreeMap::new())?,
                MovyInitObjectMode::OwnedValue,
            ))
        }
        MoveAbiSignatureToken::Reference(inner) => Some((
            inner.subst(&BTreeMap::new())?,
            MovyInitObjectMode::ImmutableRef,
        )),
        MoveAbiSignatureToken::MutableReference(inner) => Some((
            inner.subst(&BTreeMap::new())?,
            MovyInitObjectMode::MutableRef,
        )),
        _ => None,
    }
}

fn movy_init_object_arg_for_param(
    param: &MoveAbiSignatureToken,
    deployer: MoveAddress,
    objects: &[MoveObjectInfo],
    used_ids: &BTreeSet<MoveAddress>,
) -> Result<(MoveObjectInfo, ObjectArg), MovyError> {
    let Some((expected_ty, mode)) = movy_init_param_ty_and_mode(param) else {
        return Err(eyre!("unsupported movy_init parameter type: {}", param).into());
    };

    let object = objects
        .iter()
        .find(|object| {
            if used_ids.contains(&object.id) || object.ty != expected_ty {
                return false;
            }
            match (&object.owner, mode) {
                (MoveOwner::AddressOwner(owner), _) => *owner == deployer,
                (MoveOwner::Immutable, MovyInitObjectMode::ImmutableRef) => true,
                (MoveOwner::Shared { .. }, MovyInitObjectMode::ImmutableRef) => true,
                (MoveOwner::Shared { .. }, MovyInitObjectMode::MutableRef) => true,
                _ => false,
            }
        })
        .cloned()
        .ok_or_else(|| {
            eyre!(
                "unable to find a DB object for movy_init parameter {}",
                param
            )
        })?;

    let object_arg = match (&object.owner, mode) {
        (MoveOwner::AddressOwner(_), _)
        | (MoveOwner::Immutable, MovyInitObjectMode::ImmutableRef) => {
            ObjectArg::ImmOrOwnedObject(object.sui_reference())
        }
        (
            MoveOwner::Shared {
                initial_shared_version,
            },
            MovyInitObjectMode::ImmutableRef,
        ) => ObjectArg::SharedObject {
            id: object.id.into(),
            initial_shared_version: (*initial_shared_version).into(),
            mutability: SharedObjectMutability::Immutable,
        },
        (
            MoveOwner::Shared {
                initial_shared_version,
            },
            MovyInitObjectMode::MutableRef,
        ) => ObjectArg::SharedObject {
            id: object.id.into(),
            initial_shared_version: (*initial_shared_version).into(),
            mutability: SharedObjectMutability::Mutable,
        },
        _ => {
            return Err(eyre!(
                "unsupported owner {:?} for movy_init parameter {}",
                object.owner,
                param
            )
            .into());
        }
    };

    Ok((object, object_arg))
}

impl<T> SuiTestingEnv<T> {
    pub fn inner(&self) -> &T {
        &self.db
    }

    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.db
    }
    pub fn into_inner(self) -> T {
        self.db
    }
}

#[cfg(test)]
mod tests {
    use movy_types::{
        abi::{MoveAbility, MoveModuleId, MoveStructHandle},
        input::MoveStructTag,
    };
    use sui_types::{digests::ObjectDigest, transaction::SharedObjectMutability};

    use super::*;

    fn test_struct_handle(address: MoveAddress, module: &str, name: &str) -> MoveStructHandle {
        MoveStructHandle {
            module_id: MoveModuleId {
                module_address: address,
                module_name: module.to_string(),
            },
            struct_name: name.to_string(),
            abilities: MoveAbility::empty(),
            type_parameters: vec![],
        }
    }

    fn test_struct_tag(address: MoveAddress, module: &str, name: &str) -> MoveTypeTag {
        MoveTypeTag::Struct(MoveStructTag {
            address,
            module: module.to_string(),
            name: name.to_string(),
            tys: vec![],
        })
    }

    fn test_object_info(id: MoveAddress, ty: MoveTypeTag, owner: MoveOwner) -> MoveObjectInfo {
        MoveObjectInfo {
            id,
            ty,
            owner,
            version: 7,
            digest: ObjectDigest::random().into(),
        }
    }

    #[test]
    fn selects_shared_mutable_ref_for_movy_init() {
        let deployer = MoveAddress::random();
        let handle = test_struct_handle(MoveAddress::random(), "vault_config", "GlobalConfig");
        let param = MoveAbiSignatureToken::MutableReference(Box::new(
            MoveAbiSignatureToken::Struct(handle.clone()),
        ));
        let object = test_object_info(
            MoveAddress::random(),
            test_struct_tag(
                handle.module_id.module_address,
                &handle.module_id.module_name,
                &handle.struct_name,
            ),
            MoveOwner::Shared {
                initial_shared_version: 11,
            },
        );

        let (_object, arg) =
            movy_init_object_arg_for_param(&param, deployer, &[object.clone()], &BTreeSet::new())
                .unwrap();

        assert_eq!(
            arg,
            ObjectArg::SharedObject {
                id: object.id.into(),
                initial_shared_version: 11.into(),
                mutability: SharedObjectMutability::Mutable,
            }
        );
    }

    #[test]
    fn selects_deployer_owned_value_for_movy_init() {
        let deployer = MoveAddress::random();
        let handle = test_struct_handle(MoveAddress::random(), "gauge_cap", "CreateCap");
        let param = MoveAbiSignatureToken::Struct(handle.clone());
        let object = test_object_info(
            MoveAddress::random(),
            test_struct_tag(
                handle.module_id.module_address,
                &handle.module_id.module_name,
                &handle.struct_name,
            ),
            MoveOwner::AddressOwner(deployer),
        );

        let (_object, arg) =
            movy_init_object_arg_for_param(&param, deployer, &[object.clone()], &BTreeSet::new())
                .unwrap();

        assert_eq!(arg, ObjectArg::ImmOrOwnedObject(object.sui_reference()));
    }

    #[test]
    fn rejects_immutable_object_for_mutable_ref() {
        let deployer = MoveAddress::random();
        let handle = test_struct_handle(MoveAddress::random(), "config", "GlobalConfig");
        let param = MoveAbiSignatureToken::MutableReference(Box::new(
            MoveAbiSignatureToken::Struct(handle.clone()),
        ));
        let object = test_object_info(
            MoveAddress::random(),
            test_struct_tag(
                handle.module_id.module_address,
                &handle.module_id.module_name,
                &handle.struct_name,
            ),
            MoveOwner::Immutable,
        );

        let err = movy_init_object_arg_for_param(&param, deployer, &[object], &BTreeSet::new())
            .unwrap_err();
        assert!(err.to_string().contains("unable to find a DB object"));
    }
}

impl<
    T: ObjectStoreCachedStore
        + ObjectStoreInfo
        + ObjectStore
        + ObjectSuiStoreCommit
        + BackingStore
        + BackingPackageStore
        + Clone
        + 'static,
> SuiTestingEnv<T>
{
    pub fn new(db: T) -> Self {
        Self { db }
    }

    async fn build_movy_init_args(
        &self,
        builder: &mut ProgrammableTransactionBuilder,
        init: &MoveFunctionAbi,
        deployer: MoveAddress,
        attacker: MoveAddress,
    ) -> Result<Vec<sui_types::transaction::Argument>, MovyError> {
        let objects = self
            .db
            .list_objects()
            .await?
            .into_iter()
            .filter_map(|id| self.db.get_move_object_info(id).ok())
            .collect::<Vec<_>>();
        let mut used_ids = BTreeSet::new();
        let mut args = Vec::with_capacity(init.parameters.len());

        for (idx, param) in init.parameters.iter().enumerate() {
            match idx {
                0 => args.push(builder.pure(ObjectID::from(deployer))?),
                1 => args.push(builder.pure(ObjectID::from(attacker))?),
                _ => {
                    let (object, object_arg) =
                        movy_init_object_arg_for_param(param, deployer, &objects, &used_ids)?;
                    used_ids.insert(object.id);
                    args.push(builder.obj(object_arg)?);
                }
            }
        }

        Ok(args)
    }

    pub fn install_movy(&self) -> Result<(), MovyError> {
        let movy = movy_sui_stds::movy();
        tracing::info!("Installing movy to {}", movy.package_id);
        let (modules, deps) = movy.into_deployment();
        let movy_package = Object::new_package_from_data(
            Data::Package(MovePackage::new_system(
                SequenceNumber::new(),
                &modules,
                deps,
            )),
            TransactionDigest::genesis_marker(),
        );
        self.db.commit_single_object(movy_package)?;
        Ok(())
    }

    pub fn install_std(&self, test: bool) -> Result<(), MovyError> {
        // This is pretty hacky but works
        let stds = if test {
            movy_sui_stds::testing_std()
        } else {
            movy_sui_stds::sui_std()
        };

        let flag = if test { "testing" } else { "non-testing" };
        for out in stds {
            let out = out.movy_mock()?;
            if out.package_id != ObjectID::ZERO {
                tracing::info!("Committing {} std {}", flag, out.package_id);
                tracing::debug!(
                    "Modules are {}",
                    out.all_modules_iter()
                        .map(|v| v.self_id().name().to_string())
                        .join(",")
                );
                // let std_onchain_version = self
                //     .db
                //     .get_object(&out.package_id)
                //     .ok_or_else(|| eyre!("{} not onchain?!", out.package_id))?
                //     .version();
                let (modules, dependencies) = out.into_deployment();
                let move_package = Object::new_system_package(
                    &modules,
                    SequenceNumber::from_u64(0xff),
                    dependencies,
                    TransactionDigest::genesis_marker(),
                );
                self.db.commit_single_object(move_package)?;
            }
        }

        Ok(())
    }

    pub fn install_non_testing_std(&self) -> Result<(), MovyError> {
        self.install_std(false)
    }

    pub fn mock_testing_std(&self) -> Result<(), MovyError> {
        self.install_std(true)
    }

    pub async fn fetch_package_at_address(
        &self,
        package_id: MoveAddress,
        rpc: &GraphQlDatabase,
    ) -> Result<BTreeSet<ObjectID>, MovyError> {
        let mut out = BTreeSet::new();
        if let Some(object) = rpc.get_object(package_id.into()).await? {
            tracing::info!(
                "Fetching package {}:{} from chain",
                package_id,
                object.version()
            );
            let pkg = object
                .data
                .try_as_package()
                .ok_or_else(|| eyre!("Expected package data for {}", object.id()))?;

            for (id, upgrade_info) in pkg.linkage_table() {
                if self.db.get_object(&upgrade_info.upgraded_id).is_none() {
                    tracing::info!(
                        "Fetching ugprade cap {}:{} from chain",
                        upgrade_info.upgraded_id,
                        upgrade_info.upgraded_version
                    );
                    self.deploy_object_id(upgrade_info.upgraded_id.into(), rpc)
                        .await?;
                } else {
                    tracing::debug!("Upgrade info {:?} already exists", upgrade_info);
                }
                out.insert(*id);
            }
            self.db.commit_single_object(object)?;
        } else {
            return Err(eyre!("package {} not found", package_id).into());
        }
        Ok(out)
    }

    pub async fn load_local(
        &self,
        path: &Path,
        deployer: MoveAddress,
        attacker: MoveAddress,
        epoch: u64,
        epoch_ms: u64,
        gas: ObjectID,
        unpublished: bool,
        verify_deps: bool,
        trace_movy_init: bool,
        onchain_fallback: bool,
        pinned_addresses: &BTreeMap<String, MoveAddress>,
        rpc: &GraphQlDatabase,
        lcov: Option<&LineCoverageCollector>,
        isolation: &movy_sui::compile::BuildIsolation,
    ) -> Result<(MoveAddress, MovePackageAbi, MovePackageAbi, Vec<String>), MovyError> {
        tracing::info!("Compiling {} with non-test mode...", path.display());
        // Same isolation for both passes — `extra_sources` (which carry
        // `#[test]` / `#[test_only]` code) only gets injected by the package
        // system in test mode, so the non-test pass safely ignores them.
        let abi_result =
            SuiCompiledPackage::build_checked(path, false, unpublished, verify_deps, isolation)?;
        let mut non_test_abi = abi_result.abi()?;
        tracing::info!("Compiled summary: {}", &abi_result);
        tracing::info!("Compiling {} with test mode...", path.display());
        let compiled_result =
            SuiCompiledPackage::build_checked(path, true, unpublished, verify_deps, isolation)?;
        tracing::info!("Compiled summary: {}", &compiled_result);

        let package_names = compiled_result.package_names.clone();
        let mut compiled_result = compiled_result.movy_mock()?;

        // Resolve a fixed deployment address by matching this package's names against --deploy-at.
        // A package deploys to a single id, so all matching names must agree on the address.
        let pinned_address = {
            let mut matched: Option<MoveAddress> = None;
            for name in package_names.iter() {
                if let Some(addr) = pinned_addresses.get(name) {
                    if let Some(prev) = matched
                        && prev != *addr
                    {
                        return Err(eyre!(
                            "conflicting --deploy-at addresses for package {:?}",
                            package_names
                        )
                        .into());
                    }
                    matched = Some(*addr);
                }
            }
            matched
        };

        // Deploy onchain deps or deps used by immediate dependencies
        if onchain_fallback {
            tracing::info!("Enabling onchain fallback...");
            let mut packages_to_fetch = abi_result
                .dependencies()
                .iter()
                .copied()
                .chain(abi_result.all_modules_iter().flat_map(|t| {
                    t.immediate_dependencies()
                        .into_iter()
                        .map(|im| (*im.address()).into())
                }))
                .collect::<BTreeSet<_>>();
            while let Some(dep) = packages_to_fetch.pop_last() {
                let dep = AccountAddress::from(dep);

                if dep != AccountAddress::ZERO
                    && dep != compiled_result.package_id.into()
                    && self.db.get_object(&dep.into()).is_none()
                {
                    tracing::info!(
                        "Dependency {} not found in our db for {}, trying to fetch it from onchain",
                        dep,
                        path.display()
                    );
                    match self.fetch_package_at_address(dep.into(), rpc).await {
                        Ok(nexts) => {
                            packages_to_fetch.extend(nexts.into_iter());
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Fail to add the object {} due to {}, this might be fine though.",
                                dep,
                                e
                            );
                        }
                    }
                }
            }
        }

        let mut executor = SuiExecutor::new(self.db.clone())?;

        // Now we need to understand if we are deploying or upgrading
        let Some(module_address) = compiled_result.all_same_address() else {
            return Err(eyre!("{} modules' addresses not all same", compiled_result).into());
        };

        for it in compiled_result.modules_mut().iter_mut() {
            mock_module_address(ObjectID::ZERO, it);
        }

        // When a fixed deployment address is requested, publish the package there instead of a
        // freshly derived id. This keeps movy_init-derived object ids stable across source edits,
        // since they descend from the (now fixed) package id rather than the publish tx digest.
        let module_address = if let Some(pinned) = pinned_address {
            tracing::info!("Pinning local package deployment to {}", pinned);
            compiled_result.package_id = pinned.into();
            ObjectID::ZERO
        } else {
            ObjectID::from(module_address)
        };
        let address = if module_address == ObjectID::ZERO
            || module_address == compiled_result.package_id
        {
            tracing::info!(
                "Doing a normal deployment to {}...",
                compiled_result.package_id
            );
            let (address, _) =
                executor.deploy_contract(epoch, epoch_ms, deployer.into(), gas, compiled_result)?;
            address
        } else {
            tracing::info!(
                "Modules base address is {} but the package is defined as {}, we will do a deploy and then an upgrade operation",
                module_address,
                compiled_result.package_id
            );

            let mut original_deployment = compiled_result.clone();
            original_deployment.package_id = module_address;
            let (address, cap) = executor.deploy_contract(
                epoch,
                epoch_ms,
                deployer.into(),
                gas,
                original_deployment,
            )?;

            tracing::info!(
                "We have deployed the project to its original id {} with cap {}",
                address,
                cap
            );

            let address = executor.upgrade_contract(
                epoch,
                epoch_ms,
                deployer.into(),
                gas,
                address,
                cap,
                compiled_result,
            )?;

            tracing::info!("We have upgraded to package address {}", address);

            address
        };
        // In search of any deploy functions
        let mut abi = self.db.get_package_info(address.into())?.unwrap();

        for md in abi.modules.iter() {
            if md.is_test_only_module()
                && let Some(init) = md.locate_movy_init()
            {
                let mut builder = ProgrammableTransactionBuilder::new();
                let args = self
                    .build_movy_init_args(&mut builder, init, deployer, attacker)
                    .await?;
                builder.programmable_move_call(
                    address,
                    Identifier::from_str(&md.module_id.module_name).unwrap(),
                    Identifier::from_str(&init.name).unwrap(),
                    vec![],
                    args,
                );
                let ptb = builder.finish();
                tracing::info!("Detected a {} at: {}", MOVY_INIT, md.module_id);
                // Always capture movy_init traces so failures surface detailed traces even when
                // verbose tracing is disabled.
                let tracer = if let Some(collector) = lcov {
                    SelectiveTracer::T1(TeeTracer(TreeTracer::new(), collector.tracer()))
                } else {
                    SelectiveTracer::T2(TreeTracer::new())
                };
                let mut results = executor.run_ptb_with_movy_tracer_gas(
                    ptb,
                    epoch,
                    epoch_ms,
                    deployer.into(),
                    gas,
                    Some(tracer),
                )?;
                let trace = std::mem::take(&mut results.tracer).map(|tracer| match tracer {
                    SelectiveTracer::T1(TeeTracer(tree, _)) => {
                        tree.take_inner().pprint_failure_views()
                    }
                    SelectiveTracer::T2(tree) => tree.take_inner().pprint_failure_views(),
                });
                if !results.effects.status().is_ok() {
                    let details =
                        format_movy_init_failure(results.effects.status(), trace.as_deref());
                    return Err(eyre!("movy_init reverts!\n{}", details).into());
                }
                if trace_movy_init {
                    tracing::trace!(
                        "movy_init trace:\n{}",
                        trace.unwrap_or_else(|| "-".to_string())
                    );
                }
                tracing::info!("Commiting movy_init effects...");
                tracing::debug!(
                    "Status: {:?} Changed Objects: {}, Removed Objects: {}",
                    results.effects.status(),
                    results
                        .effects
                        .all_changed_objects()
                        .iter()
                        .map(|t| format!("{:?}", t))
                        .join(","),
                    results
                        .effects
                        .all_removed_objects()
                        .iter()
                        .map(|t| format!("{:?}", t.0))
                        .join(",")
                );
                self.db
                    .commit_store(results.results.store, &results.results.effects)?;
            }
        }

        non_test_abi.published_at(address.into());
        abi.published_at(address.into());
        Ok((address.into(), abi, non_test_abi, package_names))
    }

    pub async fn export_abi(&self) -> Result<BTreeMap<MoveAddress, MovePackageAbi>, MovyError> {
        let objects = self.db.list_objects().await?;

        let mut out = BTreeMap::new();
        for obj in objects {
            if let Ok(Some(abi)) = self.db.get_package_info(obj) {
                // object is package
                out.insert(abi.package_id, abi);
            }
        }
        Ok(out)
    }

    pub async fn load_history(
        &self,
        package_id: MoveAddress,
        ckpt: u64,
        rpc: &GraphQlClient,
    ) -> Result<(), MovyError> {
        if let Some(package) = self.db.get_package_info(package_id)? {
            for module in &package.modules {
                for s in &module.structs {
                    let tag = s.module_id.to_canonical_string(true);
                    let objects = rpc
                        .filter_objects(ckpt, Some(OwnerKind::Shared), None, Some(tag))
                        .await?;
                    for object in objects.into_iter() {
                        self.db.commit_single_object(object)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn load_inner_types(&self) -> Result<(), MovyError> {
        // Analyze all object types in the store
        let objects = self.db.list_objects().await?;
        for obj in objects {
            if let Ok(mv) = self.db.get_move_object_info(obj) {
                let addresses = mv.ty.flat_addresses();
                for addr in addresses {
                    self.db.load_object(addr).await?
                }
            }
        }
        Ok(())
    }

    pub async fn deploy_object_id(
        &self,
        package_id: MoveAddress,
        rpc: &GraphQlDatabase,
    ) -> Result<(), MovyError> {
        if let Some(object) = rpc.get_object(package_id.into()).await? {
            self.db.commit_single_object(object)?;
        } else {
            return Err(eyre!("object {} not found", package_id).into());
        }

        Ok(())
    }

    pub async fn all_tys(&self) -> Result<BTreeSet<MoveStructTag>, MovyError> {
        let mut tags = BTreeSet::new();
        for obj in self.db.list_objects().await? {
            if let Ok(info) = self.db.get_move_object_info(obj) {
                for st in info.ty.flat_structs() {
                    tags.insert(st);
                }
            }
        }
        Ok(tags)
    }
}
