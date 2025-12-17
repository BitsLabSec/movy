use std::{collections::BTreeSet, io::Write, path::Path};

use color_eyre::eyre::eyre;
use itertools::Itertools;
use log::{debug, trace};
use move_binary_format::CompiledModule;
use move_compiler::editions::Flavor;
use move_package::{
    resolution::resolution_graph::ResolvedGraph, source_package::layout::SourcePackageLayout,
};
use movy_types::{
    abi::{MOVY_INIT, MOVY_ORACLE, MovePackageAbi},
    error::MovyError,
    input::MoveAddress,
};
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

#[derive(Debug, Clone)]
pub struct SuiCompiledPackage {
    pub package_id: ObjectID,
    pub package_name: String,
    pub package_names: Vec<String>,
    modules: Vec<CompiledModule>,
    dependencies: Vec<ObjectID>,
    published_dependencies: Vec<ObjectID>,
}

impl SuiCompiledPackage {
    pub fn all_modules_iter(&self) -> impl Iterator<Item = &CompiledModule> {
        self.modules.iter()
    }
    pub fn into_deployment(self) -> (Vec<CompiledModule>, Vec<ObjectID>) {
        (self.modules.into_iter().collect(), self.dependencies)
    }
    pub fn abi(&self) -> Result<MovePackageAbi, MovyError> {
        MovePackageAbi::from_sui_id_and_modules(self.package_id, self.all_modules_iter())
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
        };

        let mut deps: BTreeSet<ObjectID> =
            self.published_dependencies.clone().into_iter().collect();
        for md in self.modules.iter() {
            let md = Self::mock_module(md);
            deps.extend(
                md.immediate_dependencies()
                    .into_iter()
                    .map(|v| ObjectID::from(*v.address()))
                    .filter(|v| v != &self.package_id),
            );
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
        let package_name = artifacts
            .package
            .compiled_package_info
            .package_name
            .to_string();
        let mut package_names = BTreeSet::new();
        package_names.insert(package_name.clone());
        for (dep_name, unit) in artifacts.package.deps_compiled_units.iter() {
            let module_addr: MoveAddress = (*unit.unit.module.self_id().address()).into();
            if ObjectID::from(module_addr) == root_address {
                package_names.insert(dep_name.to_string());
            }
        }
        let package_names = package_names.into_iter().collect::<Vec<_>>();
        let modules = artifacts
            .package
            .all_compiled_units()
            .filter(|m| root_address == m.address.into_inner().into())
            .map(|m| m.module.clone())
            .collect::<Vec<_>>();
        debug!("Package {} has {} modules", root_address, modules.len());
        let deps = modules
            .iter()
            .flat_map(|m| {
                m.immediate_dependencies()
                    .iter()
                    .map(|m| m.address().to_owned().into())
                    .filter(|a| a != &root_address)
                    .collect::<Vec<_>>()
            })
            .chain(artifacts.dependency_ids.published.values().cloned())
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
