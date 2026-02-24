use std::str::FromStr;

use clap::Args;
use movy_replay::{db::ObjectStoreMintObject, env::SuiTestingEnv};
use movy_sui::{
    database::{cache::CachedStore, empty::EmptyStore, graphql::GraphQlDatabase},
    rpc::graphql::GraphQlClient,
    utils::TrivialBackStore,
};
use movy_types::{error::MovyError, input::MoveTypeTag, object::MoveOwner};
use sui_types::base_types::ObjectID;

use crate::sui::{
    env::SuiTargetArgs,
    utils::{MovyInitRoles, RngSeed, SuiOnchainArguments},
};

#[derive(Args)]
pub struct SuiBuildDeployArgs {
    #[arg(short, long)]
    pub graphql: bool,
    #[clap(flatten)]
    pub seed: RngSeed,
    #[clap(flatten)]
    pub onchain: SuiOnchainArguments,
    #[clap(flatten)]
    pub target: SuiTargetArgs,
    #[clap(flatten)]
    pub roles: MovyInitRoles,
}

impl SuiBuildDeployArgs {
    pub async fn run(self) -> Result<(), MovyError> {
        let graphql = GraphQlClient::new_mystens();
        let primitives = self
            .onchain
            .resolve_onchain_primitives(Some(&graphql))
            .await?;
        let mut rand = self.seed.rng();

        let gdb = GraphQlDatabase::new_client(graphql.clone(), primitives.checkpoint);
        let inner = if self.graphql {
            TrivialBackStore::T1(gdb.clone())
        } else {
            TrivialBackStore::T2(EmptyStore)
        };
        let env = CachedStore::new(inner);
        let gas_id = ObjectID::random_from_rng(&mut rand);
        env.mint_coin_id(
            MoveTypeTag::from_str("0x2::sui::SUI").unwrap(),
            MoveOwner::AddressOwner(self.roles.deployer),
            gas_id.into(),
            100_000_000_000_0000,
        )?;
        let testing_env = SuiTestingEnv::new(env.wrapped());
        testing_env.mock_testing_std()?;
        testing_env.install_movy()?;

        // TODO: Drop dependency on graphql
        let result = self
            .target
            .build_env(
                &testing_env,
                primitives.checkpoint,
                primitives.epoch,
                primitives.epoch_ms,
                self.roles.deployer,
                self.roles.attacker,
                gas_id.into(),
                &gdb,
            )
            .await?;
        tracing::info!("Deployment succeeds, summary: {}", &result);
        Ok(())
    }
}
