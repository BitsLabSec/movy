use std::path::PathBuf;

use clap::Args;
use knowdit_repo_model::RepoDatabase;
use movy_analysis::export_call_graph::load_call_graph;
use movy_types::error::MovyError;

#[derive(Args, Debug, Clone)]
pub struct ExportCallGraphArgs {
    /// Move package root containing Move.toml.
    #[arg(long, default_value = ".")]
    pub package: PathBuf,

    /// Build in test mode before exporting the call graph.
    #[arg(long)]
    pub test_mode: bool,

    /// Export the call graph into a database using the knowdit schema.
    #[arg(
        long,
        conflicts_with = "json_path",
        required_unless_present = "json_path"
    )]
    pub database_url: Option<String>,

    /// Export the call graph as pretty JSON to a file.
    #[arg(
        long,
        conflicts_with = "database_url",
        required_unless_present = "database_url"
    )]
    pub json_path: Option<PathBuf>,
}

impl ExportCallGraphArgs {
    pub async fn run(self) -> Result<(), MovyError> {
        let call_graph = load_call_graph(&self.package, self.test_mode)?;

        if let Some(database_url) = self.database_url {
            let repo_db = RepoDatabase::open_url(database_url).await?;
            repo_db.write_call_graph(&call_graph).await?;
            println!("Call graph exported to database.");
        } else if let Some(json_path) = self.json_path {
            if let Some(parent) = json_path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            let data = serde_json::to_vec_pretty(&call_graph)
                .map_err(|error| MovyError::Any(error.into()))?;
            std::fs::write(&json_path, data)?;
            println!("Call graph exported to {}.", json_path.display());
        } else {
            unreachable!("clap should require either --database-url or --json-path");
        }
        Ok(())
    }
}
