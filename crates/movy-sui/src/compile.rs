use std::{collections::BTreeSet, fmt::Display, io::Write, path::Path};

use color_eyre::eyre::eyre;
use itertools::Itertools;
use move_binary_format::CompiledModule;
use move_compiler::editions::Flavor;
use move_core_types::account_address::AccountAddress;
use movy_types::{
    abi::{MOVY_INIT, MOVY_ORACLE, MovePackageAbi},
    error::MovyError,
    input::MoveAddress,
};
use serde::{Deserialize, Serialize};
use sui_move_build::{BuildConfig, CompiledPackage};
use sui_types::base_types::ObjectID;
use tracing::{debug, trace};

pub fn build_package_resolved(
    folder: &Path,
    test_mode: bool,
) -> Result<CompiledPackage, MovyError> {
    let mut cfg = move_package_alt_compilation::build_config::BuildConfig::default();
    cfg.default_flavor = Some(Flavor::Sui);
    cfg.test_mode = test_mode;
    cfg.silence_warnings = true;
    // IMPORTANT:
    // Sui now tries to assign a few unique addresses for each unpublished modules (see `unique_hash` and `NamedAddress::Unpublished`)
    // We overrite this and request the behavior of the old version
    cfg.set_unpublished_deps_to_zero = true;

    let cfg = BuildConfig {
        config: cfg,
        run_bytecode_verifier: false,
        print_diags_to_stderr: false,
        environment: sui_package_alt::mainnet_environment(),
    };
    trace!("Build config is {:?}", &cfg.config);
    // cfg.compile_package(path, writer) // reference
    Ok(cfg.build(folder)?)
}

pub fn mock_module_address(address: ObjectID, module: &mut CompiledModule) {
    let sh = module.self_handle().clone();
    let address_idx = sh.address;
    let name = module.identifier_at(sh.name).to_string();
    if let Some(addr) = module.address_identifiers.get_mut(address_idx.0 as usize) {
        tracing::debug!(
            "mocking module from {}:{} to {}:{}",
            addr,
            name,
            address,
            name
        );
        *addr = address.into();
    } else {
        tracing::warn!(
            "module {} does not have self address?! {:?}, module {:?}",
            name,
            address_idx,
            module
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiCompiledPackage {
    pub package_id: ObjectID,
    pub package_name: String,
    pub package_names: Vec<String>,
    modules: Vec<CompiledModule>,
    dependencies: Vec<ObjectID>,
    published_dependencies: Vec<ObjectID>,
}

impl Display for SuiCompiledPackage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "Project(id={}, name={}, modules=[{}], deps=[{}])",
            self.package_id,
            self.package_name,
            self.modules
                .iter()
                .map(|v| {
                    let id = v.self_id();
                    format!("{}:{}", id.address(), id.name().as_str())
                })
                .join(", "),
            self.dependencies.iter().map(|t| t.to_string()).join(", ")
        ))
    }
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
    pub fn dependencies(&self) -> &Vec<ObjectID> {
        &self.dependencies
    }

    pub fn modules_mut(&mut self) -> &mut Vec<CompiledModule> {
        &mut self.modules
    }

    pub fn contains_nonzero_modules(&self) -> bool {
        self.modules
            .iter()
            .any(|v| ObjectID::from(*v.address()) == ObjectID::ZERO)
    }

    pub fn all_same_address(&self) -> Option<AccountAddress> {
        self.modules
            .iter()
            .map(|v| *v.address())
            .all_equal_value()
            .ok()
    }

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
            tracing::debug!("Mock module publishable {}::{}", address, mname);
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

    pub fn build_checked(
        folder: &Path,
        test_mode: bool,
        with_unpublished: bool,
        verify_deps: bool,
    ) -> Result<SuiCompiledPackage, MovyError> {
        let artifacts = build_package_resolved(folder, test_mode)?;
        debug!("artifacts dep: {:?}", artifacts.dependency_ids);
        debug!("published: {:?}", artifacts.dependency_ids.published);

        let root_address = match artifacts.published_at {
            Some(address) => address,
            None => ObjectID::ZERO,
        };
        let package_name = artifacts
            .package
            .compiled_package_info
            .package_name
            .to_string();
        let span = tracing::debug_span!("root", root=%root_address, pkg=package_name);
        let _ = span.enter();

        let mut package_names = BTreeSet::new();
        package_names.insert(package_name.clone());
        for (dep_name, unit) in artifacts.package.deps_compiled_units.iter() {
            let module_addr: MoveAddress = (*unit.unit.module.self_id().address()).into();
            if ObjectID::from(module_addr) == root_address {
                package_names.insert(dep_name.to_string());
            }
        }
        let package_names = package_names.into_iter().collect::<Vec<_>>();

        if verify_deps && !with_unpublished {
            for (sym, md) in artifacts.package.deps_compiled_units.iter() {
                let address = md.unit.address.into_inner();
                if address == AccountAddress::ZERO {
                    return Err(eyre!(
                        "dependency {}({}:{}) does not have a published address and we are compiling _without_ bundling unpublished dependencies",
                        sym.as_str(),
                        md.unit.module.self_id().address(),
                         md.unit.module.self_id().name().as_str()
                    ).into());
                }
            }
        }

        let mut modules = if with_unpublished {
            artifacts
                .package
                .all_compiled_units()
                .filter(|m| {
                    tracing::trace!("Compiled module address: {}", m.address.into_inner());
                    root_address == m.address.into_inner().into()
                        || m.address.into_inner() == AccountAddress::ZERO
                })
                .map(|m| m.module.clone())
                .collect_vec()
        } else {
            artifacts
                .package
                .root_compiled_units
                .into_iter()
                .map(|u| u.unit.module)
                .collect_vec()
        };

        if with_unpublished {
            // Dependency linkage
            for md in modules.iter_mut() {
                for (hd_idx, hd) in md.module_handles.clone().into_iter().enumerate() {
                    let addr_idx = hd.address.0 as usize;
                    let name = md.identifier_at(hd.name).to_string();
                    let addr = *md.address_identifiers.get_mut(addr_idx).unwrap();

                    if with_unpublished
                        && addr == AccountAddress::ZERO
                        && root_address != ObjectID::ZERO
                    {
                        tracing::debug!(
                            "Mocking a bundled dependecy handle {}:{} ({}:{}) to {}:{}",
                            addr,
                            name,
                            hd.address.0,
                            hd.name.0,
                            root_address,
                            name
                        );
                        if let Some(t) = md
                            .address_identifiers
                            .iter()
                            .position(|t| t == &root_address.into())
                        {
                            md.module_handles.get_mut(hd_idx).unwrap().address.0 = t as _;
                        } else {
                            *md.address_identifiers.get_mut(addr_idx).unwrap() =
                                root_address.into();
                        }
                    }
                }

                // TODO: Remove unsued address identifiers (in most cases, 0x0)
            }
            // mock module self address
            for md in modules.iter_mut() {
                mock_module_address(root_address, md);
            }
        }

        if tracing::enabled!(tracing::Level::TRACE) {
            for md in modules.iter() {
                tracing::trace!("Module is {:?}", md);
            }
        }

        if verify_deps && root_address != ObjectID::ZERO {
            for md in modules.iter() {
                for dep in md.immediate_dependencies() {
                    let addr = *dep.address();
                    if addr == AccountAddress::ZERO {
                        return Err(eyre!(
                            "module {}:{} still has 0x0 immediate dependency, maybe bundling unpublished dependencies?",
                            dep.address(),
                            dep.name().as_str()
                        )
                        .into());
                    }
                }
            }
        }

        if modules.is_empty() {
            return Err(eyre!(
                "Compiling {} yields 0 modules for root {}",
                folder.display(),
                root_address
            )
            .into());
        }
        debug!("Package has {} modules", modules.len());
        let deps = artifacts
            .dependency_ids
            .published
            .iter()
            .map(|v| *v.1)
            // .chain(modules.iter().map(|v| {
            //     v.immediate_dependencies().into_iter().map(|t| (*t.address()).into()) // we comment this because we may add multiple addresses that belongs to the upgraded packages which share the same UpgradeCap.
            // }).flatten())
            .filter(|t| t != &ObjectID::ZERO && t != &root_address)
            .collect::<BTreeSet<ObjectID>>();

        let published: Vec<ObjectID> = artifacts
            .dependency_ids
            .published
            .values()
            .cloned()
            .collect();
        debug!(
            "Package {} transitively depends on {}, the publised ids are {}",
            root_address,
            deps.iter().map(|t| t.to_string()).join(","),
            published.iter().map(|t| t.to_string()).join(",")
        );
        Ok(SuiCompiledPackage {
            package_id: (*root_address).into(),
            package_name,
            package_names,
            modules,
            dependencies: deps.into_iter().collect(),
            published_dependencies: published,
        })
    }

    pub fn build(
        folder: &Path,
        test_mode: bool,
        with_unpublished: bool,
    ) -> Result<SuiCompiledPackage, MovyError> {
        Self::build_checked(folder, test_mode, with_unpublished, true)
    }

    // This function builds all sui packages at a given folder, packaging all the
    // unpublished dependencies together.
    pub fn build_all_unpublished_from_folder(
        folder: &Path,
        test_mode: bool,
    ) -> Result<SuiCompiledPackage, MovyError> {
        // Legacy code path
        Self::build(folder, test_mode, true)
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
    use std::path::PathBuf;

    use crate::compile::SuiCompiledPackage;

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
        dbg!(&out);
        let abi = out.abi().unwrap();
        dbg!(&abi);
    }

    #[test]
    fn test_build() {
        let out = SuiCompiledPackage::build(
            &PathBuf::from("/home/mio/bitslab/movy-oss/test-data/onchain/hello3"),
            false,
            true,
        )
        .unwrap();
    }
}
