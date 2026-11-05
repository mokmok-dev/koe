//! End-to-end checks of the `koe` binary.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::{Command, Output, Stdio};

fn koe(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_koe"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run the koe binary")
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("the process exited normally")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn assert_success(output: &Output) {
    assert_eq!(code(output), 0, "stderr: {}", stderr(output));
}

fn assert_failure(output: &Output, expected: i32) {
    assert_eq!(code(output), expected, "stdout: {}", stdout(output));
}

#[test]
fn help_names_every_command_and_exits_zero() {
    let output = koe(&["--help"]);
    assert_success(&output);
    let text = stdout(&output);
    for command in ["devices", "record", "transcribe"] {
        assert!(text.contains(command), "help must name `{command}`");
    }
}

#[test]
fn version_prints_the_package_version() {
    let output = koe(&["--version"]);
    assert_success(&output);
    assert!(stdout(&output).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn no_subcommand_is_a_parse_failure() {
    let output = koe(&[]);
    assert_failure(&output, 2);
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(text.contains("Usage"));
}

#[test]
fn an_unknown_subcommand_is_a_parse_failure() {
    let output = koe(&["speak"]);
    assert_failure(&output, 2);
    assert!(stderr(&output).contains("unrecognized subcommand"));
}

#[test]
fn an_unknown_output_extension_is_a_usage_error() {
    let output = koe(&["record", "take.mp3"]);
    assert_failure(&output, 2);
    assert!(stderr(&output).contains("no recording format"));
}

#[test]
fn a_zero_duration_is_a_usage_error() {
    let output = koe(&["record", "take.ogg", "--duration", "0"]);
    assert_failure(&output, 2);
    assert!(stderr(&output).contains("positive duration"));
}

#[test]
fn an_out_of_range_gain_is_a_usage_error() {
    let output = koe(&["record", "take.ogg", "--gain", "100"]);
    assert_failure(&output, 2);
    assert!(stderr(&output).contains("outside the accepted range"));
}

#[test]
fn a_missing_input_file_is_a_runtime_error() {
    let output = koe(&[
        "transcribe",
        "/nonexistent/koe-take.wav",
        "--model",
        "model.gguf",
    ]);
    assert_failure(&output, 1);
    assert!(stderr(&output).contains("audio file"));
}

#[test]
fn a_non_wave_input_is_a_runtime_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("take.wav");
    std::fs::write(&path, b"not a wave").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_koe"))
        .args([
            "transcribe",
            path.to_str().unwrap(),
            "--model",
            "model.gguf",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_failure(&output, 1);
    assert!(stderr(&output).contains("not a RIFF/WAVE file"));
}

#[cfg(not(any(feature = "linux", feature = "macos", feature = "windows")))]
#[test]
fn devices_without_a_backend_reports_unsupported() {
    let output = koe(&["devices"]);
    assert_failure(&output, 1);
    assert!(stderr(&output).contains("no audio capture backend is enabled"));
}

#[cfg(not(any(feature = "linux", feature = "macos", feature = "windows")))]
#[test]
fn a_record_without_a_backend_still_leaves_a_readable_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("take.wav");
    let output = Command::new(env!("CARGO_BIN_EXE_koe"))
        .args([
            "record",
            path.to_str().unwrap(),
            "--source",
            "microphone",
            "--duration",
            "0.1",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_failure(&output, 1);
    assert!(stderr(&output).contains("no audio capture backend is enabled"));
    assert_riff(&path);
}

#[cfg(not(any(feature = "linux", feature = "macos", feature = "windows")))]
fn assert_riff(path: &std::path::Path) {
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
}
