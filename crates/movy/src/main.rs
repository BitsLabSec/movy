use clap::{Parser, Subcommand};

use crate::{analysis::AnlaysisArgs, sui::SuiArgs};
use std::io::IsTerminal;

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
    // Respect the NO_COLOR convention (https://no-color.org) — any
    // non-empty value disables colors. Also disable when either of
    // stdout / stderr is not a TTY (so output piped into another
    // process is plain text). The combined flag drives both the
    // panic display (color_eyre) and the tracing subscriber.
    let no_color_env = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    let tty = std::io::stdout().is_terminal() && std::io::stderr().is_terminal();
    let use_colors = tty && !no_color_env;
    if use_colors {
        color_eyre::install().unwrap();
    } else {
        // `Theme::new()` is the no-styling theme — every span style
        // collapses to a no-op so panic display is plain text.
        color_eyre::config::HookBuilder::new()
            .theme(color_eyre::config::Theme::new())
            .install()
            .unwrap();
    }
    if let Ok(dot_file) = std::env::var("DOT") {
        dotenvy::from_path(dot_file).expect("can not read dotenvy");
    } else {
        // Allows failure
        let _ = dotenvy::dotenv();
    }
    let sub = tracing_subscriber::FmtSubscriber::builder()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing::Level::INFO.into())
                .from_env()
                .expect("env contains non-utf8"),
        )
        .with_ansi(use_colors)
        .finish();
    tracing::subscriber::set_global_default(sub).unwrap();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("can not build tokio")
        .block_on(main_entry())
}
