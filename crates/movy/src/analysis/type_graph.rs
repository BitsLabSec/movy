use std::path::PathBuf;

use clap::Args;
use movy_analysis::type_graph::MoveTypeGraph;
use movy_types::{abi::MoveModuleAbi, error::MovyError};

use crate::analysis::{glob_modules, write_dot_may_with_pdf};

#[derive(Args)]
pub struct TypeGraphArgs {
    #[arg(short, long)]
    pub modules: String,
    #[arg(short, long)]
    pub output: PathBuf,
}

impl TypeGraphArgs {
    pub async fn run(self) -> Result<(), MovyError> {
        let modules = glob_modules(&self.modules)?;
        let mut tg = MoveTypeGraph::new();
        for module in modules.into_iter() {
            let module = module.as_sui_module().unwrap();
            let abi = MoveModuleAbi::from_sui_module(module);
            if abi.module_id.module_address.is_sui_std() {
                continue;
            }
            tg.add_module(&abi);
        }

        write_dot_may_with_pdf(tg.dot(), &self.output)?;
        Ok(())
    }
}
