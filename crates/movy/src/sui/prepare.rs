use std::{collections::BTreeMap, str::FromStr, sync::Arc};

use movy_fuzz::meta::{FuzzFunctionScore, FuzzMetadata, TargetFilters};
use movy_replay::{
    db::{ObjectStoreInfo, ObjectStoreMintObject},
    env::SuiTestingEnv,
    exec::very_big_gas,
};
use movy_sui::{
    database::{cache::CachedStore, empty::EmptyStore, graphql::GraphQlDatabase},
    rpc::{graphql::GraphQlClient, grpc::SuiGrpcArg},
    utils::TrivialBackStore,
};
use movy_types::{
    abi::MoveModuleId,
    error::MovyError,
    input::{MoveAddress, MoveTypeTag},
    object::MoveOwner,
};
use sui_types::base_types::ObjectID;

use crate::sui::{
    env::{
        DeployResult, FunctionSelector, FuzzTargetArgs, ModuleSelector, PackageSelector,
        PrivilegeFunctionScoreSelector, SuiTargetArgs,
    },
    utils::{MovyInitRoles, RngSeed, SuiOnchainArguments},
};

pub(crate) type PreparedStore = Arc<CachedStore<TrivialBackStore<GraphQlDatabase, EmptyStore>>>;

pub(crate) struct PreparedFuzzContext {
    pub env: SuiTestingEnv<PreparedStore>,
    pub meta: FuzzMetadata,
}

fn resolve_modules(
    mods: &Option<Vec<ModuleSelector>>,
    local_name_map: &BTreeMap<String, MoveAddress>,
) -> Result<Option<Vec<MoveModuleId>>, MovyError> {
    mods.as_ref()
        .map(|list| {
            list.iter()
                .map(|m| m.to_module_id(local_name_map))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
}

fn resolve_packages(
    pkgs: &Option<Vec<PackageSelector>>,
    local_name_map: &BTreeMap<String, MoveAddress>,
) -> Result<Option<Vec<MoveAddress>>, MovyError> {
    pkgs.as_ref()
        .map(|list| {
            list.iter()
                .map(|p| p.resolve_address(local_name_map))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
}

fn resolve_functions(
    funcs: &Option<Vec<FunctionSelector>>,
    local_name_map: &BTreeMap<String, MoveAddress>,
) -> Result<Option<Vec<movy_types::input::FunctionIdent>>, MovyError> {
    funcs
        .as_ref()
        .map(|list| {
            list.iter()
                .map(|f| f.to_ident(local_name_map))
                .collect::<Result<Vec<_>, MovyError>>()
        })
        .transpose()
}

fn resolve_function_scores(
    funcs: &Option<Vec<PrivilegeFunctionScoreSelector>>,
    local_name_map: &BTreeMap<String, MoveAddress>,
) -> Result<Vec<FuzzFunctionScore>, MovyError> {
    funcs
        .as_ref()
        .map(|list| {
            list.iter()
                .map(|f| f.resolve(local_name_map))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
        .map(|scores| scores.unwrap_or_default())
}

fn resolve_type_tag(
    raw: &str,
    local_name_map: &BTreeMap<String, MoveAddress>,
) -> Result<MoveTypeTag, MovyError> {
    let resolved = MoveTypeTag::from_str(raw);
    if resolved.is_ok() {
        return resolved;
    }
    let mut rewritten = raw.to_string();
    for (name, addr) in local_name_map {
        let needle = format!("{name}::");
        let replacement = format!("{}::", addr.to_canonical_string(true));
        rewritten = rewritten.replace(&needle, &replacement);
    }
    MoveTypeTag::from_str(&rewritten)
}

fn resolve_type_tags(
    tags: &Option<Vec<String>>,
    local_name_map: &BTreeMap<String, MoveAddress>,
) -> Result<Option<Vec<MoveTypeTag>>, MovyError> {
    tags.as_ref()
        .map(|list| {
            list.iter()
                .map(|t| resolve_type_tag(t, local_name_map))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
}

pub(crate) async fn prepare_fuzz_context(
    roles: &MovyInitRoles,
    rpc: &SuiGrpcArg,
    seed: &RngSeed,
    graphql_deployment: bool,
    onchain: &SuiOnchainArguments,
    target: &SuiTargetArgs,
    filter_args: &FuzzTargetArgs,
) -> Result<PreparedFuzzContext, MovyError> {
    let mut rand = seed.rng();
    let graphql = GraphQlClient::new_mystens();

    let _rpc = rpc.grpc().await?;
    let primitives = onchain.resolve_onchain_primitives(Some(&graphql)).await?;
    let graphql_db = GraphQlDatabase::new_client(graphql.clone(), primitives.checkpoint);
    let inner = if graphql_deployment {
        TrivialBackStore::T1(graphql_db.clone())
    } else {
        TrivialBackStore::T2(EmptyStore)
    };
    let env = CachedStore::new(inner);
    let gas_id = ObjectID::random_from_rng(&mut rand);
    env.mint_coin_id(
        MoveTypeTag::from_str("0x2::sui::SUI").unwrap(),
        MoveOwner::AddressOwner(roles.deployer),
        gas_id.into(),
        very_big_gas(),
    )?;
    env.mint_coin_id(
        MoveTypeTag::from_str("0x2::sui::SUI").unwrap(),
        MoveOwner::AddressOwner(roles.attacker),
        gas_id.into(),
        very_big_gas(),
    )?;
    let testing_env = SuiTestingEnv::new(env.wrapped());
    testing_env.mock_testing_std()?;
    testing_env.install_movy()?;

    let DeployResult {
        target_packages_deployed: target_packages,
        abis: local_abis,
        name_mapping: mut local_name_map,
    } = target
        .build_env(
            &testing_env,
            primitives.checkpoint,
            primitives.epoch,
            primitives.epoch_ms,
            roles.deployer,
            roles.attacker,
            gas_id.into(),
            &graphql_db,
        )
        .await?;

    let mut abis = movy_sui_stds::std_abi(true);
    let mut testing_abis = movy_sui_stds::std_abi(false);

    for (testing_abi, abi, names) in local_abis {
        let testing_pkg = testing_abi.package_id;
        abis.insert(abi.package_id, abi);
        testing_abis.insert(testing_pkg, testing_abi);
        for name in names {
            local_name_map.entry(name).or_insert(testing_pkg);
        }
    }

    for target in target_packages.iter() {
        if !abis.contains_key(target) {
            let abi = testing_env.inner().get_package_info(*target)?.unwrap();
            abis.insert(*target, abi);
        }
    }

    let mut exclude_modules = filter_args.exclude_modules.clone().unwrap_or_default();
    if local_name_map.contains_key("movy") {
        exclude_modules.extend(
            ["movy::context", "movy::oracle", "movy::log"]
                .into_iter()
                .filter_map(|m| ModuleSelector::from_str(m).ok()),
        );
        exclude_modules.sort();
        exclude_modules.dedup();
    }

    let filters = TargetFilters {
        include_packages: resolve_packages(&filter_args.include_packages, &local_name_map)?,
        exclude_packages: resolve_packages(&filter_args.exclude_packages, &local_name_map)?,
        include_modules: resolve_modules(&filter_args.include_modules, &local_name_map)?,
        exclude_modules: resolve_modules(&Some(exclude_modules), &local_name_map)?,
        include_functions: resolve_functions(&filter_args.include_functions, &local_name_map)?,
        exclude_functions: resolve_functions(&filter_args.exclude_functions, &local_name_map)?,
        include_types: resolve_type_tags(&filter_args.include_types, &local_name_map)?,
        exclude_types: resolve_type_tags(&filter_args.exclude_types, &local_name_map)?,
    };

    let meta = FuzzMetadata::from_env(
        &testing_env,
        rand,
        resolve_function_scores(&filter_args.privilege_functions, &local_name_map)?,
        target_packages,
        roles.attacker,
        roles.deployer,
        gas_id.into(),
        abis,
        testing_abis,
        primitives.checkpoint,
        primitives.epoch,
        primitives.epoch_ms,
        filters,
    )
    .await?;

    Ok(PreparedFuzzContext {
        env: testing_env,
        meta,
    })
}
