use clap::Args;
use movy_fuzz::operations::sui_test;
use movy_sui::rpc::grpc::SuiGrpcArg;
use movy_types::error::MovyError;
use serde::{Deserialize, Serialize};

use crate::sui::{
    env::{FuzzTargetArgs, SuiTargetArgs},
    prepare::prepare_fuzz_context,
    utils::{MovyInitRoles, RngSeed, SuiOnchainArguments},
};

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
        sui_test::test(prepared.env, prepared.meta, self.trace)
    }
}
