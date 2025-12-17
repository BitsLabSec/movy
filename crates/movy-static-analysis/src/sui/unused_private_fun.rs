use std::collections::BTreeSet;

use move_binary_format::{
    binary_config::BinaryConfig,
    file_format::{Bytecode, Visibility},
};
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

        let modules = package_meta
            .modules
            .iter()
            .map(|m| m.module_id.module_name.clone())
            .collect::<Vec<_>>();

        let mut unused_friend_functions = modules
            .iter()
            .flat_map(|m| {
                let Ok(module_data) =
                    pkg_data.deserialize_module_by_str(m, &BinaryConfig::new_unpublishable())
                else {
                    return BTreeSet::new();
                };
                module_data
                    .function_defs()
                    .iter()
                    .filter(|f| matches!(f.visibility, Visibility::Friend) && !f.is_entry)
                    .map(|f| {
                        (
                            module_data.self_id(),
                            module_data
                                .identifier_at(module_data.function_handle_at(f.function).name)
                                .to_string(),
                        )
                    })
                    .collect::<BTreeSet<_>>()
            })
            .collect::<BTreeSet<_>>();

        for module in modules {
            let Ok(module_data) =
                pkg_data.deserialize_module_by_str(&module, &BinaryConfig::new_unpublishable())
            else {
                continue;
            };
            let mut unused_private_functions = module_data
                .function_defs()
                .iter()
                .filter(|f| {
                    matches!(f.visibility, Visibility::Private)
                        && module_data
                            .identifier_at(module_data.function_handle_at(f.function).name)
                            .as_str()
                            != "init"
                        && !f.is_entry
                })
                .map(|f| {
                    (
                        module_data.self_id(),
                        module_data
                            .identifier_at(module_data.function_handle_at(f.function).name)
                            .to_string(),
                    )
                })
                .collect::<BTreeSet<_>>();
            for function in module_data.function_defs() {
                let Some(code) = &function.code else {
                    continue;
                };
                for bytecode in &code.code {
                    match *bytecode {
                        Bytecode::Call(idx) => {
                            let function_handle = module_data.function_handle_at(idx);
                            let function_name = module_data.identifier_at(function_handle.name);
                            let module_id = module_data.module_id_for_handle(
                                module_data.module_handle_at(function_handle.module),
                            );
                            unused_private_functions
                                .remove(&(module_id.clone(), function_name.to_string()));
                            unused_friend_functions.remove(&(module_id, function_name.to_string()));
                        }
                        Bytecode::CallGeneric(idx) => {
                            let inst_idx = module_data.function_instantiation_at(idx).handle;
                            let function_handle = module_data.function_handle_at(inst_idx);
                            let function_name = module_data.identifier_at(function_handle.name);
                            let module_id = module_data.module_id_for_handle(
                                module_data.module_handle_at(function_handle.module),
                            );
                            unused_private_functions
                                .remove(&(module_id.clone(), function_name.to_string()));
                            unused_friend_functions.remove(&(module_id, function_name.to_string()));
                        }
                        _ => {}
                    }
                }
            }
            if !unused_private_functions.is_empty() {
                let unused = unused_private_functions
                    .iter()
                    .map(|(module_id, func)| format!("{module_id}::{func}"))
                    .collect::<Vec<_>>();
                reports.push(OracleFinding {
                    oracle: "StaticUnusedPrivateFunction".to_string(),
                    severity: Severity::Informational,
                    extra: json!({
                        "package": pkg.to_string(),
                        "module": module.to_string(),
                        "functions": unused,
                        "message": "Private functions are never invoked"
                    }),
                });
            }
        }
        if !unused_friend_functions.is_empty() {
            let unused = unused_friend_functions
                .iter()
                .map(|(module_id, func)| format!("{module_id}::{func}"))
                .collect::<Vec<_>>();
            reports.push(OracleFinding {
                oracle: "StaticUnusedFriendFunction".to_string(),
                severity: Severity::Informational,
                extra: json!({
                    "package": pkg.to_string(),
                    "functions": unused,
                    "message": "Friend functions are never invoked"
                }),
            });
        }
    }

    Ok(reports)
}
