use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Display,
    fs,
    path::PathBuf,
};

use clap::Args;
use color_eyre::eyre::eyre;
use itertools::Itertools;
use movy_fuzz::meta::FuzzFunctionScore;
use movy_replay::{
    db::{ObjectStoreCachedStore, ObjectStoreInfo},
    env::SuiTestingEnv,
    tracer::lcov::LineCoverageCollector,
};
use movy_sui::{
    compile::{SuiCompiledPackage, resolve_local_dependency_paths},
    database::{cache::ObjectSuiStoreCommit, graphql::GraphQlDatabase},
};
use movy_types::{
    abi::{MoveModuleId, MovePackageAbi},
    error::MovyError,
    input::{FunctionIdent, MoveAddress},
};
use serde::{Deserialize, Serialize};
use sui_types::storage::{BackingPackageStore, BackingStore, ObjectStore};

#[derive(Args, Clone, Debug, Serialize, Deserialize)]
pub struct SuiTargetArgs {
    #[arg(long, value_delimiter = ',', help = "The onchain packages to add.")]
    pub onchains: Option<Vec<MoveAddress>>,
    #[arg(
        long,
        value_delimiter = ',',
        help = "Load history objects for given packages."
    )]
    pub histories: Option<Vec<MoveAddress>>,
    #[arg(long, value_delimiter = ',', help = "The additional objects to add.")]
    pub objects: Option<Vec<MoveAddress>>,
    #[arg(short, long, help = "Local packages to build.")]
    pub locals: Option<Vec<PathBuf>>,
    #[arg(
        long,
        help = "Enable onchain fallback, i.e. fetching addresses if they are missing. NOTE: This is dangerous and likely to cause failure."
    )]
    pub onchain_fallback: bool,
    #[arg(long, help = "Trace movy_init")]
    pub trace_movy_init: bool,
    #[arg(short, long, help = "Build package with unpublished dependencies")]
    pub unpublished_dependencies: bool,
    #[arg(long, help = "Disable building dependency checks")]
    pub disable_dependency_checks: bool,
}

#[derive(Debug, Clone)]
pub struct DeployResult {
    pub target_packages_deployed: Vec<MoveAddress>,
    pub abis: Vec<(MovePackageAbi, MovePackageAbi, Vec<String>)>,
    pub name_mapping: BTreeMap<String, MoveAddress>,
}

impl Display for DeployResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "Deployment(targets=[{}], mappings=[{}])",
            self.target_packages_deployed
                .iter()
                .map(|v| v.to_string())
                .join(", "),
            self.name_mapping
                .iter()
                .map(|v| format!("{} => {}", v.0, v.1))
                .join(", ")
        ))
    }
}

impl SuiTargetArgs {
    pub fn local_abis(&self, test_mode: bool) -> Result<Vec<MovePackageAbi>, MovyError> {
        let mut out = vec![];

        let resolved_locals =
            resolve_local_dependency_paths(self.locals.as_deref().unwrap_or_default(), test_mode)?;
        for local in resolved_locals {
            let package = SuiCompiledPackage::build_all_unpublished_from_folder(&local, test_mode)?;
            out.push(package.abi()?);
        }
        Ok(out)
    }
    pub async fn build_env<T>(
        &self,
        env: &SuiTestingEnv<T>,
        checkpoint: u64,
        epoch: u64,
        epoch_ms: u64,
        deployer: MoveAddress,
        attacker: MoveAddress,
        gas: MoveAddress,
        rpc: &GraphQlDatabase,
        lcov: Option<&LineCoverageCollector>,
    ) -> Result<DeployResult, MovyError>
    where
        T: ObjectStoreCachedStore
            + ObjectStoreInfo
            + ObjectStore
            + ObjectSuiStoreCommit
            + BackingStore
            + BackingPackageStore
            + Clone
            + 'static,
    {
        let mut target_packages = Vec::new();
        let mut local_name_map = BTreeMap::new();
        let explicit_locals = self
            .locals
            .iter()
            .flatten()
            .map(std::fs::canonicalize)
            .collect::<Result<BTreeSet<_>, _>>()?;
        let resolved_locals =
            resolve_local_dependency_paths(self.locals.as_deref().unwrap_or_default(), true)?;

        for onchain in self.onchains.iter().flatten() {
            env.fetch_package_at_address(*onchain, rpc).await?;
            target_packages.push(*onchain);
        }

        for hist in self.histories.iter().flatten() {
            // TODO: This is unsound.
            tracing::info!("Loading history objects for {} at {}", hist, checkpoint);
            env.load_history(*hist, checkpoint, &rpc.graphql).await?;
        }

        tracing::info!("Loading inner types...");
        env.load_inner_types().await?;

        if !resolved_locals.is_empty() {
            tracing::info!(
                "Resolved local deployment order: {}",
                resolved_locals
                    .iter()
                    .map(|path| path.display().to_string())
                    .join(" -> ")
            );
        }

        let mut local_abis = vec![];
        for local in resolved_locals.iter() {
            if let Some((package_name, package_addr)) = bundled_local_package_mapping(local)? {
                tracing::info!(
                    "Skipping bundled local package {} at {} because it is already installed",
                    package_name,
                    local.display()
                );
                local_name_map.entry(package_name).or_insert(package_addr);
                continue;
            }
            let is_explicit_target = explicit_locals.contains(local);
            tracing::info!("Deploying the local package at {}", local.display());
            let (target_package, testing_abi, abi, package_names) = env
                .load_local(
                    local.as_path(),
                    deployer,
                    attacker,
                    epoch,
                    epoch_ms,
                    gas.into(),
                    self.unpublished_dependencies,
                    !self.disable_dependency_checks,
                    self.trace_movy_init,
                    self.onchain_fallback,
                    rpc,
                    lcov,
                )
                .await?;
            for name in package_names.iter() {
                local_name_map.insert(name.clone(), target_package);
            }
            if is_explicit_target {
                local_abis.push((testing_abi, abi, package_names));
                target_packages.push(target_package);
            }
        }

        tracing::info!("Reload inner types...");
        env.load_inner_types().await?;

        Ok(DeployResult {
            target_packages_deployed: target_packages,
            abis: local_abis,
            name_mapping: local_name_map,
        })
    }
}

fn bundled_local_package_mapping(
    local: &std::path::Path,
) -> Result<Option<(String, MoveAddress)>, MovyError> {
    let manifest = local.join("Move.toml");
    let Ok(content) = fs::read_to_string(&manifest) else {
        return Ok(None);
    };

    let mut in_package = false;
    let mut name = None::<String>;
    let mut published_at = None::<String>;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim().trim_matches('"');
            match key {
                "name" => name = Some(value.to_string()),
                "published-at" => published_at = Some(value.to_string()),
                _ => {}
            }
        }
    }

    if name.as_deref() == Some("movy") && published_at.as_deref() == Some("0xdeadbeef") {
        return Ok(Some((
            "movy".to_string(),
            MoveAddress::from_str("0xdeadbeef")?,
        )));
    }

    Ok(None)
}

#[derive(Args, Clone, Debug, Serialize, Deserialize)]
pub struct FuzzTargetArgs {
    #[arg(long, value_delimiter = ',', help = "Include specific packages")]
    pub include_packages: Option<Vec<PackageSelector>>,
    #[arg(long, value_delimiter = ',', help = "Include specific modules")]
    pub include_modules: Option<Vec<ModuleSelector>>,
    #[arg(long, value_delimiter = ',', help = "Include specific functions")]
    pub include_functions: Option<Vec<FunctionSelector>>,
    #[arg(long, value_delimiter = ',', help = "Include specific types")]
    pub include_types: Option<Vec<String>>,
    #[arg(long, value_delimiter = ',', help = "Exclude specific packages")]
    pub exclude_packages: Option<Vec<PackageSelector>>,
    #[arg(long, value_delimiter = ',', help = "Exclude specific modules")]
    pub exclude_modules: Option<Vec<ModuleSelector>>,
    #[arg(long, value_delimiter = ',', help = "Exclude specific functions")]
    pub exclude_functions: Option<Vec<FunctionSelector>>,
    #[arg(long, value_delimiter = ',', help = "Exclude specific types")]
    pub exclude_types: Option<Vec<String>>,
    #[arg(long, value_delimiter = ',')]
    pub privilege_functions: Option<Vec<PrivilegeFunctionScoreSelector>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum PackageRef {
    Address(MoveAddress),
    Named(String),
}

impl PackageRef {
    pub fn resolve(
        &self,
        local_name_map: &BTreeMap<String, MoveAddress>,
    ) -> Result<MoveAddress, MovyError> {
        Ok(match self {
            PackageRef::Address(addr) => *addr,
            PackageRef::Named(name) => local_name_map
                .get(name)
                .copied()
                .ok_or_else(|| eyre!("Unknown package name {}", name))?,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModuleSelector {
    pub package: PackageRef,
    pub module: String,
}

impl std::str::FromStr for ModuleSelector {
    type Err = MovyError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts = s.split("::").collect_vec();
        if parts.len() != 2 {
            return Err(MovyError::InvalidIdentifier(format!(
                "Invalid module selector string: {}",
                s
            )));
        }
        let package = match MoveAddress::from_str(parts[0]) {
            Ok(addr) => PackageRef::Address(addr),
            Err(_) => PackageRef::Named(parts[0].to_string()),
        };
        Ok(Self {
            package,
            module: parts[1].to_string(),
        })
    }
}

impl ModuleSelector {
    pub fn to_module_id(
        &self,
        local_name_map: &BTreeMap<String, MoveAddress>,
    ) -> Result<MoveModuleId, MovyError> {
        let module_address = self.package.resolve(local_name_map)?;
        Ok(MoveModuleId {
            module_address,
            module_name: self.module.clone(),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageSelector(pub PackageRef);

impl std::str::FromStr for PackageSelector {
    type Err = MovyError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let package = match MoveAddress::from_str(s) {
            Ok(addr) => PackageRef::Address(addr),
            Err(_) => PackageRef::Named(s.to_string()),
        };
        Ok(Self(package))
    }
}

impl PackageSelector {
    pub fn resolve_address(
        &self,
        local_name_map: &BTreeMap<String, MoveAddress>,
    ) -> Result<MoveAddress, MovyError> {
        self.0.resolve(local_name_map)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct FunctionSelector {
    pub package: PackageRef,
    pub module: String,
    pub function: String,
}

impl std::str::FromStr for FunctionSelector {
    type Err = MovyError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts = s.split("::").collect_vec();
        if parts.len() != 3 {
            return Err(MovyError::InvalidIdentifier(format!(
                "Invalid function selector string: {}",
                s
            )));
        }
        let package = match MoveAddress::from_str(parts[0]) {
            Ok(addr) => PackageRef::Address(addr),
            Err(_) => PackageRef::Named(parts[0].to_string()),
        };
        Ok(Self {
            package,
            module: parts[1].to_string(),
            function: parts[2].to_string(),
        })
    }
}

impl FunctionSelector {
    pub fn to_ident(
        &self,
        local_name_map: &BTreeMap<String, MoveAddress>,
    ) -> Result<FunctionIdent, MovyError> {
        let addr = self.package.resolve(local_name_map)?;
        Ok(movy_types::input::FunctionIdent::new(
            &addr,
            &self.module,
            &self.function,
        ))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct PrivilegeFunctionScoreSelector {
    pub function: FunctionSelector,
    pub score: u64,
}

impl std::str::FromStr for PrivilegeFunctionScoreSelector {
    type Err = MovyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let st = s.split('/').collect_vec();
        if st.len() != 2 {
            return Err(eyre!("expected usage: hopv4::liquidity::open_position/1000").into());
        }
        let score = u64::from_str(st[1]).map_err(|_| eyre!("can not parse score {}", st[1]))?;
        let function = FunctionSelector::from_str(st[0])?;
        Ok(Self { function, score })
    }
}

impl PrivilegeFunctionScoreSelector {
    pub fn resolve(
        &self,
        local_name_map: &BTreeMap<String, MoveAddress>,
    ) -> Result<FuzzFunctionScore, MovyError> {
        Ok(FuzzFunctionScore {
            function: self.function.to_ident(local_name_map)?,
            score: self.score,
        })
    }
}
