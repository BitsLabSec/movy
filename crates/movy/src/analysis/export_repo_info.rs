//! `movy analysis export-repo-info` — single-shot repo static
//! analysis dumper for the knowdit audit pipeline.
//!
//! One walk over the Move package produces the [`CallGraph`]
//! (modules / functions / call edges) **and** the
//! [`MovePackageStructure`] (struct definitions + per-function
//! Move metadata). Both land in the project DB via
//! [`RepoDatabase::write_repo_info`], which composes
//! `write_call_graph` + `write_package_structure` so a crash
//! between them only leaves an idempotently-overwritable partial
//! state.
//!
//! Future repo-level static analyses (type graphs, storage flow,
//! …) should grow under this same command rather than spawning
//! more standalone `export-*` subcommands. The audit pipeline
//! caller (knowdit-move) gets one preflight CLI invocation
//! instead of an ordered chain.

use std::path::PathBuf;

use clap::Args;
use knowdit_repo_model::RepoDatabase;
use movy_analysis::export_package_structure::PackageWalk;
use movy_types::error::MovyError;

#[derive(Args, Debug, Clone)]
pub struct ExportRepoInfoArgs {
    /// Move package root containing `Move.toml`.
    #[arg(long, default_value = ".")]
    pub package: PathBuf,

    /// Build the package in test mode before walking. Audit
    /// pipelines typically leave this off; on for self-tests that
    /// want to include `#[test]` functions.
    #[arg(long)]
    pub test_mode: bool,

    /// Project DB URL — typically `sqlite:///abs/path/db.sqlite3?mode=rwc`.
    #[arg(long)]
    pub database_url: String,
}

impl ExportRepoInfoArgs {
    pub async fn run(self) -> Result<(), MovyError> {
        let walk = PackageWalk::from_package_root(&self.package, self.test_mode)?;

        let counts = walk.call_graph.counts();
        let struct_count = walk.structure.structs.len();
        let fn_metadata_count = walk.structure.function_metadata.len();

        let repo = RepoDatabase::open_url(self.database_url).await?;
        repo.write_repo_info(&walk.call_graph, &walk.structure)
            .await?;

        println!(
            "Repo info exported: {} module(s), {} function(s), {} call edge(s), \
             {} struct(s), {} function metadata row(s).",
            counts.container_count,
            counts.function_count,
            counts.call_count,
            struct_count,
            fn_metadata_count
        );
        Ok(())
    }
}
