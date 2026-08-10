//! koe-cli — command-line interface for Koe.

use clap::{Parser, Subcommand};
use thiserror::Error;

#[derive(Debug, Parser)]
#[clap(
    name = env!("CARGO_PKG_NAME"),
    version = env!("CARGO_PKG_VERSION"),
    arg_required_else_help = true,
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {}

#[derive(Debug, Error)]
pub(crate) enum MainError {}

#[tokio::main]
async fn main() -> Result<(), MainError> {
    let _ = Cli::parse();

    Ok(())
}
