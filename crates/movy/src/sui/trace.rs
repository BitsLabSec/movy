use clap::Args;
use color_eyre::eyre::eyre;
use movy_replay::{exec::SuiExecutor, tracer::tree::TreeTracer};
use movy_sui::{
    database::{cache::CachedStore, graphql::GraphQlDatabase},
    rpc::graphql::GraphQlClient,
};
use movy_types::error::MovyError;
use sui_types::{digests::TransactionDigest, effects::TransactionEffectsAPI};

#[derive(Args)]
pub struct SuiTraceArgs {
    #[arg(short, long, help = "The transaction digest to trace")]
    pub tx: TransactionDigest,
    #[arg(long)]
    pub sequence: bool,
    #[arg(long)]
    pub trace: bool,
}

impl SuiTraceArgs {
    pub async fn run(self) -> Result<(), MovyError> {
        let graphql = GraphQlClient::new_mystens();
        let mut txs = graphql
            .query_transactions(vec![self.tx.to_string()])
            .await?;
        let tx = txs.pop().unwrap();
        let tx_ckpt = tx.checkpoint;
        let fork_ckpt = tx_ckpt - 1;
        let (_fork_tx_ckpt, fork_tx_ckpt_summary) = graphql
            .query_checkpoint(Some(fork_ckpt))
            .await?
            .ok_or_else(|| eyre!("fail to fectch ckpt {}", fork_ckpt))?;

        let graphql_db = GraphQlDatabase::new_client(graphql.clone(), fork_ckpt);
        let cache_db = CachedStore::new(graphql_db);
        let executor = SuiExecutor::new(cache_db)?;

        let mut tracer = TreeTracer::new();
        let results = executor.run_tx_trace(
            tx.tx,
            fork_tx_ckpt_summary.epoch,
            fork_tx_ckpt_summary.timestamp_ms,
            Some(&mut tracer),
        )?;

        println!("The result is {:?}", results.effects.status());
        if self.trace {
            let trace = tracer.take_inner().pprint();
            println!("The trace is:\n{}", trace);
        } else {
            let effects = results.results.effects;

            println!("Changed Objects:\n");
            for (obj, _, kind) in effects.all_changed_objects() {
                println!("{}: {:?}", obj.0, &kind);
            }
        }

        Ok(())
    }
}
