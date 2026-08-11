//! koe-cli — command-line interface for Koe.

mod commands;

use clap::{Parser, Subcommand};
use thiserror::Error;

use commands::{InfoArgs, ListArgs, PermissionsArgs, Run};

#[derive(Debug, Parser)]
#[command(
    name = "koe",
    version = env!("CARGO_PKG_VERSION"),
    about = "Capture, transcribe, and inspect system audio on macOS",
    arg_required_else_help = true,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

// record / transcribe / completions wait on unfinished task deps (20–22, 24, 26, 28–30).
#[derive(Debug, Subcommand)]
enum Command {
    /// List capture-able apps and audio activity.
    List(ListArgs),
    /// Check and diagnose macOS permissions.
    Permissions(PermissionsArgs),
    /// Show build and host system information.
    Info(InfoArgs),
}

#[derive(Debug, Error)]
pub(crate) enum MainError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// No `NativeProvider` is registered in this process.
    #[error(
        "native provider is not registered\n\
         `{0}` requires a registered NativeProvider \
         (macOS discovery shim failed to install, or register_native_provider was never called)"
    )]
    NativeBridgeUnavailable(&'static str),

    #[error("one or more permissions are not authorized")]
    PermissionsNotAuthorized,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), MainError> {
    let _ = koe_core::install_default_native_provider();

    let cli = Cli::parse();
    match cli.command {
        Command::List(args) => args.run(),
        Command::Permissions(args) => args.run(),
        Command::Info(args) => args.run(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_list_flags() {
        let cli = Cli::try_parse_from(["koe", "list", "--audio-only", "--json"]).expect("parse");
        assert!(matches!(cli.command, Command::List(_)));
    }

    #[test]
    fn parses_permissions_check() {
        let cli = Cli::try_parse_from(["koe", "permissions", "--check", "--json"]).expect("parse");
        assert!(matches!(cli.command, Command::Permissions(_)));
    }

    #[test]
    fn parses_info() {
        let cli = Cli::try_parse_from(["koe", "info", "--json"]).expect("parse");
        assert!(matches!(cli.command, Command::Info(_)));
    }
}
