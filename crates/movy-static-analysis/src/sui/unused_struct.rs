use move_binary_format::{
    binary_config::BinaryConfig,
    file_format::{Bytecode, SignatureToken},
    internals::ModuleIndex,
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
        for module in package_meta.modules.iter() {
            let Ok(module_data) = pkg_data.deserialize_module_by_str(
                &module.module_id.module_name,
                &BinaryConfig::new_unpublishable(),
            ) else {
                continue;
            };
            let struct_pool = module_data.struct_defs();
            let enum_pool = module_data.enum_defs();
            let mut struct_is_visited = vec![false; struct_pool.len()];
            let mut enum_is_visited = vec![false; enum_pool.len()];
            for function in module_data.function_defs() {
                // function parameters
                let function_handle = module_data.function_handle_at(function.function);
                let parameters = module_data.signature_at(function_handle.parameters);
                parameters.0.iter().for_each(|sig| {
                    if let SignatureToken::Datatype(idx) = sig.clone() {
                        let struct_idx = struct_pool.iter().position(|s| s.struct_handle == idx);
                        if let Some(sid) = struct_idx {
                            struct_is_visited[sid] = true;
                        }
                    }
                });

                if let Some(codes) = &function.code {
                    for code in codes.code.iter() {
                        match *code {
                            Bytecode::Pack(idx) => {
                                struct_is_visited[idx.into_index()] = true;
                            }
                            Bytecode::PackGeneric(idx) => {
                                struct_is_visited
                                    [module_data.struct_instantiation_at(idx).def.into_index()] =
                                    true;
                            }
                            Bytecode::PackVariant(idx) => {
                                enum_is_visited
                                    [module_data.variant_handle_at(idx).enum_def.into_index()] =
                                    true;
                            }
                            Bytecode::PackVariantGeneric(idx) => {
                                enum_is_visited[module_data
                                    .variant_instantiation_handle_at(idx)
                                    .enum_def
                                    .into_index()] = true;
                            }
                            _ => {}
                        }
                    }
                }
            }
            let unused_struct = struct_is_visited
                .iter()
                .enumerate()
                .filter_map(|(id, visited)| if !visited { Some(id) } else { None })
                .collect::<Vec<_>>();
            let unused_enum = enum_is_visited
                .iter()
                .enumerate()
                .filter_map(|(id, visited)| if !visited { Some(id) } else { None })
                .collect::<Vec<_>>();
            if !unused_struct.is_empty() {
                reports.push(OracleFinding {
                    oracle: "StaticUnusedStruct".to_string(),
                    severity: Severity::Informational,
                    extra: json!({
                        "package": pkg.to_string(),
                        "module": module.module_id.module_name.to_string(),
                        "struct_indices": unused_struct,
                        "message": "Structs are defined but never used"
                    }),
                });
            }
            if !unused_enum.is_empty() {
                reports.push(OracleFinding {
                    oracle: "StaticUnusedEnum".to_string(),
                    severity: Severity::Informational,
                    extra: json!({
                        "package": pkg.to_string(),
                        "module": module.module_id.module_name.to_string(),
                        "enum_indices": unused_enum,
                        "message": "Enums are defined but never used"
                    }),
                });
            }
        }
    }

    Ok(reports)
}
