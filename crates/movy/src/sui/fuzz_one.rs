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
        let lcov_map = self
            .lcov
            .as_ref()
            .map(|_| {
                LineCoverageMap::for_locals_with_package_ids(
                    self.target.locals.as_deref().unwrap_or_default(),
                    true,
                    &prepared.meta.target_packages,
                    &self.target.isolation.without_extra_sources(),
                )?
                .ok_or_else(|| {
                    MovyError::from(eyre!("--lcov requires at least one --locals package"))
                })
            })
            .transpose()?;
        let report = sui_test::test(
            prepared.env,
            prepared.meta,
            self.trace,
            lcov_map,
            Default::default(),
            Default::default(),
        )?;
        // Mirror the embedded LCOV to the on-disk path; same string
        // movy embedded in the report.
        if let (Some(path), Some(lcov_text)) = (self.lcov.as_ref(), report.lcov.as_deref()) {
            std::fs::write(path, lcov_text).map_err(|e| {
                MovyError::from(eyre!("failed to write lcov {}: {e}", path.display()))
            })?;
        }
        // Preserve `sui fuzz-one`'s historical "any failure → Err"
        // exit contract. `--machine-output` lives on `sui test`; this
        // path stays human-only.
        for entry in &report.functions {
            match &entry.outcome {
                movy_types::test_report::Outcome::Ok => {
                    println!("ok {}", entry.function);
                }
                other => {
                    return Err(eyre!(
                        "{}: {}",
                        entry.function,
                        match other {
                            movy_types::test_report::Outcome::SequenceBuildFailure =>
                                "unable to construct a test sequence".to_string(),
                            movy_types::test_report::Outcome::ExecutionFailure { status_debug, .. } =>
                                format!("execution failed (status: {status_debug})"),
                            movy_types::test_report::Outcome::OracleCrash { reason, .. } =>
                                format!("oracle crash: {}", reason.as_deref().unwrap_or("<no reason>")),
                            movy_types::test_report::Outcome::Ok => unreachable!(),
                        }
                    )
                    .into());
                }
            }
        }
        Ok(())
    }
}
