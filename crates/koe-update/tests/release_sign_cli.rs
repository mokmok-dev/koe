use std::{fs, process::Command};

use tempfile::TempDir;

const SEED_HEX: &str = "0707070707070707070707070707070707070707070707070707070707070707";
const PLATFORM: &str = "x86_64-unknown-linux-gnu";
const INSTALL_TARGET: &str = "koe-cli-x86_64-unknown-linux-gnu";

fn signer() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_koe-release-sign"));
    command.env("KOE_UPDATE_SIGNING_SEED_HEX", SEED_HEX);
    command
}

#[test]
fn keys_sign_verify_interoperate_and_tampering_fails() {
    let root = TempDir::new().expect("temp");
    let artifacts = root.path().join("artifacts");
    fs::create_dir(&artifacts).expect("artifacts");
    fs::write(artifacts.join(INSTALL_TARGET), b"executable").expect("target");
    fs::create_dir(artifacts.join("nested")).expect("nested");
    fs::write(artifacts.join("nested/notice"), b"notice").expect("notice");
    let metadata = root.path().join("metadata.json");

    let keys = signer().arg("keys").output().expect("keys");
    assert!(keys.status.success());
    let public_key = String::from_utf8(keys.stdout).expect("utf8");
    let public_key = public_key.trim();

    let status = signer()
        .args([
            "sign",
            "--app-version",
            "1.2.3",
            "--platform",
            PLATFORM,
            "--install-target",
            INSTALL_TARGET,
            "--expires-unix-s",
            "18446744073709551615",
            "--metadata-version",
            "1",
            "--artifact-dir",
        ])
        .arg(&artifacts)
        .arg("--out")
        .arg(&metadata)
        .status()
        .expect("sign");
    assert!(status.success());

    let verified = signer()
        .args(["verify", "--metadata"])
        .arg(&metadata)
        .args([
            "--public-key",
            public_key,
            "--expected-platform",
            PLATFORM,
            "--now-unix-s",
            "199",
        ])
        .output()
        .expect("verify");
    assert!(verified.status.success());

    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&metadata).expect("read")).expect("json");
    value["payload"]["app_version"] = serde_json::json!("9.9.9");
    fs::write(&metadata, serde_json::to_vec(&value).expect("json")).expect("tamper");
    let rejected = signer()
        .args(["verify", "--metadata"])
        .arg(&metadata)
        .args([
            "--public-key",
            public_key,
            "--expected-platform",
            PLATFORM,
            "--now-unix-s",
            "199",
        ])
        .output()
        .expect("verify");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("KOE-UPDATE-SIGNATURE-INVALID"));
}

#[test]
fn signing_seed_is_not_accepted_as_a_process_argument() {
    let output = signer()
        .args(["keys", "--seed-hex", SEED_HEX])
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
}
