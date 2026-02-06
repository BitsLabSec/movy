use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
    str::FromStr,
};

use color_eyre::eyre::eyre;
use itertools::Itertools;
use move_binary_format::{CompiledModule, file_format_common::VERSION_6};
use movy_sui::{
    compile::SuiCompiledPackage,
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
    base_types::ObjectID,
    digests::TransactionDigest,
    effects::TransactionEffectsAPI,
    move_package::MovePackage,
    object::Object,
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    storage::{BackingPackageStore, BackingStore, ObjectStore},
    supported_protocol_versions::ProtocolConfig,
};

use crate::{
    db::{ObjectStoreCachedStore, ObjectStoreInfo},
    exec::SuiExecutor,
    tracer::NopTracer,
};

pub struct SuiTestingEnv<T> {
    db: T,
}

#[derive(Clone, Debug)]
pub struct PackageAddressOverride {
    /// The address used in type tags / module IDs (the "original package id" in Sui upgrade terms).
    /// If unset, `published_at` is used for both roles.
    pub original: Option<MoveAddress>,
    /// The storage package object ID (the "published-at" / upgraded package id).
    pub published_at: MoveAddress,
}

fn external_module_refs(
    modules: impl Iterator<Item = CompiledModule>,
) -> BTreeMap<ObjectID, BTreeSet<String>> {
    let mut out: BTreeMap<ObjectID, BTreeSet<String>> = BTreeMap::new();
    for module in modules {
        let self_idx = module.self_handle_idx();
        for (idx, h) in module.module_handles.iter().enumerate() {
            if (idx as u16) == self_idx.0 {
                continue;
            }
            let addr = ObjectID::from(*module.address_identifier_at(h.address));
            if addr == ObjectID::ZERO {
                continue;
            }
            let name = module.identifier_at(h.name).to_string();
            out.entry(addr).or_default().insert(name);
        }
    }
    out
}

fn compiled_module_map<'a>(
    modules: impl Iterator<Item = &'a CompiledModule>,
    protocol_config: &ProtocolConfig,
) -> Result<BTreeMap<String, Vec<u8>>, MovyError> {
    let mut map = BTreeMap::new();
    for module in modules {
        let mut bytes = vec![];
        let version = if protocol_config.move_binary_format_version() > VERSION_6 {
            module.version
        } else {
            VERSION_6
        };
        module.serialize_with_version(version, &mut bytes)?;
        map.insert(module.name().to_string(), bytes);
    }
    Ok(map)
}

fn package_modules_match(
    compiled: &SuiCompiledPackage,
    pkg: &MovePackage,
    protocol_config: &ProtocolConfig,
) -> Result<bool, MovyError> {
    let compiled_map = compiled_module_map(compiled.all_modules_iter(), protocol_config)?;
    Ok(&compiled_map == pkg.serialized_module_map())
}

fn record_zero_address_modules(
    module_addr_map: &mut BTreeMap<String, ObjectID>,
    modules: &[CompiledModule],
    package_addr: ObjectID,
) -> Result<(), MovyError> {
    for module in modules.iter() {
        let name = module.name().to_string();
        if let Some(prev) = module_addr_map.insert(name.clone(), package_addr)
            && prev != package_addr
        {
            return Err(eyre!(
                "duplicate zero-address module name {} mapped to both {} and {}",
                name,
                prev,
                package_addr
            )
            .into());
        }
    }
    Ok(())
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
        let stds = if test {
            include_bytes!(concat!(env!("OUT_DIR"), "/std.testing")).to_vec()
        } else {
            include_bytes!(concat!(env!("OUT_DIR"), "/std")).to_vec()
        };
        let stds: Vec<SuiCompiledPackage> = serde_json::from_slice(&stds)?;

        let flag = if test { "testing" } else { "non-testing" };
        for out in stds {
            let out = out.movy_mock()?;
            if out.package_id != ObjectID::ZERO {
                log::info!("Committing {} std {}", flag, out.package_id);
                log::debug!(
                    "Modules are {}",
                    out.all_modules_iter()
                        .map(|v| v.self_id().name().to_string())
                        .join(",")
                );
                let std_onchain_version = self
                    .db
                    .get_object(&out.package_id.into())
                    .ok_or_else(|| eyre!("{} not onchain?!", out.package_id))?
                    .version();
                let (modules, dependencies) = out.into_deployment();
                let move_package = Object::new_system_package(
                    &modules,
                    std_onchain_version,
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
        package_address_overrides: Option<&BTreeMap<String, PackageAddressOverride>>,
    ) -> Result<(MoveAddress, MovePackageAbi, MovePackageAbi, Vec<String>), MovyError> {
        log::info!("Compiling {} with non-test mode...", path.display());
        let mut abi_result = SuiCompiledPackage::build_all_unpublished_from_folder(path, false)?;
        log::info!("Compiling {} with test mode...", path.display());
        let mut compiled_result =
            SuiCompiledPackage::build_all_unpublished_from_folder(path, true)?;
        compiled_result.ensure_immediate_deps();
        let package_names = compiled_result.package_names.clone();
        let mut executor = SuiExecutor::new(&self.db)?;
        let expected_id = abi_result.package_id;
        let address = if expected_id != ObjectID::ZERO {
            log::info!(
                "published-at detected: {}. Checking local store for existing package...",
                expected_id
            );
            if let Some(object) = self.db.get_object(&expected_id) {
                log::info!(
                    "package {} exists in store, verifying modules...",
                    expected_id
                );
                let pkg = object
                    .data
                    .try_as_package()
                    .ok_or_else(|| eyre!("Expected package data"))?;
                if !package_modules_match(&abi_result, pkg, &executor.protocol_config)? {
                    log::warn!("package {} modules mismatch", expected_id);
                    return Err(eyre!("package {} modules mismatch", expected_id).into());
                }
                log::info!("package {} modules match; using as deps", expected_id);
                expected_id
            } else {
                log::info!(
                    "package {} not found. Forcing publish at address...",
                    expected_id
                );
                let compiled_result = compiled_result.movy_mock()?;
                log::debug!(
                    "test modules are {}",
                    compiled_result
                        .test_modules()
                        .iter()
                        .map(|v| v.self_id().name().to_string())
                        .join(", ")
                );
                executor.force_deploy_contract_at(expected_id, compiled_result)?
            }
        } else {
            log::debug!("published-at is not set; entering auto-publish + rewrite path");
            log::debug!(
                "unpublished_dep_order size={} deps={}",
                compiled_result.unpublished_dep_order().len(),
                compiled_result
                    .unpublished_dep_order()
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",")
            );
            log::debug!(
                "unpublished_dep_modules keys size={} keys={}",
                compiled_result.unpublished_dep_modules().len(),
                compiled_result
                    .unpublished_dep_modules()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",")
            );
            let mut original_to_storage: BTreeMap<ObjectID, ObjectID> = BTreeMap::new();
            let mut zero_module_addr_map: BTreeMap<String, ObjectID> = BTreeMap::new();
            if !compiled_result.unpublished_dep_order().is_empty() {
                // Best-effort: ensure any dependency packages referenced by a dep publish exist in the store.
                // Otherwise `deploy_contract` will fail with LINKER_ERROR ("Cannot find ModuleId ... in data cache").
                let mut ensured: BTreeSet<ObjectID> = BTreeSet::new();

                // Only zero-address packages require name-based rewriting.
                // Non-zero unpublished deps can be force-published at their compiled address,
                // which avoids ambiguity when different packages share module names.
                for dep_name in compiled_result.unpublished_dep_order().iter() {
                    log::debug!("dep publish begin {}", dep_name);
                    let Some(modules) = compiled_result.unpublished_dep_modules().get(dep_name)
                    else {
                        log::debug!(
                            "ERROR: missing modules for dep {} (available keys={})",
                            dep_name,
                            compiled_result
                                .unpublished_dep_modules()
                                .keys()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(",")
                        );
                        return Err(eyre!("missing modules for dep {}", dep_name).into());
                    };
                    if modules.is_empty() {
                        log::debug!("ERROR: empty modules for dep {}", dep_name);
                        return Err(eyre!("empty modules for dep {}", dep_name).into());
                    }

                    let dep_self_addr = ObjectID::from(*modules[0].address());
                    let dep_is_zero_addr = dep_self_addr == ObjectID::ZERO;

                    if let Some(ov) = package_address_overrides.and_then(|m| m.get(dep_name)) {
                        let original = ov.original.unwrap_or(ov.published_at);
                        let original_id = ObjectID::from(original);
                        let storage_id = ObjectID::from(ov.published_at);

                        let exists = self
                            .db
                            .get_package_object(&storage_id)
                            .ok()
                            .flatten()
                            .is_some()
                            || self.db.get_object(&storage_id).is_some();
                        if !exists {
                            return Err(eyre!(
                                "--package-address {}:{} not found in store; add it to --onchains first",
                                dep_name,
                                storage_id
                            )
                            .into());
                        }

                        log::info!(
                            "Using package {} original={} published-at={} (--package-address)",
                            dep_name,
                            original_id,
                            storage_id
                        );
                        original_to_storage.insert(original_id, storage_id);

                        if dep_is_zero_addr {
                            record_zero_address_modules(
                                &mut zero_module_addr_map,
                                modules,
                                original_id,
                            )?;
                        }
                        continue;
                    }

                    let mut dep_pkg =
                        SuiCompiledPackage::new_unpublished(dep_name.clone(), modules.clone());
                    if dep_is_zero_addr && !zero_module_addr_map.is_empty() {
                        if let Err(e) = dep_pkg.rewrite_deps_by_module_name(&zero_module_addr_map) {
                            log::debug!(
                                "ERROR: rewrite deps by module name failed for dep {}: {:?}",
                                dep_name,
                                e
                            );
                            return Err(e);
                        }
                    }
                    dep_pkg.ensure_immediate_deps();

                    // Ensure dependency packages exist for this dep before publishing it.
                    // This is especially important for zero-address deps that must use `deploy_contract`.
                    let mut queue: VecDeque<ObjectID> =
                        dep_pkg.dependencies().iter().copied().collect();
                    while let Some(addr) = queue.pop_front() {
                        if addr == ObjectID::ZERO {
                            continue;
                        }
                        if !ensured.insert(addr) {
                            continue;
                        }
                        let exists = self.db.get_package_object(&addr).ok().flatten().is_some()
                            || self.db.get_object(&addr).is_some();
                        if exists {
                            continue;
                        }
                        log::debug!(
                            "dep {} missing in store (required by {}); trying force publish from compiled deps...",
                            addr,
                            dep_name
                        );

                        let Some(candidates) = compiled_result.dep_modules_by_addr().get(&addr)
                        else {
                            log::debug!(
                                "no compiled dep modules recorded for addr {} (required by {})",
                                addr,
                                dep_name
                            );
                            continue;
                        };
                        if candidates.len() != 1 {
                            log::debug!(
                                "ambiguous dep candidates for addr {} (required by {}): {}",
                                addr,
                                dep_name,
                                candidates.keys().cloned().collect::<Vec<_>>().join(",")
                            );
                            continue;
                        }
                        let (cand_name, cand_modules) = candidates.iter().next().unwrap();
                        if cand_modules.is_empty() {
                            log::debug!(
                                "dep candidate {} for addr {} has 0 modules (required by {})",
                                cand_name,
                                addr,
                                dep_name
                            );
                            continue;
                        }

                        let mut cand_pkg = SuiCompiledPackage::new_unpublished(
                            cand_name.clone(),
                            cand_modules.clone(),
                        );
                        if ObjectID::from(*cand_modules[0].address()) == ObjectID::ZERO
                            && !zero_module_addr_map.is_empty()
                        {
                            if let Err(e) =
                                cand_pkg.rewrite_deps_by_module_name(&zero_module_addr_map)
                            {
                                log::debug!(
                                    "ERROR: rewrite deps by module name failed for candidate {} at {}: {:?}",
                                    cand_name,
                                    addr,
                                    e
                                );
                                return Err(e);
                            }
                        }
                        cand_pkg.ensure_immediate_deps();
                        for d in cand_pkg.dependencies().iter().copied() {
                            queue.push_back(d);
                        }
                        let cand_pkg = match cand_pkg.movy_mock() {
                            Ok(v) => v,
                            Err(e) => {
                                log::debug!(
                                    "ERROR: movy_mock failed for candidate {} at {}: {:?}",
                                    cand_name,
                                    addr,
                                    e
                                );
                                return Err(e);
                            }
                        };
                        if let Err(e) = executor.force_deploy_contract_at(addr, cand_pkg) {
                            log::debug!(
                                "ERROR: force publish candidate {} at {} failed: {:?}",
                                cand_name,
                                addr,
                                e
                            );
                            return Err(e);
                        }
                        log::debug!(
                            "forced publish dep {} from package {} (required by {})",
                            addr,
                            cand_name,
                            dep_name
                        );
                    }

                    let dep_pkg = dep_pkg.movy_mock()?;

                    let dep_address = if dep_self_addr != ObjectID::ZERO {
                        if self.db.get_object(&dep_self_addr).is_some() {
                            dep_self_addr
                        } else {
                            match executor.force_deploy_contract_at(dep_self_addr, dep_pkg) {
                                Ok(v) => v,
                                Err(e) => {
                                    log::debug!(
                                        "ERROR: force publish dep {} at {} failed: {:?}",
                                        dep_name,
                                        dep_self_addr,
                                        e
                                    );
                                    return Err(e);
                                }
                            }
                        }
                    } else {
                        match executor.deploy_contract(
                            epoch,
                            epoch_ms,
                            deployer.into(),
                            gas,
                            dep_pkg,
                        ) {
                            Ok(v) => v,
                            Err(e) => {
                                log::debug!(
                                    "ERROR: deploy dep {} (self=0x0) failed: {:?}",
                                    dep_name,
                                    e
                                );
                                return Err(e);
                            }
                        }
                    };

                    log::debug!(
                        "dep publish {}: self={} -> published={}",
                        dep_name,
                        dep_self_addr,
                        dep_address
                    );

                    if dep_is_zero_addr {
                        if let Err(e) = record_zero_address_modules(
                            &mut zero_module_addr_map,
                            modules,
                            dep_address,
                        ) {
                            log::debug!(
                                "ERROR: zero-address module map update failed for dep {}: {:?}",
                                dep_name,
                                e
                            );
                            return Err(e);
                        }
                    }
                }

                if !zero_module_addr_map.is_empty() {
                    log::debug!(
                        "zero-address module map size={}",
                        zero_module_addr_map.len()
                    );
                    compiled_result.rewrite_deps_by_module_name(&zero_module_addr_map)?;
                    abi_result.rewrite_deps_by_module_name(&zero_module_addr_map)?;
                }

                log::debug!("dependency auto-publish finished");
            } else {
                log::debug!("no unpublished deps detected; skip auto-publish deps");
            }

            compiled_result.ensure_immediate_deps();
            compiled_result.rewrite_dependency_storage_ids(&original_to_storage);

            // Ensure every dependency package exists in the store before publishing root.
            // This catches the "address is set but package doesn't exist (yet)" case.
            for dep in compiled_result.dependencies().iter().copied() {
                let exists = self.db.get_package_object(&dep).ok().flatten().is_some()
                    || self.db.get_object(&dep).is_some();
                if exists {
                    continue;
                }

                log::debug!(
                    "dep {} missing in store; trying to force publish from compiled deps...",
                    dep
                );

                let Some(candidates) = compiled_result.dep_modules_by_addr().get(&dep) else {
                    log::debug!("no compiled dep modules recorded for addr {}", dep);
                    continue;
                };
                if candidates.len() != 1 {
                    log::debug!(
                        "ambiguous dep candidates for addr {}: {}",
                        dep,
                        candidates.keys().cloned().collect::<Vec<_>>().join(",")
                    );
                    continue;
                }
                let (dep_name, modules) = candidates.iter().next().unwrap();
                if modules.is_empty() {
                    log::debug!("dep candidate {} for addr {} has 0 modules", dep_name, dep);
                    continue;
                }

                let mut dep_pkg =
                    SuiCompiledPackage::new_unpublished(dep_name.clone(), modules.clone());
                if ObjectID::from(*modules[0].address()) == ObjectID::ZERO
                    && !zero_module_addr_map.is_empty()
                {
                    dep_pkg.rewrite_deps_by_module_name(&zero_module_addr_map)?;
                }
                dep_pkg.ensure_immediate_deps();
                let dep_pkg = dep_pkg.movy_mock()?;
                executor.force_deploy_contract_at(dep, dep_pkg)?;
                log::debug!(
                    "forced publish missing dep {} from package {}",
                    dep,
                    dep_name
                );
            }

            log::debug!("scanning root external refs...");
            let refs = external_module_refs(compiled_result.all_modules_iter().cloned());
            log::debug!("root external refs count={}", refs.len());
            for (addr, names) in refs.iter() {
                let mapped_storage = if self.db.get_object(addr).is_none()
                    && self.db.get_package_object(addr).ok().flatten().is_none()
                {
                    original_to_storage.get(addr).copied()
                } else {
                    None
                };
                let check_addr = mapped_storage.unwrap_or(*addr);

                if let Some(storage) = mapped_storage {
                    log::debug!(
                        "ref addr={} (mapped to storage {}) modules={}",
                        addr,
                        storage,
                        names.iter().cloned().collect::<Vec<_>>().join(",")
                    );
                } else {
                    log::debug!(
                        "ref addr={} modules={}",
                        addr,
                        names.iter().cloned().collect::<Vec<_>>().join(",")
                    );
                }
                // Best-effort: check package presence and required modules.
                let pkg_obj = self
                    .db
                    .get_package_object(&check_addr)
                    .ok()
                    .flatten()
                    .map(|p| p.object().clone())
                    .or_else(|| self.db.get_object(&check_addr));
                if let Some(obj) = pkg_obj {
                    if let Some(pkg) = obj.data.try_as_package() {
                        for m in names {
                            if !pkg.serialized_module_map().contains_key(m) {
                                log::debug!(
                                    "missing module {} in package {} (version={})",
                                    m,
                                    check_addr,
                                    obj.version().value()
                                );
                            }
                        }
                    } else {
                        log::debug!("dep {} exists but not a package", check_addr);
                    }
                } else {
                    log::debug!("dep package {} not found in store", check_addr);
                }
            }
            log::debug!(
                "root deps list size={} deps={}",
                compiled_result.dependencies().len(),
                compiled_result
                    .dependencies()
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            log::debug!("root external refs scan done");

            let compiled_result = compiled_result.movy_mock()?;
            log::debug!(
                "test modules are {}",
                compiled_result
                    .test_modules()
                    .iter()
                    .map(|v| v.self_id().name().to_string())
                    .join(", ")
            );
            log::debug!("about to publish root package");
            executor.deploy_contract(epoch, epoch_ms, deployer.into(), gas, compiled_result)?
        };

        let mut non_test_abi = abi_result.abi()?;
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
