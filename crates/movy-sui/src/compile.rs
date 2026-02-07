use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::Path,
};

use color_eyre::eyre::eyre;
use itertools::Itertools;
use log::{debug, trace};
use move_binary_format::{
    CompiledModule,
    file_format::{AddressIdentifierIndex, ModuleHandleIndex},
};
use move_compiler::editions::Flavor;
use move_core_types::account_address::AccountAddress;
use move_package::{
    resolution::resolution_graph::ResolvedGraph, source_package::layout::SourcePackageLayout,
};
use movy_types::{
    abi::{MOVY_INIT, MOVY_ORACLE, MovePackageAbi},
    error::MovyError,
    input::MoveAddress,
};
use serde::{Deserialize, Serialize};
use sui_move_build::{BuildConfig, CompiledPackage, build_from_resolution_graph, implicit_deps};
use sui_package_management::{PublishedAtError, system_package_versions::latest_system_packages};
use sui_types::{base_types::ObjectID, digests::get_mainnet_chain_identifier};

pub fn build_package_resolved(
    folder: &Path,
    test_mode: bool,
) -> Result<(CompiledPackage, ResolvedGraph), MovyError> {
    let mut cfg = move_package::BuildConfig::default();
    cfg.implicit_dependencies = implicit_deps(latest_system_packages());
    cfg.default_flavor = Some(Flavor::Sui);
    cfg.lock_file = Some(folder.join(SourcePackageLayout::Lock.path()));
    cfg.test_mode = test_mode;
    cfg.silence_warnings = true;

    let cfg = BuildConfig {
        config: cfg,
        run_bytecode_verifier: false,
        print_diags_to_stderr: false,
        chain_id: Some(get_mainnet_chain_identifier().to_string()),
    };
    trace!("Build config is {:?}", &cfg.config);

    // cfg.compile_package(path, writer) // reference
    let chain_id = cfg.chain_id.clone();
    let resolution_graph = cfg.resolution_graph(folder, chain_id.clone())?;
    let artifacts = build_from_resolution_graph(resolution_graph.clone(), false, false, chain_id)?;
    Ok((artifacts, resolution_graph))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiCompiledPackage {
    pub package_id: ObjectID,
    pub package_name: String,
    pub package_names: Vec<String>,
    modules: Vec<CompiledModule>,
    dependencies: Vec<ObjectID>,
    published_dependencies: Vec<ObjectID>,
    #[serde(default)]
    unpublished_dep_modules: BTreeMap<String, Vec<CompiledModule>>,
    #[serde(default)]
    unpublished_dep_order: Vec<String>,
    #[serde(skip, default)]
    dep_modules_by_addr: BTreeMap<ObjectID, BTreeMap<String, Vec<CompiledModule>>>,
}

impl SuiCompiledPackage {
    pub fn all_modules_iter(&self) -> impl Iterator<Item = &CompiledModule> {
        self.modules.iter()
    }
    pub fn dependencies(&self) -> &[ObjectID] {
        &self.dependencies
    }
    pub fn rewrite_dependency_storage_ids(&mut self, id_map: &BTreeMap<ObjectID, ObjectID>) {
        if id_map.is_empty() {
            return;
        }
        let mut out: BTreeSet<ObjectID> = BTreeSet::new();
        for dep in self.dependencies.iter().copied() {
            if let Some(mapped) = id_map.get(&dep) {
                out.insert(*mapped);
            } else {
                out.insert(dep);
            }
        }
        self.dependencies = out.into_iter().collect();
    }

    /// Force the runtime address (i.e. module self address) for every module in this package.
    /// This is needed when simulating upgraded packages, where the storage ID can differ from the
    /// runtime/original ID embedded in module bytes.
    pub fn set_self_address(&mut self, new_addr: ObjectID) -> Result<(), MovyError> {
        for module in self.modules.iter_mut() {
            set_module_self_address(module, new_addr)?;
        }
        Ok(())
    }
    pub fn unpublished_dep_order(&self) -> &[String] {
        &self.unpublished_dep_order
    }
    pub fn unpublished_dep_modules(&self) -> &BTreeMap<String, Vec<CompiledModule>> {
        &self.unpublished_dep_modules
    }
    pub fn dep_modules_by_addr(
        &self,
    ) -> &BTreeMap<ObjectID, BTreeMap<String, Vec<CompiledModule>>> {
        &self.dep_modules_by_addr
    }
    pub fn new_unpublished(package_name: String, modules: Vec<CompiledModule>) -> Self {
        Self {
            package_id: ObjectID::ZERO,
            package_name: package_name.clone(),
            package_names: vec![package_name],
            modules,
            dependencies: vec![],
            published_dependencies: vec![],
            unpublished_dep_modules: BTreeMap::new(),
            unpublished_dep_order: vec![],
            dep_modules_by_addr: BTreeMap::new(),
        }
    }
    pub fn rewrite_deps_by_module_name(
        &mut self,
        module_addr_map: &BTreeMap<String, ObjectID>,
    ) -> Result<(), MovyError> {
        rewrite_modules_by_name(&mut self.modules, module_addr_map)
    }
    pub fn into_deployment(self) -> (Vec<CompiledModule>, Vec<ObjectID>) {
        (self.modules.into_iter().collect(), self.dependencies)
    }
    pub fn abi(&self) -> Result<MovePackageAbi, MovyError> {
        MovePackageAbi::from_sui_id_and_modules(self.package_id, self.all_modules_iter())
    }
    pub fn ensure_immediate_deps(&mut self) {
        let mut deps: BTreeSet<ObjectID> = self.dependencies.iter().copied().collect();
        for md in &self.modules {
            for dep in md.immediate_dependencies() {
                let id: ObjectID = (*dep.address()).into();
                if id != self.package_id && id != ObjectID::ZERO {
                    deps.insert(id);
                }
            }
        }
        self.dependencies = deps.into_iter().collect();
    }
}

impl SuiCompiledPackage {
    pub fn test_modules(&self) -> Vec<&CompiledModule> {
        self.modules
            .iter()
            .filter(|v| Self::contains_unit_test(v))
            .collect()
    }
    fn contains_unit_test(module: &CompiledModule) -> bool {
        for fcall in module.function_handles() {
            let md = module.module_handle_at(fcall.module);
            let fname = module.identifier_at(fcall.name);
            let maddress = module.address_identifier_at(md.address);
            let mname = module.identifier_at(md.name);
            if MoveAddress::from(*maddress) == MoveAddress::one()
                && mname.as_str() == "unit_test"
                && fname.as_str() == "poison"
            {
                return true;
            }
        }
        false
    }
    fn contains_movy(modlue: &CompiledModule) -> bool {
        for fdef in modlue.function_defs() {
            let func = modlue.function_handle_at(fdef.function);
            let fname = modlue.identifier_at(func.name).to_string();
            if fname == MOVY_INIT || fname.starts_with(MOVY_ORACLE) {
                return true;
            }
        }

        false
    }

    fn mock_module(md: &CompiledModule) -> CompiledModule {
        let mut md = md.clone();
        let address: MoveAddress = (*md.address()).into();
        let mname = md.name().to_string();
        if !md.publishable {
            log::debug!("Mock module publishable {}::{}", address, mname);
            md.publishable = true;
        };
        md
    }

    pub fn movy_mock(&self) -> Result<Self, MovyError> {
        let mut new_package = Self {
            package_id: self.package_id,
            package_name: self.package_name.clone(),
            package_names: self.package_names.clone(),
            modules: vec![],
            dependencies: vec![],
            published_dependencies: self.published_dependencies.clone(),
            unpublished_dep_modules: self.unpublished_dep_modules.clone(),
            unpublished_dep_order: self.unpublished_dep_order.clone(),
            dep_modules_by_addr: self.dep_modules_by_addr.clone(),
        };

        let deps: BTreeSet<ObjectID> = self.dependencies.clone().into_iter().collect();
        for md in self.modules.iter() {
            let md = Self::mock_module(md);
            // deps.extend(
            //     md.immediate_dependencies()
            //         .into_iter()
            //         .map(|v| ObjectID::from(*v.address()))
            //         .filter(|v| v != &self.package_id),
            // );
            new_package.modules.push(md);
        }
        new_package.dependencies = deps.into_iter().collect();
        Ok(new_package)
    }

    // This function builds all sui packages at a given folder, packaging all the
    // unpublished dependencies together.
    pub fn build_all_unpublished_from_folder(
        folder: &Path,
        test_mode: bool,
    ) -> Result<SuiCompiledPackage, MovyError> {
        let (artifacts, _) = build_package_resolved(folder, test_mode)?;
        debug!(
            "artifacts dep: {:?}",
            artifacts.dependency_graph.topological_order()
        );
        debug!("published: {:?}", artifacts.dependency_ids.published);

        let root_address = match artifacts.published_at {
            Ok(address) => address,
            Err(PublishedAtError::NotPresent) => ObjectID::ZERO,
            _ => return Err(eyre!("Invalid published-at: {:?}", &artifacts.published_at).into()),
        };
        debug!("Root address is {}", root_address);
        let package_name = artifacts
            .package
            .compiled_package_info
            .package_name
            .to_string();
        let unpublished_deps = &artifacts.dependency_ids.unpublished;
        let mut dep_modules_by_addr: BTreeMap<ObjectID, BTreeMap<String, Vec<CompiledModule>>> =
            BTreeMap::new();
        for (dep_name, unit) in artifacts.package.deps_compiled_units.iter() {
            let addr: ObjectID = ObjectID::from(*unit.unit.module.self_id().address());
            dep_modules_by_addr
                .entry(addr)
                .or_default()
                .entry(dep_name.to_string())
                .or_default()
                .push(unit.unit.module.clone());
        }
        let mut unpublished_dep_modules: BTreeMap<String, Vec<CompiledModule>> = BTreeMap::new();
        for (dep_name, unit) in artifacts.package.deps_compiled_units.iter() {
            if unpublished_deps.contains(dep_name) {
                unpublished_dep_modules
                    .entry(dep_name.to_string())
                    .or_default()
                    .push(unit.unit.module.clone());
            }
        }
        let mut unpublished_dep_order = Vec::new();
        // `DependencyGraph::topological_order()` orders a package *before* its dependencies
        // (i.e. root first). For publishing, we need the reverse: dependencies first.
        for dep_name in artifacts
            .dependency_graph
            .topological_order()
            .into_iter()
            .rev()
        {
            if dep_name == artifacts.package.compiled_package_info.package_name {
                continue;
            }
            if unpublished_deps.contains(&dep_name) {
                unpublished_dep_order.push(dep_name.to_string());
            }
        }
        let mut package_names = BTreeSet::new();
        package_names.insert(package_name.clone());
        let modules = if root_address == ObjectID::ZERO {
            debug!("Root address is zero; using root modules only");
            artifacts
                .package
                .root_modules()
                .map(|m| m.unit.module.clone())
                .collect::<Vec<_>>()
        } else {
            for (dep_name, unit) in artifacts.package.deps_compiled_units.iter() {
                let module_addr: MoveAddress = (*unit.unit.module.self_id().address()).into();
                if ObjectID::from(module_addr) == root_address {
                    package_names.insert(dep_name.to_string());
                }
            }
            artifacts
                .package
                .all_compiled_units()
                .filter(|m| {
                    debug!("Compiled module address: {}", m.address.into_inner());
                    root_address == m.address.into_inner().into()
                })
                .map(|m| m.module.clone())
                .collect::<Vec<_>>()
        };
        let package_names = package_names.into_iter().collect::<Vec<_>>();
        if modules.len() == 0 {
            return Err(eyre!(
                "Compiling {} yields 0 modules for root {}",
                folder.display(),
                root_address
            )
            .into());
        }
        debug!("Package {} has {} modules", root_address, modules.len());
        // let deps = modules
        //     .iter()
        //     .flat_map(|m| {
        //         m.immediate_dependencies()
        //             .iter()
        //             .map(|m| m.address().to_owned().into())
        //             .filter(|a| a != &root_address)
        //             .collect::<Vec<_>>()
        //     })
        //     .chain(artifacts.dependency_ids.published.values().cloned())
        //     .collect::<BTreeSet<ObjectID>>();
        let deps = artifacts
            .dependency_ids
            .published
            .iter()
            .map(|v| *v.1)
            .collect::<BTreeSet<ObjectID>>();

        debug!(
            "Package {} transitively depends on {}",
            root_address,
            deps.iter().map(|t| t.to_string()).join(",")
        );
        Ok(SuiCompiledPackage {
            package_id: (*root_address).into(),
            package_name,
            package_names,
            modules,
            dependencies: deps.into_iter().collect(),
            published_dependencies: artifacts
                .dependency_ids
                .published
                .values()
                .cloned()
                .collect(),
            unpublished_dep_modules,
            unpublished_dep_order,
            dep_modules_by_addr,
        })
    }

    pub fn build_quick(package: &str, module: &str, content: &str) -> Result<Self, MovyError> {
        let dir = tempfile::TempDir::new()?;

        let toml = format!(
            r#"[package]
    name = "{}"
    edition = "2024.beta"

    [dependencies]
    [addresses]
    {} = "0x0"
    [dev-dependencies]
    [dev-addresses]
    "#,
            package, package
        );
        let mut fp = std::fs::File::create(dir.path().join("Move.toml"))?;
        fp.write_all(toml.as_bytes())?;

        std::fs::create_dir_all(dir.path().join("sources"))?;

        let mut fp = std::fs::File::create(dir.path().join(format!("sources/{}.move", module)))?;
        fp.write_all(content.as_bytes())?;

        Self::build_all_unpublished_from_folder(dir.path(), false)
    }
}

fn rewrite_modules_by_name(
    modules: &mut [CompiledModule],
    module_addr_map: &BTreeMap<String, ObjectID>,
) -> Result<(), MovyError> {
    for module in modules.iter_mut() {
        rewrite_module_by_name(module, module_addr_map)?;
    }
    Ok(())
}

fn rewrite_module_by_name(
    module: &mut CompiledModule,
    module_addr_map: &BTreeMap<String, ObjectID>,
) -> Result<(), MovyError> {
    let mut addr_index_map: BTreeMap<ObjectID, AddressIdentifierIndex> = BTreeMap::new();
    for (idx, addr) in module.address_identifiers.iter().enumerate() {
        addr_index_map.insert(ObjectID::from(*addr), AddressIdentifierIndex(idx as u16));
    }

    let self_handle_idx = module.self_handle_idx();
    for idx in 0..module.module_handles.len() {
        let handle_idx = ModuleHandleIndex(idx as u16);
        if handle_idx == self_handle_idx {
            continue;
        }
        let handle = module.module_handles[idx].clone();
        let current_addr = *module.address_identifier_at(handle.address);
        if current_addr != AccountAddress::ZERO {
            continue;
        }
        let name = module.identifier_at(handle.name).to_string();
        let Some(new_addr) = module_addr_map.get(&name) else {
            continue;
        };
        let addr_idx = if let Some(existing) = addr_index_map.get(new_addr) {
            *existing
        } else {
            let addr = AccountAddress::from(*new_addr);
            module.address_identifiers.push(addr);
            let new_idx = AddressIdentifierIndex((module.address_identifiers.len() - 1) as u16);
            addr_index_map.insert(*new_addr, new_idx);
            new_idx
        };
        module.module_handles[idx].address = addr_idx;
    }
    Ok(())
}

fn set_module_self_address(
    module: &mut CompiledModule,
    new_addr: ObjectID,
) -> Result<(), MovyError> {
    let new_addr = AccountAddress::from(new_addr);
    let self_handle_idx = module.self_handle_idx();
    let self_handle_pos = self_handle_idx.0 as usize;
    let current_idx = module.module_handles[self_handle_pos].address;
    let current_addr = *module.address_identifier_at(current_idx);

    if current_addr != AccountAddress::ZERO && current_addr != new_addr {
        return Err(eyre!(
            "cannot rewrite module {} self address from {} to {}",
            module.name(),
            current_addr,
            new_addr
        )
        .into());
    }

    log::debug!(
        "set self address: module={} {} -> {}",
        module.name(),
        current_addr,
        new_addr
    );

    // Ensure the new address exists in the address table.
    let mut addr_index_map: BTreeMap<AccountAddress, AddressIdentifierIndex> = BTreeMap::new();
    for (idx, addr) in module.address_identifiers.iter().enumerate() {
        addr_index_map.insert(*addr, AddressIdentifierIndex(idx as u16));
    }
    let new_idx = if let Some(idx) = addr_index_map.get(&new_addr) {
        *idx
    } else {
        module.address_identifiers.push(new_addr);
        AddressIdentifierIndex((module.address_identifiers.len() - 1) as u16)
    };
    module.module_handles[self_handle_pos].address = new_idx;
    Ok(())
}

#[cfg(test)]
mod test {
    use std::{
        collections::{HashMap, HashSet},
        path::PathBuf,
    };

    use crate::compile::{SuiCompiledPackage, build_package_resolved};

    #[test]
    fn test_build_simple() {
        let out = SuiCompiledPackage::build_quick(
            "hello",
            "hello",
            r#"module hello::hello;
public struct Test<T: drop, Z: drop> {
    t: T,
    k: Z
}
public fun new<T: drop, V: drop>(ctx: &mut TxContext, t2: &Test<T, V>, t: &Test<u64, V>) {
}
"#,
        )
        .unwrap();
        let abi = out.abi().unwrap();
        dbg!(&abi);
    }
}
