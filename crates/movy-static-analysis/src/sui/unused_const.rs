use color_eyre::eyre::eyre;
use move_binary_format::{
    binary_config::BinaryConfig, file_format::Bytecode, internals::ModuleIndex,
};
use move_core_types::runtime_value::MoveValue;
use movy_replay::{
    db::{ObjectStoreCachedStore, ObjectStoreInfo},
    env::SuiTestingEnv,
};
use movy_types::oracle::{OracleFinding, Severity};
use movy_types::{error::MovyError, input::MoveAddress};
use serde_json::json;
use sui_types::base_types::ObjectID;

use sui_types::storage::ObjectStore;

pub async fn analyze<T>(
    env: &SuiTestingEnv<T>,
    target_packages: &Vec<MoveAddress>,
) -> Result<Vec<OracleFinding>, MovyError>
where
    T: ObjectStore + ObjectStoreInfo + ObjectStoreCachedStore,
{
    let mut reports = Vec::new();

    for pkg in target_packages {
        let Some(pkg_obj) = env.inner().get_object(&ObjectID::from(*pkg)) else {
            continue;
        };
        let Some(package_meta) = env.inner().get_package_info(*pkg)? else {
            continue;
        };
        let Some(pkg_data) = pkg_obj.data.try_as_package() else {
            continue;
        };
        for module in package_meta.modules.iter() {
            let Ok(module_data) = pkg_data.deserialize_module_by_str(
                &module.module_id.module_name,
                &BinaryConfig::new_unpublishable(),
            ) else {
                continue;
            };
            let const_pool = &module_data.constant_pool;
            let len = const_pool.len();
            let mut is_visited = vec![false; len];
            for function in module_data.function_defs() {
                if let Some(codes) = &function.code {
                    for code in codes.code.iter() {
                        if let Bytecode::LdConst(idx) = code {
                            is_visited[idx.into_index()] = true;
                        }
                    }
                }
            }
            let mut unused_value: Vec<MoveValue> = vec![];
            for (id, visited) in is_visited.into_iter().enumerate() {
                if !visited {
                    let constant = &const_pool[id];
                    let value = constant
                        .deserialize_constant()
                        .ok_or_else(|| MovyError::Other(eyre!("failed to deserialize constant")))?;
                    unused_value.push(value);
                }
            }
            if !unused_value.is_empty() {
                let unused_constants = unused_value
                    .iter()
                    .map(|v| format!("{v:?}"))
                    .collect::<Vec<_>>();
                reports.push(OracleFinding {
                    oracle: "StaticUnusedConstant".to_string(),
                    severity: Severity::Informational,
                    extra: json!({
                        "package": pkg.to_string(),
                        "module": module.module_id.module_name.to_string(),
                        "unused_constants": unused_constants,
                        "message": "Constants are defined but never referenced"
                    }),
                });
            }
        }
    }

    Ok(reports)
}
