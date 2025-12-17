use std::{path::PathBuf, sync::Arc};

use clap::Args;
use color_eyre::eyre::eyre;
use movy_fuzz::{
    input::MoveFuzzInput,
    meta::FuzzMetadata,
    operations::sui_replay::{sui_fuzz_replay_seed, sui_plain_replay_seed},
};
use movy_replay::env::SuiTestingEnv;
use movy_sui::database::{
    cache::{CachedSnapshot, CachedStore},
    graphql::GraphQlDatabase,
};
use movy_types::error::MovyError;

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
}

impl SuiReplaySeedArgs {
    pub async fn run(self) -> Result<(), MovyError> {
        log::info!("Loading the seed {}", self.seed.display());
        let seed: MoveFuzzInput = read_value(&self.seed)?;
        log::info!("Loading the snapshot {}", self.env.display());
        let env: CachedSnapshot = read_bcs_value(&self.env)?;
        log::info!("Loading the fuzz metadata {}", self.meta.display());
        let meta: FuzzMetadata = read_value(&self.meta)?;
        let gql = GraphQlDatabase::new_mystens(meta.checkpoint);
        let db = CachedStore::new(gql);
        log::info!("Restoring the snapshot...");
        db.restore_snapshot(env);
        let env = SuiTestingEnv::new(Arc::new(db));
        if self.fuzz && self.trace {
            return Err(eyre!("Fuzz and trace are not supported together").into());
        }
        if self.fuzz {
            sui_fuzz_replay_seed(env, meta, seed)?;
        } else {
            sui_plain_replay_seed(env, meta, seed, self.trace)?;
        }

        Ok(())
    }
}
