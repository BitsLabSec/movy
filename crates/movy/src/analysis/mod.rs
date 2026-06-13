use std::{
    io::{Read, Write},
    path::Path,
};

use clap::{Args, Subcommand};
use movy_types::{error::MovyError, module::MoveModule};

use crate::analysis::{
    call_graph::CallGraphArgs, export_repo_info::ExportRepoInfoArgs, type_graph::TypeGraphArgs,
};

pub mod call_graph;
pub mod export_repo_info;
pub mod type_graph;

pub fn write_dot_may_with_pdf(dot: String, fpath: &Path) -> Result<(), MovyError> {
    let fname = fpath.file_name().unwrap().to_str().unwrap();
    let folder = fpath.parent().unwrap();
    let pdf_path = folder.join(format!("{}.pdf", fname));
    let mut fp = std::fs::File::create(fpath)?;
    fp.write_all(dot.as_bytes())?;
    fp.flush()?;

    tracing::debug!("Converting pdf to {}", pdf_path.display());

    match std::process::Command::new("dot")
        .args([
            "-T",
            "pdf",
            fpath.to_str().unwrap(),
            "-o",
            pdf_path.to_str().unwrap(),
        ])
        .spawn()
    {
        Ok(mut p) => {
            eprintln!(
                "We will write additional pdf file to {} since dot is detected.",
                pdf_path.display()
            );
            p.wait()?;
            tracing::debug!("Pdf written to {}!", pdf_path.display());
        }
        Err(e) => {
            if let std::io::ErrorKind::NotFound = e.kind() {
                tracing::debug!("No dot installed!");
            } else {
                return Err(e.into());
            }
        }
    }
    Ok(())
}

// TODO: Flavor::Aptos
pub fn glob_modules(pattern: &str) -> Result<Vec<MoveModule>, MovyError> {
    let mut out = vec![];
    for path in glob::glob(pattern)? {
        let path = path?;
        let mut fp = std::fs::File::open(&path)?;
        let mut buf = vec![];
        fp.read_to_end(&mut buf)?;
        let module = MoveModule::from_sui_module_contents(&buf)?;
        out.push(module);
    }
    Ok(out)
}

#[derive(Subcommand)]
pub enum AnalysisSubcommand {
    /// DOT dump of the type graph. Visualisation only — does not
    /// touch any project DB.
    TypeGraph(TypeGraphArgs),
    /// DOT dump of the call graph. Visualisation only — does not
    /// touch any project DB. The audit pipeline uses
    /// `export-repo-info` instead.
    CallGraph(CallGraphArgs),
    /// One-shot dumper for everything the audit pipeline needs
    /// from a Move package: call graph + struct + function
    /// metadata, all written into the project DB in one walk.
    /// Future repo-level static analyses get added here, not as
    /// new top-level subcommands.
    ExportRepoInfo(ExportRepoInfoArgs),
}

#[derive(Args)]
pub struct AnlaysisArgs {
    #[clap(subcommand)]
    pub cmd: AnalysisSubcommand,
}

impl AnlaysisArgs {
    pub async fn run(self) -> Result<(), MovyError> {
        match self.cmd {
            AnalysisSubcommand::TypeGraph(args) => args.run().await?,
            AnalysisSubcommand::CallGraph(args) => args.run().await?,
            AnalysisSubcommand::ExportRepoInfo(args) => args.run().await?,
        }
        Ok(())
    }
}
