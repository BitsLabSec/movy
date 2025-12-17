use clap::{Args, Subcommand};
use movy_types::error::MovyError;

use crate::sui::{
    fuzz::SuiFuzzArgs, replay::SuiReplaySeedArgs, static_analysis::SuiStaticAnalysisArgs,
    trace::SuiTraceArgs,
};

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
        match self.cmd {
            SuiSubcommand::TraceTx(args) => args.run().await?,
            SuiSubcommand::Fuzz(args) => args.run().await?,
            SuiSubcommand::StaticAnalysis(args) => args.run().await?,
            SuiSubcommand::ReplaySeed(args) => args.run().await?,
        }
        Ok(())
    }
}
