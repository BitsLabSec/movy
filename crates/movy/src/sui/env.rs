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
    #[arg(
        long,
        value_delimiter = ',',
        help = "Publish local packages at fixed addresses instead of freshly derived ids (format: <pkg_name>:0x<address>). Keeps package ids and type strings stable across source edits. Repeatable / comma-separated; matches packages by name."
    )]
    pub deploy_at: Option<Vec<DeployAt>>,
    #[arg(short, long, help = "Build package with unpublished dependencies")]
    pub unpublished_dependencies: bool,
    #[arg(long, help = "Disable building dependency checks")]
    pub disable_dependency_checks: bool,
    #[clap(flatten)]
    pub isolation: BuildIsolationArgs,
}

/// CLI knobs for per-build isolation, shared by every subcommand that takes
/// [`SuiTargetArgs`]. Both knobs (`install_dir` artifact redirection) apply
/// either way; the two converters differ only in whether the injected
/// `extra_sources` are included: [`Self::with_extra_sources`] (the targeted
/// package) vs [`Self::without_extra_sources`] (dependencies / ABI /
/// coverage). Call sites use these instead of assembling a
/// [`movy_sui::compile::BuildIsolation`] by hand or passing a `default()`.
#[derive(Args, Clone, Debug, Serialize, Deserialize, Default)]
pub struct BuildIsolationArgs {
    #[arg(
        long,
        help = "Redirect compiled artifacts + lockfile for ALL local packages to this directory instead of writing them next to the sources. Keeps the source tree read-only, and (because the package build lock is keyed on the output dir) lets concurrent builds of the same source with distinct install dirs run without serializing."
    )]
    pub install_dir: Option<PathBuf>,
    #[arg(
        long,
        value_delimiter = ',',
        help = "Extra .move source files to compile into the explicitly-listed (--locals) target package's named-address scope, on top of its own sources/. Only applied to the --locals targets, never their dependencies. Lets a driver inject a generated test without copying it into the package. Test-mode only."
    )]
    pub extra_sources: Option<Vec<PathBuf>>,
}

impl BuildIsolationArgs {
    /// Build isolation **with** the injected `extra_sources`. Use for the
    /// explicitly-targeted package — its compile gets both the artifact
    /// redirection (`install_dir`) and the extra files.
    pub fn with_extra_sources(&self) -> movy_sui::compile::BuildIsolation {
        movy_sui::compile::BuildIsolation {
            install_dir: self.install_dir.clone(),
            extra_sources: self.extra_sources.clone().unwrap_or_default(),
        }
    }

    /// Build isolation with the same `install_dir` artifact redirection but
    /// **dropping** the injected `extra_sources`. Despite the name, this is
    /// not "ignore what the user passed" — it's "this particular compile is
    /// not the extra sources' target, so they must not go in".
    ///
    /// Used for every compile that is NOT the explicitly-targeted package's
    /// test build:
    ///
    /// * **Transitive local dependencies.** `build_env` compiles each entry
    ///   of `resolved_locals` (the `--locals` targets *plus* their
    ///   `local = "../dep"` dependencies) in its own pass, and in each pass
    ///   that package is the compile's root. The injected file is a test for
    ///   the *target* (`module <target>::knowdit_spec_N { use <target>::...; }`)
    ///   and won't resolve against a dependency's namespace — injecting it
    ///   into a dependency's compile is a hard compile error. The sui-side
    ///   `pkg.is_root() && test_mode` guard can't prevent this, because a
    ///   dependency *is* the root of its own pass; only the caller knows it
    ///   isn't the user-requested target (`is_explicit_target`), so the
    ///   filtering has to happen here by handing the dependency an empty
    ///   `extra_sources`.
    /// * **Auxiliary compiles** — ABI extraction (`local_abis`) and coverage
    ///   (`lcov`): the test file is neither part of the published ABI surface
    ///   nor the coverage surface of the audited code, so it's left out.
    ///
    /// See [`Self::with_extra_sources`] for the one compile that does get them.
    pub fn without_extra_sources(&self) -> movy_sui::compile::BuildIsolation {
        movy_sui::compile::BuildIsolation {
            install_dir: self.install_dir.clone(),
            extra_sources: vec![],
        }
    }
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

        // ABI computation: redirect artifacts via install_dir, but never
        // inject extra_sources here — extras are test code, not part of a
        // package's published ABI surface.
        let isolation = self.isolation.without_extra_sources();
        let resolved_locals =
            resolve_local_dependency_paths(self.locals.as_deref().unwrap_or_default(), test_mode)?;
        for local in resolved_locals {
            let package = SuiCompiledPackage::build_all_unpublished_from_folder(
                &local, test_mode, &isolation,
            )?;
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

        // --deploy-at pins listed packages (matched by name) to fixed deployment addresses.
        let mut pin_map: BTreeMap<String, MoveAddress> = BTreeMap::new();
        for entry in self.deploy_at.iter().flatten() {
            if let Some(prev) = pin_map.insert(entry.name.clone(), entry.address)
                && prev != entry.address
            {
                return Err(eyre!(
                    "conflicting --deploy-at addresses for package {}",
                    entry.name
                )
                .into());
            }
        }
        let mut pinned_seen: BTreeSet<String> = BTreeSet::new();

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
            // install_dir applies to every local package (keep all build
            // artifacts off the source tree); extra_sources only attach to
            // the explicitly-targeted package(s), never their dependencies.
            let isolation = if is_explicit_target {
                self.isolation.with_extra_sources()
            } else {
                self.isolation.without_extra_sources()
            };
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
                    &pin_map,
                    rpc,
                    lcov,
                    &isolation,
                )
                .await?;
            for name in package_names.iter() {
                local_name_map.insert(name.clone(), target_package);
                if pin_map.contains_key(name) {
                    pinned_seen.insert(name.clone());
                }
            }
            if is_explicit_target {
                local_abis.push((testing_abi, abi, package_names));
                target_packages.push(target_package);
            }
        }

        let unmatched: Vec<&String> = pin_map
            .keys()
            .filter(|k| !pinned_seen.contains(*k))
            .collect();
        if !unmatched.is_empty() {
            return Err(eyre!(
                "--deploy-at names did not match any deployed local package: {:?}",
                unmatched
            )
            .into());
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

/// A `<pkg_name>:0x<address>` entry for `--deploy-at`, pinning a local package's deployment id.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeployAt {
    pub name: String,
    pub address: MoveAddress,
}

impl std::str::FromStr for DeployAt {
    type Err = MovyError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, addr) = s.split_once(':').ok_or_else(|| {
            MovyError::InvalidIdentifier(format!(
                "Invalid --deploy-at entry '{s}', expected <pkg_name>:0x<address>"
            ))
        })?;
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(MovyError::InvalidIdentifier(format!(
                "Empty package name in --deploy-at entry '{s}'"
            )));
        }
        Ok(Self {
            name,
            address: MoveAddress::from_str(addr.trim())?,
        })
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
