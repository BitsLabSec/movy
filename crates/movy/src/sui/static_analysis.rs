use std::{path::PathBuf, str::FromStr};

use clap::Args;
use color_eyre::eyre::eyre;
use movy_fuzz::utils::{SuperRand, random_seed};
use movy_replay::{db::ObjectStoreMintObject, env::SuiTestingEnv};
use movy_static_analysis::sui as static_sui;
use movy_sui::{
    database::{cache::CachedStore, graphql::GraphQlDatabase},
    rpc::{graphql::GraphQlClient, grpc::SuiGrpcArg},
};
use movy_types::{
    error::MovyError,
    input::{MoveAddress, MoveTypeTag},
    object::MoveOwner,
};
use sui_types::base_types::ObjectID;

use crate::sui::env::SuiTargetArgs;

#[derive(Args)]
pub struct SuiStaticAnalysisArgs {
    #[arg(
        short,
        long,
        help = "deployer to use",
        default_value = "0xb64151ee0dd0f7bab72df320c5f8e0c4b784958e7411a6c37d352fe9e176092f"
    )]
    pub deployer: MoveAddress,
    #[arg(
        short,
        long,
        help = "rpc to use",
        default_value = "https://fullnode.mainnet.sui.io"
    )]
    pub rpc: SuiGrpcArg,
    #[arg(short, long, help = "checkpoint to fork")]
    pub checkpoint: Option<u64>,
    #[arg(short, long, help = "write findings to this folder")]
    pub output: Option<PathBuf>,
    #[clap(flatten)]
    pub target: SuiTargetArgs,
}

impl SuiStaticAnalysisArgs {
    pub async fn run(self) -> Result<(), MovyError> {
        let mut rand = SuperRand::new(random_seed());
        let graphql = GraphQlClient::new_mystens();
        let _rpc = self.rpc.grpc().await?;

        let (_ckpt_contents, ckpt_summary) = graphql
            .query_checkpoint(self.checkpoint)
            .await?
            .ok_or_else(|| eyre!("no ckpt {:?} from grahql", self.checkpoint))?;
        let checkpoint = ckpt_summary.sequence_number;
        let epoch = ckpt_summary.epoch;
        let epoch_ms = ckpt_summary.timestamp_ms;

        let env = CachedStore::new(GraphQlDatabase::new_client(graphql.clone(), checkpoint));
        let gas_id = ObjectID::random_from_rng(&mut rand);
        env.mint_coin_id(
            MoveTypeTag::from_str("0x2::sui::SUI").unwrap(),
            MoveOwner::AddressOwner(self.deployer),
            gas_id.into(),
            100_000_000_000,
        )?;
        let testing_env = SuiTestingEnv::new(env);
        testing_env.mock_testing_std()?;
        let (target_packages, _, _) = self
            .target
            .build_env(
                &testing_env,
                checkpoint,
                epoch,
                epoch_ms,
                self.deployer,
                self.deployer,
                gas_id.into(),
                &graphql,
            )
            .await?;

        let reports = static_sui::run_all(&testing_env, &target_packages).await?;

        if let Some(output) = self.output {
            std::fs::create_dir_all(&output)?;
            let analysis_path = output.join("static_analysis.json");
            let fp = std::fs::File::create(&analysis_path)?;
            serde_json::to_writer_pretty(fp, &reports)?;
            println!(
                "Static analysis finished with {} findings -> {}",
                reports.len(),
                analysis_path.display()
            );
        } else if reports.is_empty() {
            println!("No findings from static analysis.");
        } else {
            for finding in &reports {
                println!("{finding}");
            }
        }

        Ok(())
    }
}
