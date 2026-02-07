use std::{collections::BTreeMap, path::PathBuf};

use clap::Args;
use color_eyre::eyre::eyre;
use itertools::Itertools;
use movy_fuzz::meta::FuzzFunctionScore;
use movy_replay::{
    db::{ObjectStoreCachedStore, ObjectStoreInfo},
    env::{PackageAddressOverride, SuiTestingEnv},
};
use movy_sui::{
    compile::SuiCompiledPackage, database::cache::ObjectSuiStoreCommit, rpc::graphql::GraphQlClient,
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
    #[arg(
        short,
        long,
        value_delimiter = ',',
        help = "The onchain packages to add."
    )]
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
        value_delimiter = ',',
        help = "Override package address mapping. Form: Name:0xPUBLISHED_AT or Name:0xORIGINAL@0xPUBLISHED_AT. Example: --package-address governance:0x03..@0x92.."
    )]
    pub package_address: Option<Vec<PackageAddressOverrideArg>>,
    #[arg(
        long,
        value_delimiter = ',',
        help = "Force redeploy test build for these packages even when --package-address is provided (sources must be part of the local build graph). Example: --redeploy-test governance"
    )]
    pub redeploy_test: Option<Vec<String>>,
}

impl SuiTargetArgs {
    pub fn local_abis(&self, test_mode: bool) -> Result<Vec<MovePackageAbi>, MovyError> {
        let mut out = vec![];

        for local in self.locals.iter().flatten() {
            let package = SuiCompiledPackage::build_all_unpublished_from_folder(local, test_mode)?;
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
        rpc: &GraphQlClient,
    ) -> Result<
        (
            Vec<MoveAddress>,
            Vec<(MovePackageAbi, MovePackageAbi, Vec<String>)>,
            BTreeMap<String, MoveAddress>,
        ),
        MovyError,
    >
    where
        T: ObjectStoreCachedStore
            + ObjectStoreInfo
            + ObjectStore
            + ObjectSuiStoreCommit
            + BackingStore
            + BackingPackageStore,
    {
        let mut target_packages = Vec::new();
        let mut local_name_map = BTreeMap::new();
        let mut package_address_overrides: BTreeMap<String, PackageAddressOverride> =
            BTreeMap::new();
        for ov in self.package_address.iter().flatten() {
            if package_address_overrides
                .insert(
                    ov.package.clone(),
                    PackageAddressOverride {
                        original: ov.original,
                        published_at: ov.published_at,
                    },
                )
                .is_some()
            {
                log::warn!("Duplicate --package-address for {}", ov.package);
            }
        }
        let package_address_overrides = if package_address_overrides.is_empty() {
            None
        } else {
            Some(package_address_overrides)
        };
        if let Some(overrides) = &package_address_overrides {
            log::debug!(
                "package address overrides: {}",
                overrides
                    .iter()
                    .map(|(name, ov)| {
                        let orig = ov
                            .original
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "<auto>".to_string());
                        format!(
                            "{}: original={} published-at={}",
                            name, orig, ov.published_at
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let redeploy_test: std::collections::BTreeSet<String> = self
            .redeploy_test
            .iter()
            .flatten()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let redeploy_test = if redeploy_test.is_empty() {
            None
        } else {
            Some(redeploy_test)
        };
        if let Some(pkgs) = &redeploy_test {
            log::debug!(
                "redeploy test packages: {}",
                pkgs.iter().cloned().collect::<Vec<_>>().join(", ")
            );
        }

        for onchain in self.onchains.iter().flatten() {
            log::info!("Deploying onchain address {} to env...", onchain);
            env.deploy_address(*onchain).await?;
            target_packages.push(*onchain);
        }

        for hist in self.histories.iter().flatten() {
            // TODO: This is unsound.
            log::info!("Loading history objects for {} at {}", hist, checkpoint);
            env.load_history(*hist, checkpoint, rpc).await?;
        }

        for obj in self.objects.iter().flatten() {
            log::info!("Loading additional object {}...", obj);
            // TODO: should wrap one level.
            env.inner().load_object(*obj).await?;
        }

        log::info!("Loading inner types...");
        env.load_inner_types().await?;

        let mut local_abis = vec![];
        for local in self.locals.iter().flatten() {
            log::info!("Deploying the local package at {}", local.display());
            let overrides_ref = package_address_overrides.as_ref();
            let redeploy_ref = redeploy_test.as_ref();
            let (target_package, testing_abi, abi, package_names) = env
                .load_local(
                    local,
                    deployer,
                    attacker,
                    epoch,
                    epoch_ms,
                    gas.into(),
                    overrides_ref,
                    redeploy_ref,
                )
                .await?;
            for name in package_names.iter() {
                local_name_map.insert(name.clone(), target_package);
            }
            local_abis.push((testing_abi, abi, package_names));
            target_packages.push(target_package);
        }

        log::info!("Reload inner types...");
        env.load_inner_types().await?;

        Ok((target_packages, local_abis, local_name_map))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackageAddressOverrideArg {
    pub package: String,
    pub original: Option<MoveAddress>,
    pub published_at: MoveAddress,
}

impl std::str::FromStr for PackageAddressOverrideArg {
    type Err = MovyError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (pkg, rest) = s.split_once(':').ok_or_else(|| {
            MovyError::InvalidIdentifier(format!("Invalid package address: {}", s))
        })?;
        let package = pkg.trim();
        if package.is_empty() {
            return Err(MovyError::InvalidIdentifier(format!(
                "Invalid package address: {}",
                s
            )));
        }

        let rest = rest.trim();
        if let Some((orig, published)) = rest.split_once('@').or_else(|| rest.split_once('=')) {
            let original = MoveAddress::from_str(orig.trim()).map_err(|_| {
                MovyError::InvalidIdentifier(format!("Invalid package address: {}", s))
            })?;
            let published_at = MoveAddress::from_str(published.trim()).map_err(|_| {
                MovyError::InvalidIdentifier(format!("Invalid package address: {}", s))
            })?;
            Ok(Self {
                package: package.to_string(),
                original: Some(original),
                published_at,
            })
        } else {
            let published_at = MoveAddress::from_str(rest).map_err(|_| {
                MovyError::InvalidIdentifier(format!("Invalid package address: {}", s))
            })?;
            Ok(Self {
                package: package.to_string(),
                original: None,
                published_at,
            })
        }
    }
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
    pub privilege_functions: Option<Vec<FuzzFunctionScore>>,
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
