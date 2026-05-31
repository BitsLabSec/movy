use std::path::PathBuf;

use clap::Args;
use color_eyre::eyre::eyre;
use movy_fuzz::operations::sui_test;
use movy_sui::{lcov::LineCoverageMap, rpc::grpc::SuiGrpcArg};
use movy_types::error::MovyError;
use serde::{Deserialize, Serialize};

use crate::sui::{
    env::{FuzzTargetArgs, SuiTargetArgs},
    prepare::prepare_fuzz_context,
    utils::{MovyInitRoles, RngSeed, SuiOnchainArguments},
};

#[derive(Args, Clone, Debug, Serialize, Deserialize)]
pub struct SuiFuzzOneArgs {
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
}

impl SuiFuzzOneArgs {
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
        let lcov = self
            .lcov
            .as_ref()
            .map(|path| {
                LineCoverageMap::for_locals_with_package_ids(
                    self.target.locals.as_deref().unwrap_or_default(),
                    true,
                    &prepared.meta.target_packages,
                )?
                .map(|map| (path.clone(), map))
                .ok_or_else(|| {
                    MovyError::from(eyre!("--lcov requires at least one --locals package"))
                })
            })
            .transpose()?;
        sui_test::test(
            prepared.env,
            prepared.meta,
            self.trace,
            lcov,
            Default::default(),
            Default::default(),
        )
    }
}
