//! Build a full per-repo static-analysis snapshot from a compiled
//! Sui Move package: the [`knowdit_repo_model::cg::CallGraph`]
//! (modules / functions / call edges) plus the
//! [`MovePackageStructure`] (struct definitions + per-function
//! metadata). One walk over `package.all_modules_iter()` produces
//! both; the call-graph extractor is the existing
//! [`crate::export_call_graph::load_call_graph`] reused verbatim
//! so other consumers that only want CG keep working.
//!
//! All access goes through movy's wrapper types
//! ([`MoveModuleAbi`] / [`MoveStructAbi`] / [`MoveFunctionAbi`])
//! rather than raw `move_binary_format` entry points — keeps us
//! insulated from the upstream Sui fork's API churn.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use knowdit_repo_model::cg::CallGraph;
use knowdit_repo_model::move_lang::{
    MoveAbility, MoveField, MoveFunctionMetadata, MoveGenericParam, MovePackageStructure,
    MoveStruct, MoveVisibility,
};
use movy_sui::compile::{SuiCompiledPackage, resolve_local_dependency_paths};
use movy_types::abi::{
    MoveAbility as MovyAbility, MoveFunctionVisibility, MoveModuleAbi, MoveStructAbi,
};
use movy_types::error::MovyError;

use crate::export_call_graph::load_call_graph;

/// Full per-repo static-analysis snapshot. Holds the call graph
/// (modules + functions + call edges) plus the Move-specific
/// package structure (struct definitions, per-function metadata).
/// Persisted in one shot via
/// [`knowdit_repo_model::RepoDatabase::write_repo_info`].
///
/// Built via [`Self::from_package_root`]. The
/// `(module_id, struct_name) → struct.id` index is preserved
/// alongside in case future static-analysis passes (added under
/// the `export-repo-info` umbrella) need to cross-reference
/// structs by name.
pub struct PackageWalk {
    pub call_graph: CallGraph,
    pub structure: MovePackageStructure,
    pub struct_id_lookup: BTreeMap<(String, String), i32>,
}

impl PackageWalk {
    /// Walk the Move package at `package_root` end-to-end:
    ///
    /// 1. Run the bytecode call-graph extractor (shared with
    ///    [`crate::export_call_graph::load_call_graph`]) to get a
    ///    [`CallGraph`] with assigned `contract.id` /
    ///    `function.id` values.
    /// 2. Walk the package again and collect struct definitions +
    ///    per-function Move metadata, using the in-memory
    ///    [`NameLookup`] derived from the CG so the row IDs
    ///    match up without a DB round-trip.
    ///
    /// Sui's package build cache makes the second compile
    /// essentially free.
    pub fn from_package_root(package_root: &Path, test_mode: bool) -> Result<Self, MovyError> {
        let call_graph = load_call_graph(package_root, test_mode)?;
        let name_lookup = NameLookup::from_call_graph(&call_graph);
        let (structure, struct_id_lookup) =
            Self::collect_package_structure(package_root, test_mode, &name_lookup)?;
        Ok(Self {
            call_graph,
            structure,
            struct_id_lookup,
        })
    }

    /// Single-pass walk over the compiled package that collects every
    /// struct definition + per-function metadata row, looking up the
    /// CG-assigned `contract.id` / `function.id` via `name_lookup`.
    /// Returned together with the `(module_id, struct_name) →
    /// struct.id` index so downstream passes can resolve struct
    /// references by name.
    fn collect_package_structure(
        package_root: &Path,
        test_mode: bool,
        name_lookup: &NameLookup,
    ) -> Result<(MovePackageStructure, BTreeMap<(String, String), i32>), MovyError> {
        let package_root = canonical_package_root(package_root)?;
        let search_roots = resolve_search_roots(&package_root, test_mode)?;

        let mut seen_modules: BTreeSet<String> = BTreeSet::new();
        let mut next_struct_id: i32 = 1;
        let mut structs: Vec<MoveStruct> = Vec::new();
        let mut struct_id_lookup: BTreeMap<(String, String), i32> = BTreeMap::new();
        let mut function_metadata: Vec<MoveFunctionMetadata> = Vec::new();

        for root in &search_roots {
            let package = SuiCompiledPackage::build_checked(
                root,
                test_mode,
                false,
                true,
                &movy_sui::compile::BuildIsolation::default(),
            )?;
            for module in package.all_modules_iter() {
                let abi = MoveModuleAbi::from_sui_module(module);
                if abi.module_id.module_address.is_sui_std() {
                    continue;
                }
                let module_id = abi.module_id.canonical_short();
                if !seen_modules.insert(module_id.clone()) {
                    continue;
                }
                let contract_id = name_lookup.contract_id(&module_id).ok_or_else(|| {
                    MovyError::Any(anyhow!(
                        "module {module_id} has no row in `contract` — the \
                         in-memory call graph passed in via `NameLookup` is \
                         missing this module. This indicates an analyzer bug; \
                         `PackageWalk::from_package_root` should have produced \
                         the CG and populated the lookup in a single walk."
                    ))
                })?;

                for struct_abi in &abi.structs {
                    let id = next_struct_id;
                    next_struct_id += 1;
                    struct_id_lookup.insert(
                        (module_id.clone(), struct_abi.handle.struct_name.clone()),
                        id,
                    );
                    structs.push(struct_from_abi(id, contract_id, struct_abi));
                }

                for function_abi in &abi.functions {
                    let Some(function_id) = name_lookup.function_id(&module_id, &function_abi.name)
                    else {
                        // CG didn't track this function (test-only,
                        // generated stub, etc.). Skip — the audit
                        // pipeline only needs metadata for functions
                        // we already have a `function` row for.
                        tracing::debug!(
                            module_id = %module_id,
                            function = %function_abi.name,
                            "skipping function metadata: no matching `function` row"
                        );
                        continue;
                    };
                    function_metadata.push(MoveFunctionMetadata {
                        function_id,
                        visibility: visibility_to_knowdit(&function_abi.visibility),
                        is_entry: function_abi.is_entry,
                        generic_params: function_abi
                            .type_parameters
                            .iter()
                            .enumerate()
                            .map(|(i, ability)| MoveGenericParam {
                                name: format!("T{i}"),
                                constraints: movy_ability_to_vec(*ability),
                                // FunctionHandle generic params don't
                                // carry phantom (only struct generics
                                // can be phantom in Move), so always
                                // false here.
                                phantom: false,
                            })
                            .collect(),
                    });
                }
            }
        }

        Ok((
            MovePackageStructure {
                function_metadata,
                structs,
            },
            struct_id_lookup,
        ))
    }
}

/// Lookup table that maps the canonical Move identifiers movy uses
/// at compile time (`0xADDR::module_name`, function name) to the
/// row IDs the CG writer committed. Built by
/// [`Self::from_repo_database`] in one [`RepoDatabase::load_call_graph`]
/// call.
pub struct NameLookup {
    contract_ids: BTreeMap<String, i32>,
    function_ids: BTreeMap<(String, String), i32>,
}

impl NameLookup {
    /// Build the lookup straight from an in-memory [`CallGraph`].
    /// No DB round-trip — the CG already carries every assigned
    /// `contract.id` / `function.id` we need.
    pub fn from_call_graph(cg: &CallGraph) -> Self {
        let mut contract_ids = BTreeMap::new();
        let mut function_ids = BTreeMap::new();
        for contract in cg.contracts.values() {
            contract_ids.insert(contract.name.clone(), contract.id);
            for function in &contract.functions {
                function_ids.insert((contract.name.clone(), function.name.clone()), function.id);
            }
        }
        for iface in cg.interfaces.values() {
            contract_ids.insert(iface.name.clone(), iface.id);
            for function in &iface.functions {
                function_ids.insert((iface.name.clone(), function.name.clone()), function.id);
            }
        }
        Self {
            contract_ids,
            function_ids,
        }
    }

    /// Same lookup, built off whatever CG snapshot is currently
    /// in the project DB. Reserved for callers that don't have a
    /// fresh in-memory CG handy. The normal path is
    /// [`Self::from_call_graph`] inside
    /// [`super::PackageWalk::from_package_root`].
    pub async fn from_repo_database(
        repo: &knowdit_repo_model::RepoDatabase,
    ) -> Result<Self, MovyError> {
        let cg = repo.load_call_graph().await?;
        Ok(Self::from_call_graph(&cg))
    }

    pub fn contract_id(&self, module_id: &str) -> Option<i32> {
        self.contract_ids.get(module_id).copied()
    }

    pub fn function_id(&self, module_id: &str, function_name: &str) -> Option<i32> {
        self.function_ids
            .get(&(module_id.to_string(), function_name.to_string()))
            .copied()
    }
}

// -----------------------------------------------------------------
// Shared helpers reused by the object-flow analyzer.
// -----------------------------------------------------------------

pub(crate) fn canonical_package_root(package_root: &Path) -> Result<PathBuf, MovyError> {
    let package_root = package_root
        .canonicalize()
        .unwrap_or_else(|_| package_root.to_path_buf());
    if !package_root.is_dir() {
        return Err(MovyError::Any(anyhow!(
            "Move package root {} is not a directory",
            package_root.display()
        )));
    }
    if !package_root.join("Move.toml").is_file() {
        return Err(MovyError::Any(anyhow!(
            "Move package root {} does not contain Move.toml",
            package_root.display()
        )));
    }
    Ok(package_root)
}

pub(crate) fn resolve_search_roots(
    package_root: &Path,
    test_mode: bool,
) -> Result<Vec<PathBuf>, MovyError> {
    let mut roots = vec![package_root.to_path_buf()];
    for root in resolve_local_dependency_paths(&[package_root.to_path_buf()], test_mode)? {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    Ok(roots)
}

// -----------------------------------------------------------------
// Internal builders.
// -----------------------------------------------------------------

fn struct_from_abi(id: i32, contract_id: i32, abi: &MoveStructAbi) -> MoveStruct {
    MoveStruct {
        id,
        contract_id,
        name: abi.handle.struct_name.clone(),
        abilities: movy_ability_to_vec(abi.handle.abilities),
        generic_params: abi
            .handle
            .type_parameters
            .iter()
            .enumerate()
            .map(|(i, ty_param)| MoveGenericParam {
                name: format!("T{i}"),
                constraints: movy_ability_to_vec(ty_param.constraints),
                phantom: ty_param.phantom,
            })
            .collect(),
        fields: abi
            .fields
            .iter()
            .map(|field| MoveField {
                name: field.name.clone(),
                type_repr: field.ty.to_string(),
            })
            .collect(),
    }
}

fn movy_ability_to_vec(ability: MovyAbility) -> Vec<MoveAbility> {
    let mut out = Vec::new();
    if ability.contains(MovyAbility::COPY) {
        out.push(MoveAbility::Copy);
    }
    if ability.contains(MovyAbility::DROP) {
        out.push(MoveAbility::Drop);
    }
    if ability.contains(MovyAbility::STORE) {
        out.push(MoveAbility::Store);
    }
    if ability.contains(MovyAbility::KEY) {
        out.push(MoveAbility::Key);
    }
    out
}

fn visibility_to_knowdit(v: &MoveFunctionVisibility) -> MoveVisibility {
    match v {
        MoveFunctionVisibility::Public => MoveVisibility::Public,
        MoveFunctionVisibility::Private => MoveVisibility::Private,
        // The Move binary format collapses Sui 2024's `public(package)`
        // and legacy `public(friend)` into a single `Friend` enum; we
        // map both to the modern `PublicPackage` label since that's
        // what new code uses. Legacy `public(friend)` projects take
        // the precision loss noted in plan_move_lang.md §10.
        MoveFunctionVisibility::Friend => MoveVisibility::PublicPackage,
    }
}
