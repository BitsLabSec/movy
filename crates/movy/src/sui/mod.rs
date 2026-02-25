use clap::{Args, Subcommand};
use movy_types::error::MovyError;

use crate::sui::{
    deploy::SuiBuildDeployArgs, fuzz::SuiFuzzArgs, replay::SuiReplaySeedArgs,
    static_analysis::SuiStaticAnalysisArgs, trace::SuiTraceArgs,
};

pub mod deploy;
pub mod env;
pub mod fuzz;
pub mod replay;
pub mod static_analysis;
pub mod trace;
pub mod utils;

#[derive(Subcommand)]
pub enum SuiSubcommand {
    TraceTx(SuiTraceArgs),
    Fuzz(SuiFuzzArgs),
    BuildDeploy(SuiBuildDeployArgs),
    ReplaySeed(SuiReplaySeedArgs),
    StaticAnalysis(SuiStaticAnalysisArgs),
}

#[derive(Args)]
pub struct SuiArgs {
    #[clap(subcommand)]
    pub cmd: SuiSubcommand,
}

impl SuiArgs {
    pub async fn run(self) -> Result<(), MovyError> {
        // Increase the maximum move package size
        // Super safe because we are the only active thread at this moment
        unsafe {
            std::env::set_var("SUI_PROTOCOL_CONFIG_OVERRIDE_ENABLE", "1");
            std::env::set_var(
                "SUI_PROTOCOL_CONFIG_OVERRIDE_MAX_MOVE_PACKAGE_SIZE",
                "16777216",
            );
            std::env::set_var("SUI_PROTOCOL_CONFIG_OVERRIDE_BASE_TX_COST_PER_BYTE", "0");
            std::env::set_var(
                "SUI_PROTOCOL_CONFIG_OVERRIDE_OBJ_ACCESS_COST_MUTATE_PER_BYTE",
                "0",
            );
            std::env::set_var(
                "SUI_PROTOCOL_CONFIG_OVERRIDE_OBJ_ACCESS_COST_VERIFY_PER_BYTE",
                "0",
            );
            std::env::set_var(
                "SUI_PROTOCOL_CONFIG_OVERRIDE_PACKAGE_PUBLISH_COST_PER_BYTE",
                "0",
            );
        }
        match self.cmd {
            SuiSubcommand::TraceTx(args) => args.run().await?,
            SuiSubcommand::Fuzz(args) => args.run().await?,
            SuiSubcommand::StaticAnalysis(args) => args.run().await?,
            SuiSubcommand::ReplaySeed(args) => args.run().await?,
            SuiSubcommand::BuildDeploy(args) => args.run().await?,
        }
        Ok(())
    }
}
