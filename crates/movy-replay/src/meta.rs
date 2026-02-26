use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use crate::{
    db::{ObjectStoreCachedStore, ObjectStoreInfo},
    env::SuiTestingEnv,
};
use color_eyre::eyre::eyre;
use move_core_types::{
    annotated_value::{MoveDatatypeLayout, MoveStruct, MoveValue},
    language_storage::StructTag,
};
use movy_analysis::type_graph::MoveTypeGraph;
use movy_sui::database::cache::ObjectSuiStoreCommit;
use movy_types::{
    abi::{
        MoveAbiSignatureToken, MoveAbility, MoveFunctionAbi, MoveModuleAbi, MoveModuleId,
        MovePackageAbi, MoveStructAbi,
    },
    error::MovyError,
    input::{FunctionIdent, MoveAddress, MoveStructTag, MoveTypeTag},
};
use serde::{Deserialize, Serialize};
use serde_json_any_key::*;
use sui_json_rpc_types::type_and_fields_from_move_event_data;
use sui_types::{
    event::Event,
    storage::{BackingPackageStore, BackingStore, ObjectStore},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metadata {
    pub type_graph: MoveTypeGraph,
    pub abis: BTreeMap<MoveAddress, MovePackageAbi>,
    pub testing_abis: BTreeMap<MoveAddress, MovePackageAbi>,
    #[serde(with = "any_key_map")]
    pub types_pool: BTreeMap<MoveTypeTag, BTreeSet<MoveAddress>>,
    pub module_address_to_package: BTreeMap<MoveAddress, MoveAddress>,
    pub ability_to_type_tag: BTreeMap<MoveAbility, Vec<MoveTypeTag>>,
    pub function_name_to_idents: BTreeMap<String, Vec<FunctionIdent>>,
    #[serde(with = "any_key_map")]
    pub structs_mapping: BTreeMap<(MoveModuleId, String), MoveStructAbi>,
}

pub struct TargetPackages {
    pub addresses: Vec<MoveAddress>,
    pub local_paths: BTreeMap<MoveAddress, PathBuf>,
}

impl Metadata {
    pub fn iter_functions(
        &self,
    ) -> impl Iterator<
        Item = (
            &MoveAddress,     // package_addr
            &String,          // module_name
            &MoveModuleAbi,   // module_data
            &String,          // func_name
            &MoveFunctionAbi, // func_data
        ),
    > {
        self.abis.iter().flat_map(|(package_addr, package_meta)| {
            package_meta.modules.iter().flat_map(move |module_data| {
                module_data.functions.iter().map(move |func_data| {
                    (
                        package_addr,
                        &module_data.module_id.module_name,
                        module_data,
                        &func_data.name,
                        func_data,
                    )
                })
            })
        })
    }

    pub fn get_package_metadata(&self, package_id: &MoveAddress) -> Option<&MovePackageAbi> {
        self.testing_abis.get(
            self.module_address_to_package
                .get(package_id)
                .unwrap_or(package_id),
        )
    }

    pub fn get_original_package_metadata(
        &self,
        package_id: &MoveAddress,
    ) -> Option<&MovePackageAbi> {
        self.abis.get(
            self.module_address_to_package
                .get(package_id)
                .unwrap_or(package_id),
        )
    }

    pub fn get_function(
        &self,
        package_id: &MoveAddress,
        module: &str,
        function: &str,
    ) -> Option<&MoveFunctionAbi> {
        self.get_package_metadata(package_id)
            .and_then(|pkg| {
                pkg.modules
                    .iter()
                    .find(|m| m.module_id.module_name == module)
            })
            .and_then(|module| module.functions.iter().find(|f| f.name == function))
    }

    pub fn get_struct(
        &self,
        package_id: &MoveAddress,
        module: &str,
        struct_name: &str,
    ) -> Option<&MoveStructAbi> {
        self.get_package_metadata(package_id)
            .and_then(|pkg| {
                pkg.modules
                    .iter()
                    .find(|m| m.module_id.module_name == module)
            })
            .and_then(|module| module.structs.iter().find(|s| s.struct_name == struct_name))
    }

    pub fn get_enum(
        &self,
        package_id: &MoveAddress,
        module: &str,
        enum_name: &str,
    ) -> Option<&MoveStructAbi> {
        self.get_package_metadata(package_id)
            .and_then(|pkg| {
                pkg.modules
                    .iter()
                    .find(|m| m.module_id.module_name == module)
            })
            .and_then(|module| module.structs.iter().find(|s| s.struct_name == enum_name))
    }

    pub fn get_abilities(
        &self,
        package_id: &MoveAddress,
        module: &str,
        struct_name: &str,
    ) -> Option<MoveAbility> {
        self.get_struct(package_id, module, struct_name)
            .map(|s| s.abilities)
            .or(self
                .get_enum(package_id, module, struct_name)
                .map(|e| e.abilities))
    }

    pub fn decode_sui_event(
        &self,
        event: &Event,
    ) -> Result<Option<(StructTag, serde_json::Value)>, MovyError> {
        log::debug!("Decoding event {}", event.type_.to_canonical_string(true));
        let id: MoveAddress = event.type_.address.into();
        if let Some(st) =
            self.get_struct(&id, event.type_.module.as_str(), event.type_.name.as_str())
        {
            let mut typs = vec![];
            for ty in event.type_.type_params.iter() {
                let ty = MoveTypeTag::from(ty.clone());
                let abi_ty = MoveAbiSignatureToken::from_type_tag_lossy(&ty);
                if let Some(typ) = abi_ty.to_move_type_layout(&[], &self.structs_mapping) {
                    typs.push(typ);
                } else {
                    log::debug!("decode_event: abi_ty {} is mising", &abi_ty);
                }
            }
            if let Some(layout) = st.to_move_struct_layout(&typs, &self.structs_mapping) {
                let e = Event::move_event_to_move_value(
                    &event.contents,
                    MoveDatatypeLayout::Struct(Box::new(layout)),
                )?;
                return Ok(Some(type_and_fields_from_move_event_data(e)?));
            } else {
                log::debug!(
                    "can not convert to move struct layout for {} and {:?}",
                    st.struct_name,
                    &typs
                );
            }
        } else {
            log::debug!("the event struct is not known");
        }

        Ok(None)
    }

    pub async fn from_env_filtered<T>(
        env: &SuiTestingEnv<T>,
        local_abis: BTreeMap<MoveAddress, MovePackageAbi>,
        include_types: Option<&[MoveTypeTag]>,
        exclude_types: Option<&[MoveTypeTag]>,
    ) -> Result<Self, MovyError>
    where
        T: ObjectStoreCachedStore
            + ObjectStoreInfo
            + ObjectStore
            + ObjectSuiStoreCommit
            + BackingStore
            + BackingPackageStore,
    {
        let testing_abis = env.export_abi().await?;
        let mut abis = testing_abis.clone();
        for (addr, abi) in local_abis {
            abis.insert(addr, abi);
        }
        abis.remove(&MoveAddress::from_str("0xa")?);

        // Map every module address back to its package id.
        let mut module_address_to_package = BTreeMap::new();
        for (pkg_id, abi) in &abis {
            for module in &abi.modules {
                if let Some(old_pkg_id) =
                    module_address_to_package.get(&module.module_id.module_address)
                {
                    if env.inner().get_version(*old_pkg_id)? < env.inner().get_version(*pkg_id)? {
                        module_address_to_package.insert(module.module_id.module_address, *pkg_id);
                    }
                } else {
                    module_address_to_package.insert(module.module_id.module_address, *pkg_id);
                }
            }
        }

        // Ability -> concrete, non-generic type tags.
        let mut ability_to_type_tag: BTreeMap<MoveAbility, BTreeSet<MoveTypeTag>> = BTreeMap::new();
        ability_to_type_tag.insert(
            MoveAbility::PRIMITIVES,
            BTreeSet::from([
                MoveTypeTag::Bool,
                MoveTypeTag::Address,
                MoveTypeTag::U8,
                MoveTypeTag::U16,
                MoveTypeTag::U32,
                MoveTypeTag::U64,
                MoveTypeTag::U128,
                MoveTypeTag::U256,
                MoveTypeTag::Vector(Box::new(MoveTypeTag::U8)),
            ]),
        );
        ability_to_type_tag.insert(MoveAbility::DROP, BTreeSet::from([MoveTypeTag::Signer]));

        for pkg in abis.values() {
            for module in &pkg.modules {
                for s in &module.structs {
                    if !s.type_parameters.is_empty() {
                        continue; // only consider monomorphic structs
                    }
                    let type_tag = MoveTypeTag::Struct(MoveStructTag {
                        address: s.module_id.module_address,
                        module: s.module_id.module_name.clone(),
                        name: s.struct_name.clone(),
                        tys: vec![],
                    });
                    ability_to_type_tag
                        .entry(s.abilities)
                        .or_default()
                        .insert(type_tag);
                }
            }
        }

        let mut function_name_to_idents: BTreeMap<String, Vec<FunctionIdent>> = BTreeMap::new();
        for pkg in testing_abis.values() {
            for module in &pkg.modules {
                for f in &module.functions {
                    let ident = FunctionIdent::new(
                        &module.module_id.module_address,
                        &module.module_id.module_name,
                        &f.name,
                    );
                    function_name_to_idents
                        .entry(f.name.clone())
                        .or_default()
                        .push(ident);
                }
            }
        }

        // Collect concrete object types currently present in the store.
        let mut types_pool: BTreeMap<MoveTypeTag, BTreeSet<MoveAddress>> = BTreeMap::new();
        for obj_id in env.inner().list_objects().await? {
            if let Ok(info) = env.inner().get_move_object_info(obj_id) {
                types_pool
                    .entry(info.ty.clone())
                    .or_default()
                    .insert(obj_id);
            }
        }
        let mut type_graph = MoveTypeGraph::default();
        for package in abis.values() {
            type_graph.add_package(package);
        }

        let mut structs_mapping = BTreeMap::new();
        for (pkg_id, pkg) in testing_abis.iter() {
            for md in pkg.modules.iter() {
                for st in md.structs.iter() {
                    structs_mapping
                        .insert((md.module_id.clone(), st.struct_name.clone()), st.clone());
                }
            }
        }

        let meta = Metadata {
            type_graph,
            abis,
            testing_abis,
            types_pool: filter_types_pool(types_pool, include_types, exclude_types),
            module_address_to_package,
            ability_to_type_tag: ability_to_type_tag
                .into_iter()
                .map(|(ability, tags)| (ability, tags.into_iter().collect()))
                .collect(),
            function_name_to_idents,
            structs_mapping,
        };
        Ok(meta)
    }

    pub async fn from_env<T>(
        env: &SuiTestingEnv<T>,
        local_abis: BTreeMap<MoveAddress, MovePackageAbi>,
    ) -> Result<Self, MovyError>
    where
        T: ObjectStoreCachedStore
            + ObjectStoreInfo
            + ObjectStore
            + ObjectSuiStoreCommit
            + BackingStore
            + BackingPackageStore,
    {
        Self::from_env_filtered(env, local_abis, None, None).await
    }
}

fn filter_types_pool(
    mut types_pool: BTreeMap<MoveTypeTag, BTreeSet<MoveAddress>>,
    include_types: Option<&[MoveTypeTag]>,
    exclude_types: Option<&[MoveTypeTag]>,
) -> BTreeMap<MoveTypeTag, BTreeSet<MoveAddress>> {
    if let Some(include) = include_types {
        let include_set: BTreeSet<_> = include.iter().cloned().collect();
        types_pool.retain(|ty, _| include_set.contains(ty));
    }

    if let Some(exclude) = exclude_types {
        let exclude_set: BTreeSet<_> = exclude.iter().cloned().collect();
        types_pool.retain(|ty, _| !exclude_set.contains(ty));
    }

    types_pool.retain(|_, ids| !ids.is_empty());
    types_pool
}
