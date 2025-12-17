use clap::{Parser, Subcommand};

use crate::{analysis::AnlaysisArgs, sui::SuiArgs};

mod analysis;
mod aptos;
mod sui;

#[derive(Subcommand)]
pub enum MovySubcommand {
    Sui(SuiArgs),
    Analysis(AnlaysisArgs), // Aptos(AptosArgs)
}

#[derive(Parser)]
pub struct MovyCommand {
    #[clap(subcommand)]
    pub cmd: MovySubcommand,
}

async fn main_entry() {
    let args = MovyCommand::parse();
    match args.cmd {
        MovySubcommand::Sui(args) => args.run().await.expect("sui command failed"),
        MovySubcommand::Analysis(args) => args.run().await.expect("analysis failed"),
    }
}

fn main() {
    color_eyre::install().expect("Fail to install color_eyre");
    if let Ok(dot_file) = std::env::var("DOT") {
        dotenvy::from_path(dot_file).expect("fail to import");
    } else {
        // Allows failure
        let _ = dotenvy::dotenv();
    }
    env_logger::init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("can not build runtime")
        .block_on(main_entry())
}
