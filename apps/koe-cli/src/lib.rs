//! The `koe` command-line application.

#![cfg_attr(
    test,
    allow(
        clippy::assert_is_empty,
        clippy::expect_used,
        clippy::float_cmp,
        clippy::panic,
        clippy::significant_drop_tightening,
        clippy::struct_excessive_bools,
        clippy::trivially_copy_pass_by_ref,
        clippy::unwrap_used
    )
)]

use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use clap::Parser;

use crate::cli::{Cli, Command};
use crate::error::AppError;
use crate::host::{Host, RealHost, install_interrupt};
use crate::io::Io;

mod cli;
mod devices;
mod error;
mod host;
mod io;
mod meeting;
mod model;
mod record;
mod transcribe;
mod transcript;
mod wav;

#[cfg(test)]
mod fakes;

/// Runs the command line and returns the process exit code.
#[must_use = "the process must exit with the returned code"]
pub fn main() -> ExitCode {
    let interrupt = Arc::new(AtomicBool::new(false));
    let _ = install_interrupt(&interrupt);
    run(std::env::args_os(), &interrupt)
}

fn run(args: impl IntoIterator<Item = OsString>, interrupt: &Arc<AtomicBool>) -> ExitCode {
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let mut io = Io {
        out: &mut out,
        err: &mut err,
    };
    run_with(args, &RealHost, interrupt, &mut io)
}

fn run_with(
    args: impl IntoIterator<Item = OsString>,
    host: &dyn Host,
    interrupt: &Arc<AtomicBool>,
    io: &mut Io<'_>,
) -> ExitCode {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => return cli::report(&error, io),
    };
    match execute(&cli, host, interrupt, io) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io.err, "error: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

fn execute(
    cli: &Cli,
    host: &dyn Host,
    interrupt: &Arc<AtomicBool>,
    io: &mut Io<'_>,
) -> Result<(), AppError> {
    match &cli.command {
        Command::Devices => devices::execute(host, io),
        Command::Record(args) => record::execute(args, host, interrupt, io),
        Command::Meeting(args) => meeting::execute(args, host, interrupt, io),
        Command::Transcribe(args) => transcribe::execute(args, host, io),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::ffi::OsString;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use koe_capture::{DeviceId, Source};

    use super::*;
    use crate::fakes::FakeHost;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(|item| OsString::from(*item)).collect()
    }

    fn run_with_fake(items: &[&str], host: &FakeHost) -> (ExitCode, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
        };
        let interrupt = Arc::new(AtomicBool::new(false));
        let code = run_with(args(items), host, &interrupt, &mut io);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn device() -> koe_capture::DeviceInfo {
        koe_capture::DeviceInfo::new(
            DeviceId::new("mic").unwrap(),
            "Microphone",
            Source::Microphone,
            true,
            48_000,
            1,
        )
    }

    #[test]
    fn help_goes_to_stdout_and_exits_zero() {
        let (code, out, err) = run_with_fake(&["koe", "--help"], &FakeHost::default());
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(out.contains("Usage"));
        assert!(err.is_empty());
    }

    #[test]
    fn a_parse_failure_goes_to_stderr_and_exits_two() {
        let (code, out, err) = run_with_fake(&["koe", "speak"], &FakeHost::default());
        assert_eq!(code, ExitCode::from(2));
        assert!(out.is_empty());
        assert!(err.contains("unrecognized subcommand"));
    }

    #[test]
    fn devices_dispatches_to_the_device_list() {
        let host = FakeHost::with_devices(vec![device()]);
        let (code, out, err) = run_with_fake(&["koe", "devices"], &host);
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(out.contains("Microphone"));
        assert!(err.is_empty());
    }

    #[test]
    fn a_device_error_is_printed_and_exits_one() {
        let host = FakeHost::without_devices();
        let (code, out, err) = run_with_fake(&["koe", "devices"], &host);
        assert_eq!(code, ExitCode::FAILURE);
        assert!(out.is_empty());
        assert!(err.contains("error: capture failed"));
    }

    #[test]
    fn a_usage_error_exits_two() {
        let (code, out, err) = run_with_fake(&["koe", "record", "take.mp3"], &FakeHost::default());
        assert_eq!(code, ExitCode::from(2));
        assert!(out.is_empty());
        assert!(err.contains("no recording format"));
    }

    #[test]
    fn a_runtime_error_exits_one() {
        let (code, out, err) = run_with_fake(
            &[
                "koe",
                "transcribe",
                "/nonexistent/take.wav",
                "--model",
                "m.gguf",
            ],
            &FakeHost::default(),
        );
        assert_eq!(code, ExitCode::FAILURE);
        assert!(out.is_empty());
        assert!(err.contains("error: audio file"));
    }
}
