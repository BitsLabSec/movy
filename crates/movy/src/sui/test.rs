use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use clap::Args;
use color_eyre::eyre::eyre;
use movy_fuzz::operations::sui_test;
use movy_replay::db::{ObjectStoreCachedStore, ObjectStoreInfo};
use movy_sui::{lcov::LineCoverageMap, rpc::grpc::SuiGrpcArg};
use movy_types::{
    abi::MoveFunctionAbi,
    error::MovyError,
    input::{FunctionIdent, MoveAddress, MoveTypeTag},
    object::MoveOwner,
};
use serde::{Deserialize, Serialize};

use crate::sui::{
    env::{FunctionSelector, FuzzTargetArgs, SuiTargetArgs},
    prepare::{PreparedStore, prepare_fuzz_context, resolve_type_tag},
    utils::{MovyInitRoles, RngSeed, SuiOnchainArguments},
};
use movy_replay::env::SuiTestingEnv;

#[derive(Args, Clone, Debug, Serialize, Deserialize)]
pub struct SuiTestArgs {
    #[clap(flatten)]
    pub roles: MovyInitRoles,
    #[arg(
        short,
        long,
        help = "rpc to use",
        default_value = "https://fullnode.mainnet.sui.io"
    )]
    pub rpc: SuiGrpcArg,
    #[clap(flatten)]
    pub seed: RngSeed,
    #[arg(
        short,
        long,
        help = "Print the execution trace for each generated test case"
    )]
    pub trace: bool,
    #[arg(short, long, help = "Enable GraphQL during deployment")]
    pub graphql_deployment: bool,
    #[clap(flatten)]
    pub onchain: SuiOnchainArguments,
    #[clap(flatten)]
    pub target: SuiTargetArgs,
    #[clap(flatten)]
    pub filters: FuzzTargetArgs,
    #[arg(long, help = "Write line coverage in lcov format to this file")]
    pub lcov: Option<PathBuf>,
    #[arg(
        long,
        help = "Only run movy_init, then print the resulting objects and the test functions awaiting arguments, and exit"
    )]
    pub only_init: bool,
    #[arg(
        long,
        value_delimiter = ',',
        help = "Fill an object parameter from a specific object (format: <type>/0x<object_id>). Type may use a local package name, e.g. counter::counter::Counter/0x.... Repeatable; reused in parameter order for same-typed params."
    )]
    pub object_mapping: Option<Vec<String>>,
    #[arg(
        long,
        value_delimiter = ',',
        help = "Pin a test function's type parameter (format: <pkg::module::func>:<index>/<type>). Type may use a local package name. E.g. counter::counter_tests::test_foo:0/0x2::sui::SUI"
    )]
    pub test_ty: Option<Vec<String>>,
}

impl SuiTestArgs {
    pub async fn run(self) -> Result<(), MovyError> {
        let prepared = prepare_fuzz_context(
            &self.roles,
            &self.rpc,
            &self.seed,
            self.graphql_deployment,
            &self.onchain,
            &self.target,
            &self.filters,
        )
        .await?;
        let mut meta = prepared.meta;
        if self.only_init {
            return dump_only_init(
                &prepared.env,
                &meta,
                self.roles.deployer,
                self.roles.attacker,
            )
            .await;
        }
        let lcov = self
            .lcov
            .as_ref()
            .map(|path| {
                LineCoverageMap::for_locals_with_package_ids(
                    self.target.locals.as_deref().unwrap_or_default(),
                    true,
                    &meta.target_packages,
                )?
                .map(|map| (path.clone(), map))
                .ok_or_else(|| {
                    MovyError::from(eyre!("--lcov requires at least one --locals package"))
                })
            })
            .transpose()?;

        let object_mapping =
            resolve_object_mapping(&self.object_mapping, &prepared.name_mapping, &prepared.env)?;
        let type_args = resolve_test_ty(&self.test_ty, &prepared.name_mapping, &meta)?;

        meta.target_functions = select_test_functions(&meta);
        sui_test::test(
            prepared.env,
            meta,
            self.trace,
            lcov,
            object_mapping,
            type_args,
        )
    }
}

/// Parse `--object-mapping` entries (`<type>/0x<object_id>`) into a type -> object-ids map,
/// resolving local package names in the type and validating that each object exists with the
/// declared type. Same-typed entries accumulate in CLI order (consumed per parameter).
fn resolve_object_mapping(
    entries: &Option<Vec<String>>,
    name_map: &BTreeMap<String, MoveAddress>,
    env: &SuiTestingEnv<PreparedStore>,
) -> Result<BTreeMap<MoveTypeTag, Vec<MoveAddress>>, MovyError> {
    let mut out: BTreeMap<MoveTypeTag, Vec<MoveAddress>> = BTreeMap::new();
    for entry in entries.iter().flatten() {
        let (ty_str, id_str) = entry.split_once('/').ok_or_else(|| {
            eyre!("invalid --object-mapping '{entry}', expected <type>/0x<object_id>")
        })?;
        let ty = resolve_type_tag(ty_str.trim(), name_map)?;
        let id = MoveAddress::from_str(id_str.trim())?;
        let info = env
            .inner()
            .get_move_object_info(id)
            .map_err(|_| eyre!("--object-mapping object {id} not found in the store"))?;
        if info.ty != ty {
            return Err(eyre!(
                "--object-mapping object {id} has type {} but the mapping declares {}",
                info.ty,
                ty
            )
            .into());
        }
        out.entry(ty).or_default().push(id);
    }
    Ok(out)
}

/// Parse `--test-ty` entries (`<pkg::module::func>:<index>/<type>`) into a per-function map of
/// type-parameter index -> concrete type, resolving local package names in both the function
/// selector and the type.
fn resolve_test_ty(
    entries: &Option<Vec<String>>,
    name_map: &BTreeMap<String, MoveAddress>,
    meta: &movy_fuzz::meta::FuzzMetadata,
) -> Result<BTreeMap<FunctionIdent, BTreeMap<u16, MoveTypeTag>>, MovyError> {
    let mut out: BTreeMap<FunctionIdent, BTreeMap<u16, MoveTypeTag>> = BTreeMap::new();
    for entry in entries.iter().flatten() {
        let (lhs, ty_str) = entry.split_once('/').ok_or_else(|| {
            eyre!("invalid --test-ty '{entry}', expected <pkg::module::func>:<index>/<type>")
        })?;
        let (sel_str, idx_str) = lhs.rsplit_once(':').ok_or_else(|| {
            eyre!("invalid --test-ty '{entry}', missing :<type-parameter index> before the type")
        })?;
        let ident = FunctionSelector::from_str(sel_str.trim())?.to_ident(name_map)?;
        let idx: u16 = idx_str.trim().parse().map_err(|_| {
            eyre!("invalid type-parameter index '{idx_str}' in --test-ty '{entry}'")
        })?;
        let ty = resolve_type_tag(ty_str.trim(), name_map)?;

        let func = meta
            .get_function(&ident.0.module_address, &ident.0.module_name, &ident.1)
            .ok_or_else(|| eyre!("--test-ty function {ident} not found"))?;
        if idx as usize >= func.type_parameters.len() {
            return Err(eyre!(
                "--test-ty index {idx} out of range for {ident}, which has {} type parameter(s)",
                func.type_parameters.len()
            )
            .into());
        }
        out.entry(ident).or_default().insert(idx, ty);
    }
    Ok(out)
}

fn format_owner(owner: &MoveOwner, deployer: MoveAddress, attacker: MoveAddress) -> String {
    let tag = |addr: MoveAddress| {
        if addr == deployer {
            " (deployer)"
        } else if addr == attacker {
            " (attacker)"
        } else {
            ""
        }
    };
    match owner {
        MoveOwner::AddressOwner(addr) => format!("owned by {addr}{}", tag(*addr)),
        MoveOwner::ObjectOwner(addr) => format!("owned by object {addr}"),
        MoveOwner::Immutable => "immutable".to_string(),
        MoveOwner::Shared {
            initial_shared_version,
        } => format!("shared (v{initial_shared_version})"),
        MoveOwner::ConsensusAddressOwner { owner, .. } => {
            format!("consensus-owned by {owner}{}", tag(*owner))
        }
    }
}

fn format_test_signature(module: &str, func: &MoveFunctionAbi) -> String {
    let params = func
        .parameters
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let ty_params = if func.type_parameters.is_empty() {
        String::new()
    } else {
        let tys = func
            .type_parameters
            .iter()
            .enumerate()
            .map(|(i, ability)| {
                if ability.is_empty() {
                    format!("T{i}")
                } else {
                    format!("T{i}: {ability}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("<{tys}>")
    };
    format!("{module}::{}{ty_params}({params})", func.name)
}

/// Run only `movy_init` (already executed during context preparation), then print every object
/// currently in the store together with the test functions that still need their arguments filled.
/// This is the discovery step for `--object-mapping` / `--test-ty`.
async fn dump_only_init(
    env: &SuiTestingEnv<PreparedStore>,
    meta: &movy_fuzz::meta::FuzzMetadata,
    deployer: MoveAddress,
    attacker: MoveAddress,
) -> Result<(), MovyError> {
    let mut object_ids = env.inner().list_objects().await?;
    object_ids.sort();

    let mut objects = Vec::new();
    for id in object_ids {
        // Packages (and other non-move objects) are not move objects and are skipped here.
        if let Ok(info) = env.inner().get_move_object_info(id) {
            objects.push(info);
        }
    }

    println!("=== objects after movy_init ({}) ===", objects.len());
    println!("deployer: {deployer}");
    println!("attacker: {attacker}");
    for info in &objects {
        println!(
            "{}  {}  [{}]  v{}",
            info.id,
            info.ty,
            format_owner(&info.owner, deployer, attacker),
            info.version
        );
    }

    println!("\n=== test functions ===");
    for (package, abi) in meta
        .testing_abis
        .iter()
        .filter(|(package, _)| meta.target_packages.contains(package))
    {
        for module in &abi.modules {
            for func in &module.functions {
                if !func.name.starts_with("test_") {
                    continue;
                }
                let needs_args = !func.parameters.is_empty() || !func.type_parameters.is_empty();
                let marker = if needs_args { "needs args" } else { "no args" };
                println!(
                    "{package}::{}  [{marker}]",
                    format_test_signature(&module.module_id.module_name, func)
                );
            }
        }
    }

    Ok(())
}

fn select_test_functions(meta: &movy_fuzz::meta::FuzzMetadata) -> Vec<FunctionIdent> {
    let mut functions: Vec<_> = meta
        .testing_abis
        .iter()
        .filter(|(package, _)| meta.target_packages.contains(package))
        .flat_map(|(package, abi)| {
            abi.modules.iter().flat_map(move |module| {
                module.functions.iter().filter_map(move |function| {
                    if function.name.starts_with("test_") {
                        Some(FunctionIdent::new(
                            package,
                            &module.module_id.module_name,
                            &function.name,
                        ))
                    } else {
                        None
                    }
                })
            })
        })
        .collect();
    functions.sort();
    functions.dedup();
    functions
}
