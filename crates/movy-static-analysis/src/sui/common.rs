use std::collections::BTreeSet;

use itertools::Itertools;
use move_binary_format::{CompiledModule, binary_config::BinaryConfig};
use move_core_types::{
    account_address::AccountAddress, identifier::Identifier, language_storage::ModuleId,
};
use move_model::model::{FunId, GlobalEnv};
use move_stackless_bytecode::stackless_bytecode::Bytecode;
use movy_replay::{
    db::{ObjectStoreCachedStore, ObjectStoreInfo},
    env::SuiTestingEnv,
};
use movy_types::{error::MovyError, input::MoveAddress};
use sui_types::{base_types::ObjectID, storage::ObjectStore};

use super::generate_bytecode::{
    FunctionInfo, ModuleBytecode, generate_stackless_bytecode_for_module,
};

pub struct ModuleAnalysis {
    pub compiled: CompiledModule,
    pub stackless: ModuleBytecode,
    pub global_env: GlobalEnv,
}

impl ModuleAnalysis {
    pub fn qualified_module_name(&self) -> String {
        let module_id = self.compiled.self_id();
        format!("{}::{}", module_id.address(), module_id.name())
    }

    pub fn functions(&self) -> &[FunctionInfo] {
        &self.stackless.functions
    }

    pub fn is_native(&self, function: &FunctionInfo) -> bool {
        let defs = self.compiled.function_defs();
        match defs.get(function.idx) {
            Some(def) => def.is_native(),
            None => {
                log::debug!(
                    "Skip function {}::{} (idx {}) - definition missing in compiled module",
                    self.qualified_module_name(),
                    function.name,
                    function.idx
                );
                true
            }
        }
    }

    pub fn get_function_name(&self, fun_id: &FunId) -> String {
        self.global_env
            .symbol_pool()
            .string(fun_id.symbol())
            .to_string()
    }
}

fn fetch_compiled_module<T>(
    env: &SuiTestingEnv<T>,
    module_id: &move_core_types::language_storage::ModuleId,
) -> Result<Option<CompiledModule>, MovyError>
where
    T: ObjectStore + ObjectStoreInfo,
{
    let module_addr: MoveAddress = (*module_id.address()).into();
    let package_id = module_addr;

    let Some(object) = env.inner().get_object(&ObjectID::from(package_id)) else {
        log::debug!(
            "Object for dependency {}::{} (package {}) not found in backing store",
            module_id.address(),
            module_id.name(),
            package_id
        );
        return Ok(None);
    };
    let Some(package) = object.data.try_as_package() else {
        log::debug!(
            "Dependency {}::{} (package {}) is not a package object",
            module_id.address(),
            module_id.name(),
            package_id
        );
        return Ok(None);
    };
    let module = package.deserialize_module_by_str(
        module_id.name().as_str(),
        &BinaryConfig::new_unpublishable(),
    );
    Ok(module.ok())
}

fn collect_modules_rec<T>(
    env: &SuiTestingEnv<T>,
    module: CompiledModule,
    visited: &mut BTreeSet<(String, String)>,
    ordered: &mut Vec<CompiledModule>,
) -> Result<(), MovyError>
where
    T: ObjectStore + ObjectStoreInfo,
{
    let module_id = module.self_id();
    let key = (
        module_id.address().to_string(),
        module_id.name().to_string(),
    );
    if !visited.insert(key) {
        return Ok(());
    }

    for dep in module.immediate_dependencies() {
        let dep_key = (dep.address().to_string(), dep.name().to_string());
        if visited.contains(&dep_key) {
            continue;
        }
        if let Some(dep_module) = fetch_compiled_module(env, &dep)? {
            collect_modules_rec(env, dep_module, visited, ordered)?;
        }
    }

    ordered.push(module);
    Ok(())
}

fn collect_modules<T>(
    env: &SuiTestingEnv<T>,
    root: CompiledModule,
) -> Result<Vec<CompiledModule>, MovyError>
where
    T: ObjectStore + ObjectStoreInfo,
{
    let mut visited = BTreeSet::new();
    let mut ordered = Vec::new();
    collect_modules_rec(env, root, &mut visited, &mut ordered)?;
    Ok(ordered)
}

fn analyze_module<T>(
    env: &SuiTestingEnv<T>,
    compiled: CompiledModule,
    std_dependency: Option<CompiledModule>,
) -> Result<Option<ModuleAnalysis>, MovyError>
where
    T: ObjectStore + ObjectStoreInfo,
{
    let mut modules = collect_modules(env, compiled)?;
    if modules.is_empty() {
        return Ok(None);
    }

    if let Some(std_dep) = std_dependency {
        modules.insert(0, std_dep);
    }

    let root_module = modules.pop().unwrap();

    let (stackless, global_env) =
        match generate_stackless_bytecode_for_module(modules.iter(), &root_module) {
            Ok(m) => m,
            Err(e) => {
                log::debug!("Failed to generate stackless bytecode: {e}");
                return Ok(None);
            }
        };

    Ok(Some(ModuleAnalysis {
        compiled: root_module,
        stackless,
        global_env,
    }))
}

pub async fn load_target_modules<T>(
    env: &SuiTestingEnv<T>,
    target_packages: &Vec<MoveAddress>,
) -> Result<Vec<ModuleAnalysis>, MovyError>
where
    T: ObjectStore + ObjectStoreInfo + ObjectStoreCachedStore,
{
    let mut seen = BTreeSet::new();
    let mut analyses = Vec::new();

    let std_dependency = fetch_compiled_module(
        env,
        &ModuleId::new(AccountAddress::ONE, Identifier::new("vector").unwrap()),
    )?;

    for package_id in target_packages {
        let Some(package_meta) = env.inner().get_package_info(*package_id)? else {
            continue;
        };
        let Some(object) = env.inner().get_object(&ObjectID::from(*package_id)) else {
            continue;
        };
        let Some(package) = object.data.try_as_package() else {
            continue;
        };

        for module_name in package_meta
            .modules
            .iter()
            .map(|m| &m.module_id.module_name)
            .sorted()
        {
            if !seen.insert((package_id, module_name.clone())) {
                continue;
            }
            let Ok(compiled) =
                package.deserialize_module_by_str(module_name, &BinaryConfig::new_unpublishable())
            else {
                continue;
            };

            if let Some(analysis) = analyze_module(env, compiled, std_dependency.clone())? {
                analyses.push(analysis);
            }
        }
    }

    Ok(analyses)
}

pub fn get_def_bytecode(
    function: &FunctionInfo,
    temp: usize,
    code_offset: usize,
) -> Option<&Bytecode> {
    if temp >= function.def_attrid.len() {
        return None;
    }
    let defs = &function.def_attrid[temp];
    if defs.is_empty() {
        return None;
    }
    if defs.len() == 1 {
        return function.code.get(defs[0]);
    }
    let mut candidates = defs
        .iter()
        .filter(|idx| **idx < code_offset)
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort();
    if let Some(idx) = candidates.last() {
        return function.code.get(*idx);
    }
    function.code.get(defs[0])
}
