use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    str::FromStr,
};

use color_eyre::eyre::eyre;
use itertools::Itertools;
use movy_sui::{
    compile::{SuiCompiledPackage, build_package_resolved},
    database::cache::ObjectSuiStoreCommit,
    rpc::graphql::{GraphQlClient, OwnerKind},
};
use movy_types::{
    abi::{MOVY_INIT, MovePackageAbi},
    error::MovyError,
    input::{MoveAddress, MoveStructTag},
};
use sui_types::{
    Identifier,
    base_types::{ObjectID, SequenceNumber},
    digests::TransactionDigest,
    effects::TransactionEffectsAPI,
    object::Object,
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    storage::{BackingPackageStore, BackingStore, ObjectStore},
};

use crate::{
    db::{ObjectStoreCachedStore, ObjectStoreInfo},
    exec::SuiExecutor,
    tracer::NopTracer,
};

pub struct SuiTestingEnv<T> {
    db: T,
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

impl<
    T: ObjectStoreCachedStore
        + ObjectStoreInfo
        + ObjectStore
        + ObjectSuiStoreCommit
        + BackingStore
        + BackingPackageStore,
> SuiTestingEnv<T>
{
    pub fn new(db: T) -> Self {
        Self { db }
    }

    pub fn install_std(&self, test: bool) -> Result<(), MovyError> {
        // This is pretty hacky but works
        let hello_std = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("hello_std");
        let (_, resolved) = build_package_resolved(&hello_std, test)?;
        let flag = if test { "testing" } else { "non-testing" };
        for package in resolved.package_table.values() {
            log::info!(
                "Building {} std at {}",
                flag,
                package.package_path.display()
            );
            let out =
                SuiCompiledPackage::build_all_unpublished_from_folder(&package.package_path, test)?;
            let out = out.movy_mock()?;
            if out.package_id != ObjectID::ZERO {
                log::info!("Committing {} std {}", flag, out.package_id);
                log::debug!(
                    "Modules are {}",
                    out.all_modules_iter()
                        .map(|v| v.self_id().name().to_string())
                        .join(",")
                );
                let (modules, dependencies) = out.into_deployment();
                let move_package = Object::new_system_package(
                    &modules,
                    SequenceNumber::new(),
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

    pub async fn load_local(
        &self,
        path: &Path,
        deployer: MoveAddress,
        attacker: MoveAddress,
        epoch: u64,
        epoch_ms: u64,
        gas: ObjectID,
    ) -> Result<(MoveAddress, MovePackageAbi, MovePackageAbi, Vec<String>), MovyError> {
        log::info!("Compiling {} with non-test mode...", path.display());
        let abi_result = SuiCompiledPackage::build_all_unpublished_from_folder(path, false)?;
        let package_names = abi_result.package_names.clone();
        let mut non_test_abi = abi_result.abi()?;
        log::info!("Compiling {} with test mode...", path.display());
        let compiled_result = SuiCompiledPackage::build_all_unpublished_from_folder(path, true)?;
        let compiled_result = compiled_result.movy_mock()?;
        log::debug!(
            "test modules are {}",
            compiled_result
                .test_modules()
                .iter()
                .map(|v| v.self_id().name().to_string())
                .join(", ")
        );
        let mut executor = SuiExecutor::new(&self.db)?;
        let address =
            executor.deploy_contract(epoch, epoch_ms, deployer.into(), gas, compiled_result)?;

        // In search of any deploy functions
        let mut abi = self.db.get_package_info(address.into())?.unwrap();

        for md in abi.modules.iter() {
            if md.is_test_only_module()
                && let Some(init) = md.locate_movy_init()
            {
                let mut builder = ProgrammableTransactionBuilder::new();
                let deployer_arg = builder.pure(ObjectID::from(deployer))?;
                let attacker_arg = builder.pure(ObjectID::from(attacker))?;
                builder.programmable_move_call(
                    address,
                    Identifier::from_str(&md.module_id.module_name).unwrap(),
                    Identifier::from_str(&init.name).unwrap(),
                    vec![],
                    vec![deployer_arg, attacker_arg],
                );
                let ptb = builder.finish();
                log::info!("Detected a {} at: {}", MOVY_INIT, md.module_id);
                let results = executor.run_ptb_with_gas::<NopTracer>(
                    ptb,
                    epoch,
                    epoch_ms,
                    deployer.into(),
                    gas,
                    None,
                )?;
                log::info!("Commiting movy_init effects...");
                log::debug!(
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
                    for object in objects.iter() {
                        self.db.load_object(object.id().into()).await?;
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

    pub async fn deploy_address(&self, package_id: MoveAddress) -> Result<(), MovyError> {
        let Some(package_object) = self.db.get_object(&package_id.into()) else {
            return Err(eyre!("Package object not found: {}", package_id).into());
        };

        // Analyze package dependencies
        let pkg = package_object
            .data
            .try_as_package()
            .ok_or_else(|| eyre!("Expected package data"))?;
        for upgrade_info in pkg.linkage_table().values() {
            self.db.load_object(upgrade_info.upgraded_id.into()).await?;
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
