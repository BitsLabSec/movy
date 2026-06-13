use std::{path::PathBuf, sync::Arc};

use clap::Args;
use color_eyre::eyre::eyre;
use movy_fuzz::{
    input::MoveFuzzInput,
    meta::FuzzMetadata,
    operations::sui_replay::{sui_fuzz_replay_seed, sui_plain_replay_seed},
};
use movy_replay::{
    env::SuiTestingEnv,
    exec::SuiExecutor,
    tracer::{SelectiveTracer, TeeTracer, lcov::LineCoverageCollector, tree::TreeTracer},
};
use movy_sui::database::{
    cache::{CachedSnapshot, CachedStore},
    graphql::GraphQlDatabase,
};
use movy_sui::lcov::LineCoverageMap;
use movy_types::error::MovyError;
use sui_types::effects::TransactionEffectsAPI;

use crate::sui::utils::{read_bcs_value, read_value};

#[derive(Args)]
pub struct SuiReplaySeedArgs {
    #[arg(short, long, help = "Path to a seed file")]
    pub seed: PathBuf,
    #[arg(short, long, help = "Path to an env file, usually env.bin")]
    pub env: PathBuf,
    #[arg(short, long, help = "Path to a fuzz meta, usually fuzz_meta.json")]
    pub meta: PathBuf,
    #[arg(
        long,
        help = "Redo all fuzzing components including concolic state etc"
    )]
    pub fuzz: bool,
    #[arg(
        long,
        help = "Replay the seed on the top of testing environment, without any fuzzing information"
    )]
    pub trace: bool,
    #[arg(long, help = "Write line coverage in lcov format to this file")]
    pub lcov: Option<PathBuf>,
    #[arg(short, long, help = "Local packages to use for lcov source maps")]
    pub locals: Option<Vec<PathBuf>>,
}

impl SuiReplaySeedArgs {
    pub async fn run(self) -> Result<(), MovyError> {
        tracing::info!("Loading the seed {}", self.seed.display());
        let seed: MoveFuzzInput = read_value(&self.seed)?;
        tracing::info!("Loading the snapshot {}", self.env.display());
        let env: CachedSnapshot = read_bcs_value(&self.env)?;
        tracing::info!("Loading the fuzz metadata {}", self.meta.display());
        let meta: FuzzMetadata = read_value(&self.meta)?;
        let gql = GraphQlDatabase::new_mystens(meta.checkpoint);
        let db = CachedStore::new(gql);
        tracing::info!("Restoring the snapshot...");
        db.restore_snapshot(env);
        let env = SuiTestingEnv::new(Arc::new(db));
        if self.fuzz && self.trace {
            return Err(eyre!("Fuzz and trace are not supported together").into());
        }
        if let Some(lcov) = &self.lcov {
            if self.fuzz {
                return Err(eyre!("--lcov is only supported for plain replay").into());
            }
            let map = LineCoverageMap::for_locals_with_package_ids(
                self.locals.as_deref().unwrap_or_default(),
                true,
                &meta.target_packages,
                &movy_sui::compile::BuildIsolation::default(),
            )?
            .ok_or_else(|| {
                MovyError::from(eyre!("--lcov requires at least one --locals package"))
            })?;
            let coverage = LineCoverageCollector::new();
            let executor = SuiExecutor::new(env.into_inner())?;
            let tracer = if self.trace {
                SelectiveTracer::T1(TeeTracer(TreeTracer::new(), coverage.tracer()))
            } else {
                SelectiveTracer::T2(coverage.tracer())
            };
            let out = executor.run_ptb_with_movy_tracer_gas(
                seed.sequence.to_ptb()?,
                meta.epoch,
                meta.epoch_ms,
                meta.attacker.into(),
                meta.gas_id.into(),
                Some(tracer),
            )?;
            tracing::info!("Replay status is {:?}", &out.results.effects.status());
            if let Some(SelectiveTracer::T1(TeeTracer(tracer, _))) = out.tracer {
                println!("Trace:\n{}", &tracer.take_inner().pprint_failure_views());
            }
            map.write_lcov(coverage.hits(), lcov)?;
        } else if self.fuzz {
            sui_fuzz_replay_seed(env, meta, seed)?;
        } else {
            sui_plain_replay_seed(env, meta, seed, self.trace)?;
        }

        Ok(())
    }
}
