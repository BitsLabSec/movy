use std::{path::PathBuf, sync::Arc};

use clap::Args;
use color_eyre::eyre::eyre;
use movy_fuzz::operations::sui_fuzz;
use movy_replay::{db::ObjectStoreCachedStore, env::SuiTestingEnv};
use movy_sui::{
    database::{cache::CachedStore, empty::EmptyStore, graphql::GraphQlDatabase},
    rpc::grpc::SuiGrpcArg,
    utils::TrivialBackStore,
};
use movy_types::error::MovyError;
use serde::{Deserialize, Serialize};

use crate::sui::{
    env::{FuzzTargetArgs, SuiTargetArgs},
    prepare::prepare_fuzz_context,
    utils::{MovyInitRoles, RngSeed, SuiOnchainArguments, may_save_bytes, may_save_json_value},
};

#[derive(Args, Clone, Debug, Serialize, Deserialize)]
pub struct SuiFuzzArgs {
    #[clap(flatten)]
    pub roles: MovyInitRoles,
    #[arg(
        short,
        long,
        help = "rpc to use",
        default_value = "https://fullnode.mainnet.sui.io"
    )]
    pub rpc: SuiGrpcArg,
    #[arg(long, help = "Time limit of the fuzzing campaign")]
    pub time_limit: Option<u64>,
    #[arg(long, help = "Cycle limit fo the fuzzing campaign")]
    pub cycle_limit: Option<u64>,
    #[clap(flatten)]
    pub seed: RngSeed,
    #[arg(short, long, help = "Ouput directory to save all contents")]
    pub output: Option<PathBuf>,
    #[arg(
        short,
        long,
        help = "Force removal of the output directory",
        env = "MOVY_FORCE_REMOVAL"
    )]
    pub force_removal: bool,

    #[arg(short, long, help = "Enable GraphQL fallback")]
    pub graphql: bool,
    #[arg(long, help = "Enable GraphQL during deployment")]
    pub graphql_deployment: bool,

    #[clap(flatten)]
    pub onchain: SuiOnchainArguments,
    #[clap(flatten)]
    pub target: SuiTargetArgs,
    #[clap(flatten)]
    pub filters: FuzzTargetArgs,
    #[arg(
        long,
        help = "Detect typed bug via abort code 19260817 instead of oracle event",
        default_value_t = false
    )]
    pub typed_bug_abort: bool,
    #[arg(
        long,
        help = "Disable profit oracle (ProceedsOracle)",
        default_value_t = false
    )]
    pub disable_profit_oracle: bool,
    #[arg(
        long,
        help = "Disable defect oracles (others including typed bug event-based checks)",
        default_value_t = false
    )]
    pub disable_defects_oracle: bool,
}

impl SuiFuzzArgs {
    pub async fn run(self) -> Result<(), MovyError> {
        if let Some(output) = &self.output {
            if output.exists() {
                tracing::info!("We will remove {}", output.display());
                if self.force_removal {
                    std::fs::remove_dir_all(output)?;
                } else {
                    return Err(eyre!("The given output is already there, pass -f or env MOVY_FORCE_REMOVAl to always remove it").into());
                }
            }
            std::fs::create_dir_all(output)?;
        }
        may_save_json_value(&self.output, "args.json", &self)?;
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
        let testing_env = prepared.env;
        let meta = prepared.meta;

        may_save_json_value(&self.output, "fuzz_meta.json", &meta)?;
        may_save_bytes(&self.output, "env.bin", &testing_env.inner().dump().await?)?;

        // Arc<T> is send only if T is Sync while RefCell is not.
        let inner = testing_env.into_inner();
        let inner = Arc::try_unwrap(inner).unwrap();
        let dump = inner.inner.take();
        let inner = if self.graphql {
            TrivialBackStore::T1(GraphQlDatabase::new_mystens(meta.checkpoint))
        } else {
            TrivialBackStore::T2(EmptyStore)
        };
        let store = CachedStore::new(inner);
        store.restore_snapshot(dump);
        tokio::task::spawn_blocking(move || {
            let env = SuiTestingEnv::new(store.wrapped());
            sui_fuzz::fuzz(
                meta,
                env,
                &self.output,
                self.time_limit,
                self.cycle_limit,
                self.typed_bug_abort,
                self.disable_profit_oracle,
                self.disable_defects_oracle,
            )
        })
        .await??;
        Ok(())
    }
}
