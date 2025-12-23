use std::{
    collections::{BTreeMap, BTreeSet},
    ops::{Deref, DerefMut},
    str::FromStr,
};

use color_eyre::eyre::eyre;
use itertools::Itertools;
use libafl::{HasMetadata, state::HasRand};
use libafl_bolts::{impl_serdeany, rands::Rand};
use log::debug;
use movy_replay::{
    db::{ObjectStoreCachedStore, ObjectStoreInfo},
    env::SuiTestingEnv,
    meta::Metadata,
};
use movy_sui::database::cache::ObjectSuiStoreCommit;
use movy_types::abi::MoveAbiSignatureToken;
use movy_types::{
    abi::{
        MoveAbility, MoveFunctionAbi, MoveFunctionVisibility, MoveModuleAbi, MoveModuleId,
        MovePackageAbi, MoveStructAbi,
    },
    error::MovyError,
    input::{FunctionIdent, MoveAddress, MoveTypeTag},
};
use serde::{Deserialize, Serialize};
use serde_json_any_key::any_key_map;
use sui_types::storage::{BackingPackageStore, BackingStore, ObjectStore};

use crate::{r#const::INIT_FUNCTION_SCORE, utils::SuperRand};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectWithversion {
    pub id: MoveAddress,
    pub version: Option<u64>,
}

impl FromStr for ObjectWithversion {
    type Err = movy_types::error::MovyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<_> = s.split(':').collect();
        match parts.as_slice() {
            [id] => {
                let id = MoveAddress::from_str(id)
                    .map_err(|e| eyre!("can not parse id {} with {}", id, e))?;
                Ok(Self { id, version: None })
            }
            [id, version] => {
                let version = u64::from_str(version)
                    .map_err(|e| eyre!("can not parse version {} with {}", version, e))?;
                let id = MoveAddress::from_str(id)
                    .map_err(|e| eyre!("can not parse id {} with {}", id, e))?;
                Ok(Self {
                    id,
                    version: Some(version),
                })
            }
            _ => Err(eyre!("can not parse ObjectWithversion from {}", s).into()),
        }
    }
}

pub trait HasFuzzMetadata {
    fn fuzz_state_mut(&mut self) -> &mut FuzzMetadata;

    fn fuzz_state(&self) -> &FuzzMetadata;
}

impl<T: HasMetadata> HasFuzzMetadata for T {
    fn fuzz_state(&self) -> &FuzzMetadata {
        self.metadata().expect("meta not installed yet?")
    }

    fn fuzz_state_mut(&mut self) -> &mut FuzzMetadata {
        self.metadata_mut().expect("meta not installed yet?")
    }
}

pub trait HasCaller {
    /// Get a random address from the address set, used for ABI mutation
    fn get_rand_address(&mut self) -> MoveAddress;
    /// Get a random caller from the caller set, used for transaction sender
    /// mutation
    fn get_rand_caller(&mut self) -> MoveAddress;
    /// Does the address exist in the caller set
    fn has_caller(&self, addr: &MoveAddress) -> bool;
    /// Add a caller to the caller set
    fn add_caller(&mut self, caller: &MoveAddress);
    /// Add an address to the address set
    fn add_address(&mut self, caller: &MoveAddress);
}

impl<T: HasFuzzMetadata + HasRand> HasCaller for T {
    /// Get a random address from the address pool, used for ABI mutation
    fn get_rand_address(&mut self) -> MoveAddress {
        let length = self.fuzz_state().addresses_pool.len();
        let idx = self.rand_mut().below_or_zero(length);
        self.fuzz_state_mut().addresses_pool[idx]
    }

    /// Get a random caller from the caller pool, used for mutating the caller
    fn get_rand_caller(&mut self) -> MoveAddress {
        let length = self.fuzz_state().callers_pool.len();
        let idx = self.rand_mut().below_or_zero(length);
        self.fuzz_state_mut().callers_pool[idx]
    }

    /// Get a random caller from the caller pool, used for mutating the caller
    fn has_caller(&self, addr: &MoveAddress) -> bool {
        self.fuzz_state().callers_pool.contains(addr)
    }

    /// Add a caller to the caller pool
    fn add_caller(&mut self, addr: &MoveAddress) {
        let callers_pool = &mut self.fuzz_state_mut().callers_pool;
        if !callers_pool.contains(addr) {
            callers_pool.push(*addr);
        }
    }

    /// Add an address to the address pool
    fn add_address(&mut self, caller: &MoveAddress) {
        let addresses_pool = &mut self.fuzz_state_mut().addresses_pool;
        if !addresses_pool.contains(caller) {
            addresses_pool.push(*caller);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MutatorKind {
    Sequence,
    Arg,
    Magic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuzzFunctionScore {
    pub function: FunctionIdent,
    pub score: u64,
}

impl FromStr for FuzzFunctionScore {
    type Err = MovyError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let st = s.split("/").collect_vec();
        if st.len() != 2 {
            return Err(eyre!("expected usage: 0x2::coin::split/1000").into());
        }
        let score = u64::from_str(st[1]).map_err(|_| eyre!("can not parse score {}", st[1]))?;
        let function = FunctionIdent::from_str(st[0])?;
        Ok(Self { function, score })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetFilters {
    pub include_packages: Option<Vec<MoveAddress>>,
    pub exclude_packages: Option<Vec<MoveAddress>>,
    pub include_modules: Option<Vec<MoveModuleId>>,
    pub exclude_modules: Option<Vec<MoveModuleId>>,
    pub include_functions: Option<Vec<FunctionIdent>>,
    pub exclude_functions: Option<Vec<FunctionIdent>>,
    pub include_types: Option<Vec<MoveTypeTag>>,
    pub exclude_types: Option<Vec<MoveTypeTag>>,
}

fn normalize_packages(
    target_packages: Vec<MoveAddress>,
    filters: &TargetFilters,
) -> Vec<MoveAddress> {
    let mut packages = target_packages;

    if let Some(include) = &filters.include_packages {
        let include_set: BTreeSet<_> = include.iter().copied().collect();
        packages.retain(|pkg| include_set.contains(pkg));
    }

    if let Some(exclude) = &filters.exclude_packages {
        let exclude_set: BTreeSet<_> = exclude.iter().copied().collect();
        packages.retain(|pkg| !exclude_set.contains(pkg));
    }

    packages.sort();
    packages.dedup();
    packages
}

fn should_skip_function(base: &Metadata, func_data: &MoveFunctionAbi) -> bool {
    if func_data.visibility != MoveFunctionVisibility::Public {
        return true; // skip non-public functions
    }

    if func_data.parameters.iter().all(|t| {
        (t.ability().is_some_and(|a| a.contains(MoveAbility::DROP)))
            || matches!(t, MoveAbiSignatureToken::Reference(_))
            || t.is_tx_context()
    }) && func_data
        .return_paramters
        .iter()
        .all(|t| t.is_mutable() || t.ability().is_some_and(|a| a.contains(MoveAbility::DROP)))
    {
        // skip read-only functions
        return true;
    }

    for ret_ty in func_data.return_paramters.iter() {
        let self_used = func_data
            .parameters
            .iter()
            .any(|t| t == ret_ty || t.dereference().is_some_and(|r| r.as_ref() == ret_ty));
        let ret_ref = matches!(
            ret_ty,
            MoveAbiSignatureToken::Reference(_) | MoveAbiSignatureToken::MutableReference(_)
        );
        let hanging_hot_potato =
            ret_ty.is_hot_potato() && base.type_graph.find_consumers(ret_ty).is_empty();
        if self_used || ret_ref || hanging_hot_potato {
            return true;
        }
    }

    false
}

fn collect_target_functions(
    base: &Metadata,
    target_packages: &[MoveAddress],
) -> Vec<FunctionIdent> {
    let mut target_functions: Vec<FunctionIdent> = vec![];

    for package_addr in target_packages.iter() {
        let Some(package_meta) = base.get_package_metadata(package_addr) else {
            continue;
        };
        for module in package_meta.modules.iter() {
            for func_data in module.functions.iter() {
                if should_skip_function(base, func_data) {
                    continue;
                }
                let func_ident = FunctionIdent::new(
                    package_addr,
                    &module.module_id.module_name,
                    &func_data.name,
                );
                debug!("Re-analyzing function: {:?}", &func_ident);
                target_functions.push(func_ident);
            }
        }
    }

    target_functions
}

fn fallback_target_functions(
    base: &Metadata,
    target_packages: &[MoveAddress],
) -> Vec<FunctionIdent> {
    base.abis
        .iter()
        .filter(|(package_addr, _)| target_packages.contains(package_addr))
        .flat_map(|(package_addr, package_meta)| {
            package_meta.modules.iter().flat_map(move |module_data| {
                module_data.functions.iter().map(move |func_data| {
                    FunctionIdent::new(
                        package_addr,
                        &module_data.module_id.module_name,
                        &func_data.name,
                    )
                })
            })
        })
        .collect()
}

fn apply_function_filters(
    target_functions: Vec<FunctionIdent>,
    filters: &TargetFilters,
) -> Vec<FunctionIdent> {
    let include_pkgs = filters
        .include_packages
        .as_ref()
        .map(|v| v.iter().copied().collect::<BTreeSet<_>>());
    let exclude_pkgs = filters
        .exclude_packages
        .as_ref()
        .map(|v| v.iter().copied().collect::<BTreeSet<_>>());
    let include_modules = filters
        .include_modules
        .as_ref()
        .map(|v| v.iter().cloned().collect::<BTreeSet<_>>());
    let exclude_modules = filters
        .exclude_modules
        .as_ref()
        .map(|v| v.iter().cloned().collect::<BTreeSet<_>>());
    let include_funcs = filters
        .include_functions
        .as_ref()
        .map(|v| v.iter().cloned().collect::<BTreeSet<_>>());
    let exclude_funcs = filters
        .exclude_functions
        .as_ref()
        .map(|v| v.iter().cloned().collect::<BTreeSet<_>>());

    target_functions
        .into_iter()
        .filter(|func| {
            let pkg = func.0.module_address;
            let module_id = &func.0;
            if let Some(set) = &include_pkgs
                && !set.contains(&pkg)
            {
                return false;
            }
            if let Some(set) = &exclude_pkgs
                && set.contains(&pkg)
            {
                return false;
            }
            if let Some(set) = &include_funcs
                && !set.contains(func)
            {
                return false;
            }
            if let Some(set) = &exclude_funcs
                && set.contains(func)
            {
                return false;
            }
            if let Some(set) = &include_modules
                && !set.contains(module_id)
            {
                return false;
            }
            if let Some(set) = &exclude_modules
                && set.contains(module_id)
            {
                return false;
            }
            true
        })
        .collect()
}

fn derive_function_scores(
    target_functions: &[FunctionIdent],
    specific_function_scores: &BTreeMap<FunctionIdent, u64>,
) -> BTreeMap<FunctionIdent, u64> {
    let mut function_scores: BTreeMap<FunctionIdent, u64> = BTreeMap::new();
    for func_ident in target_functions.iter() {
        let score = specific_function_scores
            .get(func_ident)
            .copied()
            .unwrap_or(INIT_FUNCTION_SCORE);
        function_scores.entry(func_ident.clone()).or_insert(score);
    }
    function_scores
}

fn collect_target_packages_from_functions(functions: &[FunctionIdent]) -> Vec<MoveAddress> {
    functions
        .iter()
        .map(|f| f.0.module_address)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn derive_function_hooks(
    abis: &BTreeMap<MoveAddress, MovePackageAbi>,
    function_name_to_idents: &BTreeMap<String, Vec<FunctionIdent>>,
) -> BTreeMap<FunctionIdent, FunctionHook> {
    let mut function_hooks: BTreeMap<FunctionIdent, FunctionHook> = BTreeMap::new();

    for (package_addr, package_abi) in abis.iter() {
        for module in package_abi.modules.iter() {
            for func in module.functions.iter() {
                if !func.is_movy_pre_ptb()
                    && let Some(pre_func) = func.try_derive_movy_pre()
                {
                    let target_ident = if let Some(idents) = function_name_to_idents.get(pre_func) {
                        if idents.len() == 1 {
                            idents[0].clone()
                        } else {
                            idents
                                .iter()
                                .find(|ident| {
                                    ident.0.module_address == *package_addr
                                        && format!("movy_{}", ident.0.module_name)
                                            == module.module_id.module_name
                                })
                                .cloned()
                                .unwrap()
                        }
                    } else {
                        panic!("can not find movy_pre function {}", pre_func);
                    };
                    let func_ident =
                        FunctionIdent::new(package_addr, &module.module_id.module_name, &func.name);
                    function_hooks
                        .entry(target_ident)
                        .or_default()
                        .pre_hooks
                        .push(func_ident);
                }
                if !func.is_movy_post_ptb()
                    && let Some(post_func) = func.try_derive_movy_post()
                {
                    let target_ident = if let Some(idents) = function_name_to_idents.get(post_func)
                    {
                        if idents.len() == 1 {
                            idents[0].clone()
                        } else {
                            idents
                                .iter()
                                .find(|ident| {
                                    ident.0.module_address == *package_addr
                                        && ident.0.module_name == module.module_id.module_name
                                })
                                .cloned()
                                .unwrap()
                        }
                    } else {
                        panic!("can not find movy_post function {}", post_func);
                    };
                    let func_ident =
                        FunctionIdent::new(package_addr, &module.module_id.module_name, &func.name);
                    function_hooks
                        .entry(target_ident)
                        .or_default()
                        .post_hooks
                        .push(func_ident);
                }
            }
        }
    }

    function_hooks
}

fn derive_sequence_hooks(abis: &BTreeMap<MoveAddress, MovePackageAbi>) -> FunctionHook {
    let mut sequence_hooks = FunctionHook::default();

    for (package_addr, package_abi) in abis.iter() {
        for module in package_abi.modules.iter() {
            for func in module.functions.iter() {
                if func.is_movy_pre_ptb() {
                    sequence_hooks.pre_hooks.push(FunctionIdent::new(
                        package_addr,
                        &module.module_id.module_name,
                        &func.name,
                    ));
                }
                if func.is_movy_post_ptb() {
                    sequence_hooks.post_hooks.push(FunctionIdent::new(
                        package_addr,
                        &module.module_id.module_name,
                        &func.name,
                    ));
                }
            }
        }
    }

    sequence_hooks
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FunctionHook {
    pub pre_hooks: Vec<FunctionIdent>,
    pub post_hooks: Vec<FunctionIdent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuzzMetadata {
    pub base: Metadata,
    pub rand: SuperRand,

    pub attacker: MoveAddress,
    pub callers_pool: Vec<MoveAddress>,
    pub addresses_pool: Vec<MoveAddress>,

    #[serde(with = "any_key_map")]
    pub function_scores: BTreeMap<FunctionIdent, u64>,
    pub current_mutator: Option<MutatorKind>,
    #[serde(with = "any_key_map")]
    pub specific_function_scores: BTreeMap<FunctionIdent, u64>,

    pub target_functions: Vec<FunctionIdent>,
    pub target_packages: Vec<MoveAddress>,

    #[serde(with = "any_key_map")]
    pub function_hooks: BTreeMap<FunctionIdent, FunctionHook>,
    pub sequence_hooks: FunctionHook,

    pub gas_id: MoveAddress,
    pub checkpoint: u64,
    pub epoch: u64,
    pub epoch_ms: u64,
}

impl Deref for FuzzMetadata {
    type Target = Metadata;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl DerefMut for FuzzMetadata {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.base
    }
}

impl_serdeany!(FuzzMetadata);

impl FuzzMetadata {
    pub async fn from_env<T>(
        env: &SuiTestingEnv<T>,
        rand: SuperRand,
        function_scores: Vec<FuzzFunctionScore>,
        target_packages: Vec<MoveAddress>,
        attacker: MoveAddress,
        admin: MoveAddress,
        gas_id: MoveAddress,
        local_abis: BTreeMap<MoveAddress, MovePackageAbi>,
        local_testing_abis: BTreeMap<MoveAddress, MovePackageAbi>,
        checkpoint: u64,
        epoch: u64,
        epoch_ms: u64,
        filters: TargetFilters,
    ) -> Result<Self, MovyError>
    where
        T: ObjectStoreCachedStore
            + ObjectStoreInfo
            + ObjectStore
            + ObjectSuiStoreCommit
            + BackingStore
            + BackingPackageStore,
    {
        let base = Metadata::from_env_filtered(
            env,
            local_abis,
            filters.include_types.as_deref(),
            filters.exclude_types.as_deref(),
        )
        .await?;
        Ok(Self::from_metadata(
            base,
            rand,
            function_scores,
            target_packages,
            attacker,
            admin,
            gas_id,
            local_testing_abis,
            checkpoint,
            epoch,
            epoch_ms,
            filters,
        ))
    }

    pub fn from_metadata(
        base: Metadata,
        rand: SuperRand,
        function_scores: Vec<FuzzFunctionScore>,
        target_packages: Vec<MoveAddress>,
        attacker: MoveAddress,
        admin: MoveAddress,
        gas_id: MoveAddress,
        local_testing_abis: BTreeMap<MoveAddress, MovePackageAbi>,
        checkpoint: u64,
        epoch: u64,
        epoch_ms: u64,
        filters: TargetFilters,
    ) -> Self {
        let specific_function_scores: BTreeMap<FunctionIdent, u64> = function_scores
            .into_iter()
            .map(|f| (f.function, f.score))
            .collect();

        let filtered_packages = normalize_packages(target_packages, &filters);
        let mut target_functions = collect_target_functions(&base, &filtered_packages);
        target_functions = apply_function_filters(target_functions, &filters);
        if target_functions.is_empty() {
            // if no all functions excluded, use all functions
            target_functions = fallback_target_functions(&base, &filtered_packages);
            target_functions = apply_function_filters(target_functions, &filters);
        }

        let function_scores = derive_function_scores(&target_functions, &specific_function_scores);
        let target_packages = collect_target_packages_from_functions(&target_functions);

        let function_hooks =
            derive_function_hooks(&local_testing_abis, &base.function_name_to_idents);

        let sequence_hooks = derive_sequence_hooks(&local_testing_abis);

        Self {
            base,
            rand,
            attacker,
            callers_pool: vec![attacker],
            addresses_pool: vec![attacker, admin],
            function_scores,
            current_mutator: None,
            specific_function_scores,
            target_functions,
            target_packages,
            function_hooks,
            sequence_hooks,
            gas_id,
            checkpoint,
            epoch,
            epoch_ms,
        }
    }

    pub fn iter_target_functions(
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
        self.target_functions
            .iter()
            .filter_map(move |FunctionIdent(module_id, func_name)| {
                let package_addr = &module_id.module_address;
                let module_name = &module_id.module_name;
                self.get_function(package_addr, module_name, func_name)
                    .map(|func_data| {
                        let package_meta = self.get_package_metadata(package_addr).unwrap();
                        let module_data = package_meta
                            .modules
                            .iter()
                            .find(|m| &m.module_id == module_id)
                            .unwrap();
                        (package_addr, module_name, module_data, func_name, func_data)
                    })
            })
    }

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

    pub fn generate_magic_number_pool(&self) -> BTreeSet<Vec<u8>> {
        BTreeSet::new()
    }
}
