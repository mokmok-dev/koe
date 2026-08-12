//! koe-cli — command-line interface for Koe.

mod commands;

use clap::{Parser, Subcommand};
use thiserror::Error;

use commands::{InfoArgs, ListArgs, PermissionsArgs, RecordArgs, Run, TranscribeArgs};

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

#[derive(Debug, Subcommand)]
enum Command {
    /// Start a recording with optional transcription.
    Record(Box<RecordArgs>),
    /// List capture-able apps and audio activity.
    List(ListArgs),
    /// Transcribe an existing audio file (offline).
    Transcribe(TranscribeArgs),
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

    #[error("permission denied: {0} (tip: run `koe permissions`)")]
    PermissionDenied(String),

    #[error("invalid arguments: {0}")]
    InvalidArgs(String),

    #[error("capture error: {0}")]
    Capture(String),

    #[error("I/O error: {0}")]
    Io(String),

    #[error("interrupted")]
    Interrupted,

    #[error("internal error: {0}")]
    Internal(String),
}

impl MainError {
    /// Process exit code per the CLI interface spec.
    const fn exit_code(&self) -> i32 {
        match self {
            Self::PermissionDenied(_) | Self::PermissionsNotAuthorized => 1,
            Self::InvalidArgs(_) | Self::Json(_) => 2,
            Self::Capture(_) => 3,
            Self::Io(_) => 4,
            Self::Interrupted => 5,
            Self::NativeBridgeUnavailable(_) | Self::Internal(_) => 6,
        }
    }
}

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(error) => {
            // Interrupted is an expected exit path; keep the message short.
            if !matches!(error, MainError::Interrupted) {
                eprintln!("{error}");
            }
            error.exit_code()
        },
    };
    std::process::exit(code);
}

fn run() -> Result<(), MainError> {
    let _ = koe_core::install_default_native_provider();

    let cli = Cli::parse();
    match cli.command {
        Command::Record(args) => (*args).run(),
        Command::List(args) => args.run(),
        Command::Transcribe(args) => args.run(),
        Command::Permissions(args) => args.run(),
        Command::Info(args) => args.run(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_record_list_sources() {
        let cli = Cli::try_parse_from(["koe", "record", "--list-sources"]).expect("parse");
        assert!(matches!(cli.command, Command::Record(_)));
    }

    #[test]
    fn parses_list_flags() {
        let cli = Cli::try_parse_from(["koe", "list", "--audio-only", "--json"]).expect("parse");
        assert!(matches!(cli.command, Command::List(_)));
    }

    #[test]
    fn parses_transcribe_flags() {
        let cli = Cli::try_parse_from([
            "koe",
            "transcribe",
            "--format",
            "srt",
            "--locale",
            "ja-JP",
            "--start-at",
            "30s",
            "meeting.ogg",
        ])
        .expect("parse");
        assert!(matches!(cli.command, Command::Transcribe(_)));
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

    #[test]
    fn exit_codes_match_spec() {
        assert_eq!(MainError::PermissionDenied("mic".into()).exit_code(), 1);
        assert_eq!(MainError::InvalidArgs("bad".into()).exit_code(), 2);
        assert_eq!(MainError::Capture("tap".into()).exit_code(), 3);
        assert_eq!(MainError::Io("disk".into()).exit_code(), 4);
        assert_eq!(MainError::Interrupted.exit_code(), 5);
        assert_eq!(MainError::Internal("x".into()).exit_code(), 6);
    }
}
