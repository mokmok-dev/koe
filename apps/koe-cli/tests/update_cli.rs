use std::{fs, process::Command};

use tempfile::TempDir;

fn koe() -> Command {
    Command::new(env!("CARGO_BIN_EXE_koe"))
}

#[test]
fn status_json_contract_runs_through_clap_and_dispatch() {
    let root = TempDir::new().expect("temp");
    let output = koe()
        .args(["--output-format", "json", "update", "--data-root"])
        .arg(root.path())
        .arg("status")
        .output()
        .expect("run");
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(value["current_version"], serde_json::Value::Null);
}

#[test]
fn missing_consent_and_malformed_input_have_stable_stderr_contract() {
    let root = TempDir::new().expect("temp");
    let metadata = root.path().join("metadata.json");
    let target = root.path().join("target");
    fs::write(&metadata, b"not json").expect("metadata");
    fs::write(&target, b"target").expect("target");

    let without_consent = koe()
        .args(["update", "--data-root"])
        .arg(root.path())
        .args(["apply", "--metadata"])
        .arg(&metadata)
        .arg("--target")
        .arg(&target)
        .output()
        .expect("run");
    assert!(!without_consent.status.success());
    assert!(without_consent.stdout.is_empty());
    let error: serde_json::Value =
        serde_json::from_slice(&without_consent.stderr).expect("error json");
    assert_eq!(error["code"], "KOE-UPDATE-CONSENT-REQUIRED");

    let malformed = koe()
        .args(["update", "--data-root"])
        .arg(root.path())
        .args(["apply", "--metadata"])
        .arg(&metadata)
        .arg("--target")
        .arg(&target)
        .arg("--consent")
        .output()
        .expect("run");
    assert!(!malformed.status.success());
    assert!(malformed.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&malformed.stderr).expect("error json");
    assert_eq!(error["code"], "KOE-UPDATE-INPUT-FAILED");
}

#[test]
fn normal_apply_has_no_public_key_override() {
    let root = TempDir::new().expect("temp");
    let output = koe()
        .args(["update", "--data-root"])
        .arg(root.path())
        .args([
            "apply",
            "--metadata",
            "metadata.json",
            "--target",
            "target",
            "--public-key",
            "attacker-key",
            "--consent",
        ])
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument '--public-key'"));
}

#[test]
fn launch_without_an_active_version_fails_cleanly() {
    let root = TempDir::new().expect("temp");
    let output = koe()
        .args(["update", "--data-root"])
        .arg(root.path())
        .args(["launch", "--", "capabilities"])
        .output()
        .expect("run");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).expect("error json");
    assert_eq!(error["code"], "KOE-UPDATE-MISSING");
}
