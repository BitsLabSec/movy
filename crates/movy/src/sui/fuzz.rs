use std::{collections::BTreeMap, path::PathBuf, str::FromStr, sync::Arc};

use clap::Args;
use color_eyre::eyre::eyre;
use log::debug;
use movy_fuzz::{
    meta::{FuzzMetadata, TargetFilters},
    operations::sui_fuzz,
    utils::{SuperRand, random_seed},
};
use movy_replay::{
    db::{ObjectStoreCachedStore, ObjectStoreInfo, ObjectStoreMintObject},
    env::SuiTestingEnv,
};
use movy_sui::{
    database::{cache::CachedStore, graphql::GraphQlDatabase},
    rpc::{graphql::GraphQlClient, grpc::SuiGrpcArg},
};
use movy_types::{
    abi::MoveModuleId,
    error::MovyError,
    input::{MoveAddress, MoveTypeTag},
    object::MoveOwner,
};
use serde::{Deserialize, Serialize};
use sui_types::base_types::ObjectID;

use crate::sui::{
    env::{FunctionSelector, FuzzTargetArgs, ModuleSelector, PackageSelector, SuiTargetArgs},
    utils::{SuiOnchainArguments, may_save_bytes, may_save_json_value},
};

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

fn resolve_type_tag(
    raw: &str,
    local_name_map: &BTreeMap<String, MoveAddress>,
) -> Result<MoveTypeTag, MovyError> {
    let resolved = MoveTypeTag::from_str(raw);
    if resolved.is_ok() {
        return resolved;
    }
    let mut rewritten = raw.to_string();
    for (name, addr) in local_name_map.iter() {
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

#[derive(Args, Clone, Debug, Serialize, Deserialize)]
pub struct SuiFuzzArgs {
    #[arg(
        short,
        long,
        help = "deployer to use",
        default_value = "0xb64151ee0dd0f7bab72df320c5f8e0c4b784958e7411a6c37d352fe9e176092f"
    )]
    pub deployer: MoveAddress,
    #[arg(
        short,
        long,
        help = "attacker to use",
        default_value = "0xa773c4c5ef0b74150638fcfe8b0cd1bb3bbf6f1af963715168ad909bbaf2eddb"
    )]
    pub attacker: MoveAddress,
    #[arg(
        short,
        long,
        help = "rpc to use",
        default_value = "https://fullnode.mainnet.sui.io"
    )]
    pub rpc: SuiGrpcArg,
    #[arg(long, help = "Time limit of the fuzzing campaign")]
    pub time_limit: Option<u64>,
    #[arg(long, help = "rng seeds")]
    pub seed: Option<u64>,
    #[arg(short, long, help = "Ouput directory to save all contents")]
    pub output: Option<PathBuf>,
    #[arg(
        short,
        long,
        help = "Force removal of the output directory",
        env = "MOVY_FORCE_REMOVAL"
    )]
    pub force_removal: bool,

    #[clap(flatten)]
    pub onchain: SuiOnchainArguments,
    #[clap(flatten)]
    pub target: SuiTargetArgs,
    #[clap(flatten)]
    pub filters: FuzzTargetArgs,
}

impl SuiFuzzArgs {
    pub async fn run(self) -> Result<(), MovyError> {
        if let Some(output) = &self.output {
            if output.exists() {
                log::info!("We will remove {}", output.display());
                if self.force_removal {
                    std::fs::remove_dir_all(output)?;
                } else {
                    return Err(eyre!("The given output is already there, pass -f or env MOVY_FORCE_REMOVAl to always remove it").into());
                }
            }
            std::fs::create_dir_all(output)?;
        }
        may_save_json_value(&self.output, "args.json", &self)?;
        let seed = if let Some(seed) = self.seed {
            seed
        } else {
            random_seed()
        };
        let mut rand = SuperRand::new(seed);
        let graphql = GraphQlClient::new_mystens();
        let _rpc = self.rpc.grpc().await?;
        let primitives = self
            .onchain
            .resolve_onchain_primitives(Some(&graphql))
            .await?;
        let env = CachedStore::new(GraphQlDatabase::new_client(
            graphql.clone(),
            primitives.checkpoint,
        ));
        let gas_id = ObjectID::random_from_rng(&mut rand);
        env.mint_coin_id(
            MoveTypeTag::from_str("0x2::sui::SUI").unwrap(),
            MoveOwner::AddressOwner(self.deployer),
            gas_id.into(),
            100_000_000_000,
        )?;
        let testing_env = SuiTestingEnv::new(env);
        testing_env.mock_testing_std()?;

        // TODO: Drop dependency on graphql
        let (target_packages, local_abis, mut local_name_map) = self
            .target
            .build_env(
                &testing_env,
                primitives.checkpoint,
                primitives.epoch,
                primitives.epoch_ms,
                self.deployer,
                self.attacker,
                gas_id.into(),
                &graphql,
            )
            .await?;
        let mut abis = BTreeMap::new();
        let mut testing_abis = BTreeMap::new();

        for (testing_abi, abi, names) in local_abis {
            let testing_pkg = testing_abi.package_id;
            abis.insert(abi.package_id, abi);
            testing_abis.insert(testing_pkg, testing_abi);
            for name in names {
                local_name_map.entry(name).or_insert(testing_pkg.into());
            }
        }
        debug!("Local name map: {:?}", local_name_map);

        for target in target_packages.iter() {
            if !abis.contains_key(target) {
                let abi = testing_env.inner().get_package_info(*target)?.unwrap();
                abis.insert(*target, abi);
            }
        }

        let mut exclude_modules = self.filters.exclude_modules.clone().unwrap_or_default();
        exclude_modules.extend(
            ["movy::context", "movy::oracle", "movy::log"]
                .into_iter()
                .filter_map(|m| ModuleSelector::from_str(m).ok()),
        );
        exclude_modules.sort();
        exclude_modules.dedup();

        let filters = TargetFilters {
            include_packages: resolve_packages(&self.filters.include_packages, &local_name_map)?,
            exclude_packages: resolve_packages(&self.filters.exclude_packages, &local_name_map)?,
            include_modules: resolve_modules(&self.filters.include_modules, &local_name_map)?,
            exclude_modules: resolve_modules(&Some(exclude_modules), &local_name_map)?,
            include_functions: resolve_functions(&self.filters.include_functions, &local_name_map)?,
            exclude_functions: resolve_functions(&self.filters.exclude_functions, &local_name_map)?,
            include_types: resolve_type_tags(&self.filters.include_types, &local_name_map)?,
            exclude_types: resolve_type_tags(&self.filters.exclude_types, &local_name_map)?,
        };

        let meta = FuzzMetadata::from_env(
            &testing_env,
            rand,
            self.filters.privilege_functions.unwrap_or_default(),
            target_packages,
            self.attacker,
            self.deployer,
            gas_id.into(),
            abis,
            testing_abis,
            primitives.checkpoint,
            primitives.epoch,
            primitives.epoch_ms,
            filters,
        )
        .await?;

        may_save_json_value(&self.output, "fuzz_meta.json", &meta)?;
        may_save_bytes(&self.output, "env.bin", &testing_env.inner().dump().await?)?;

        tokio::task::spawn_blocking(move || {
            let inner = testing_env.into_inner();
            let env = SuiTestingEnv::new(Arc::new(inner));
            sui_fuzz::fuzz(meta, env, &self.output, self.time_limit)
        })
        .await??;
        Ok(())
    }
}
