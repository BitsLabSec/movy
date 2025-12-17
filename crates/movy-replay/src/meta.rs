use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use crate::{
    db::{ObjectStoreCachedStore, ObjectStoreInfo},
    env::SuiTestingEnv,
};
use movy_analysis::type_graph::MoveTypeGraph;
use movy_sui::database::cache::ObjectSuiStoreCommit;
use movy_types::{
    abi::{MoveAbility, MovePackageAbi},
    error::MovyError,
    input::{FunctionIdent, MoveAddress, MoveStructTag, MoveTypeTag},
};
use serde::{Deserialize, Serialize};
use serde_json_any_key::*;
use sui_types::storage::{BackingPackageStore, BackingStore, ObjectStore};

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
}

pub struct TargetPackages {
    pub addresses: Vec<MoveAddress>,
    pub local_paths: BTreeMap<MoveAddress, PathBuf>,
}

impl Metadata {
    pub fn get_package_metadata(&self, package_id: &MoveAddress) -> Option<&MovePackageAbi> {
        self.abis.get(
            self.module_address_to_package
                .get(package_id)
                .unwrap_or(package_id),
        )
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
