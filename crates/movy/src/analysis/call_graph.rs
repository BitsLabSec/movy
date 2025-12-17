use std::path::PathBuf;

use clap::Args;
use movy_analysis::call_graph::MoveCallGraph;
use movy_types::{bytecode::MoveModuleBytecodeAnalysis, error::MovyError};

use crate::analysis::{glob_modules, write_dot_may_with_pdf};

#[derive(Args)]
pub struct CallGraphArgs {
    #[arg(short, long)]
    pub modules: String,
    #[arg(short, long)]
    pub output: PathBuf,
}

impl CallGraphArgs {
    pub async fn run(self) -> Result<(), MovyError> {
        let modules = glob_modules(&self.modules)?;
        let mut cg = MoveCallGraph::new();
        for module in modules.into_iter() {
            let module = module.as_sui_module().unwrap();
            let result = MoveModuleBytecodeAnalysis::from_sui_module(module);
            if result.abi.module_id.module_address.is_sui_std() {
                continue;
            }
            cg.add_bytecode_analysis(&result);
        }

        write_dot_may_with_pdf(cg.dot(), &self.output)?;
        Ok(())
    }
}
