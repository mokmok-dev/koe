//! Signed update status, apply, and rollback for the koe CLI.
//!
//! Milestone 7 (`spec/08-roadmap.md`): update metadata is TUF-style signed,
//! versioned and expiring; the store keeps the previous version for rollback
//! and rejects expired, replayed, foreign-platform or tampered inputs. All
//! operations here are strictly offline — a release is fetched out of band
//! and verified from signed metadata and the publisher key compiled into koe.

use std::{
    fs,
    io::{self, Read, Write},
    path::Path,
};

/// Publisher trust root compiled into this binary. Production release builds
/// set `KOE_UPDATE_PUBLIC_KEY_HEX`; the fallback is a non-rotatable development
/// key whose private scalar is not present in this repository.
const DEVELOPMENT_UPDATE_PUBLIC_KEY_HEX: &str =
    "e95e4f0ca47820f0bea85d4ad4cc4b10289ef1b7680012da01087734367ba26a";

fn trusted_update_public_key_hex() -> &'static str {
    option_env!("KOE_UPDATE_PUBLIC_KEY_HEX")
        .filter(|key| !key.is_empty())
        .unwrap_or(DEVELOPMENT_UPDATE_PUBLIC_KEY_HEX)
}

use koe_update::{MAX_METADATA_BYTES, UpdateError, UpdateStatus, UpdateStore, parse_signed_update};
use thiserror::Error;

/// Update command failures.
#[derive(Debug, Error)]
pub enum UpdateCliError {
    #[error("update store failed: {0}")]
    Update(#[from] UpdateError),
    #[error("update input I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("update metadata could not be parsed or exceeds supported limits")]
    MalformedMetadata,
    #[error("update output could not be encoded: {0}")]
    Json(#[from] serde_json::Error),
    #[error("fresh consent is required to apply an update; review the release and pass --consent")]
    ConsentRequired,
    #[error("the active updated executable exited unsuccessfully")]
    LaunchFailed,
}

impl UpdateCliError {
    /// Stable error code for CLI reporting.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Update(error) => error.code(),
            Self::Io(_) | Self::MalformedMetadata | Self::Json(_) => "KOE-UPDATE-INPUT-FAILED",
            Self::ConsentRequired => "KOE-UPDATE-CONSENT-REQUIRED",
            Self::LaunchFailed => "KOE-UPDATE-LAUNCH-FAILED",
        }
    }
}

/// Shows the durable update state without network access.
///
/// # Errors
///
/// Returns an error when the update store cannot be read.
pub fn status(
    data_root: &Path,
    format: crate::OutputFormat,
    output: &mut impl Write,
) -> Result<(), UpdateCliError> {
    let store = UpdateStore::open(data_root)?;
    let status = store.status()?;
    render_status(&status, format, output)
}

/// Verifies a signed release (metadata + artifact) and installs it side by
/// side, keeping the previous version for rollback.
///
/// Requires fresh `consent`; otherwise the update is refused before any
/// filesystem state changes.
///
/// # Errors
///
/// Returns an error for missing consent, unreadable inputs, invalid
/// signatures, expired/replayed/foreign metadata, and digest mismatches.
pub fn apply_update(
    data_root: &Path,
    metadata: &Path,
    target: &Path,
    consent: bool,
    format: crate::OutputFormat,
    output: &mut impl Write,
) -> Result<(), UpdateCliError> {
    if !consent {
        return Err(UpdateCliError::ConsentRequired);
    }
    apply_update_with_key(
        data_root,
        metadata,
        target,
        trusted_update_public_key_hex(),
        format,
        output,
    )
}

fn apply_update_with_key(
    data_root: &Path,
    metadata: &Path,
    target: &Path,
    trusted_key: &str,
    format: crate::OutputFormat,
    output: &mut impl Write,
) -> Result<(), UpdateCliError> {
    let bytes = read_metadata(metadata)?;
    let signed = parse_signed_update(&bytes).map_err(|_| UpdateCliError::MalformedMetadata)?;
    let store = UpdateStore::open(data_root)?;
    store.verify_release_artifact(&signed, &signed.payload.install_target, target, trusted_key)?;
    if store.active_versions()?.0.is_none() {
        let running = std::env::current_exe()?;
        let shipped_version = if signed.payload.app_version == env!("CARGO_PKG_VERSION") {
            format!("{}-shipped", env!("CARGO_PKG_VERSION"))
        } else {
            env!("CARGO_PKG_VERSION").to_owned()
        };
        store.bootstrap_fallback(&shipped_version, &running)?;
    }
    let status = store.install(&signed, target, trusted_key)?;
    render_status(&status, format, output)
}

/// Verifies a specifically named inventory artifact against pinned metadata.
///
/// # Errors
///
/// Returns an input, signature, target binding, or digest error.
pub fn verify_release_artifact(
    data_root: &Path,
    metadata: &Path,
    target: &Path,
    target_name: &str,
) -> Result<(), UpdateCliError> {
    let bytes = read_metadata(metadata)?;
    let signed = parse_signed_update(&bytes).map_err(|_| UpdateCliError::MalformedMetadata)?;
    let store = UpdateStore::open(data_root)?;
    store.verify_release_artifact(
        &signed,
        target_name,
        target,
        trusted_update_public_key_hex(),
    )?;
    Ok(())
}

/// Restores the previously installed version after verifying it at rest.
///
/// # Errors
///
/// Returns an error when no previous version exists or the stored artifact
/// fails verification.
pub fn rollback(
    data_root: &Path,
    format: crate::OutputFormat,
    output: &mut impl Write,
) -> Result<(), UpdateCliError> {
    rollback_with_key(data_root, trusted_update_public_key_hex(), format, output)
}

fn rollback_with_key(
    data_root: &Path,
    trusted_key: &str,
    format: crate::OutputFormat,
    output: &mut impl Write,
) -> Result<(), UpdateCliError> {
    let store = UpdateStore::open(data_root)?;
    let status = store.rollback(trusted_key)?;
    render_status(&status, format, output)
}

/// Verifies and executes the active side-by-side application target. This is
/// the launcher/activation mechanism used after apply and rollback.
///
/// # Errors
///
/// Returns a verification, spawn, or unsuccessful-child error.
pub fn launch(
    data_root: &Path,
    args: &[String],
) -> Result<(), UpdateCliError> {
    let store = UpdateStore::open(data_root)?;
    let status = store.run_active(trusted_update_public_key_hex(), args)?;
    if status.success() {
        Ok(())
    } else {
        Err(UpdateCliError::LaunchFailed)
    }
}

fn read_metadata(path: &Path) -> Result<Vec<u8>, UpdateCliError> {
    let file = fs::File::open(path)?;
    if file.metadata()?.len() > MAX_METADATA_BYTES {
        return Err(UpdateError::InvalidMetadata.into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_METADATA_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_err(|_| UpdateError::InvalidMetadata)? > MAX_METADATA_BYTES {
        Err(UpdateError::InvalidMetadata.into())
    } else {
        Ok(bytes)
    }
}

fn render_status(
    status: &UpdateStatus,
    format: crate::OutputFormat,
    output: &mut impl Write,
) -> Result<(), UpdateCliError> {
    match format {
        crate::OutputFormat::Human => {
            let current = status.current_version.as_deref().unwrap_or("none");
            let previous = status.previous_version.as_deref().unwrap_or("none");
            writeln!(output, "current: {current}")?;
            writeln!(output, "previous: {previous}")?;
            writeln!(
                output,
                "installed versions: {}",
                status.installed_versions.join(", ")
            )?;
            writeln!(
                output,
                "last verified metadata version: {}",
                status.last_verified_update_version
            )?;
            if let Some(app_version) = &status.last_verified_app_version {
                writeln!(output, "last verified app version: {app_version}")?;
            }
        },
        crate::OutputFormat::Json => serde_json::to_writer_pretty(&mut *output, &status)?,
        crate::OutputFormat::Jsonl => serde_json::to_writer(&mut *output, &status)?,
    }
    if !matches!(format, crate::OutputFormat::Human) {
        writeln!(output)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use koe_update::{
        UpdateMetadata, UpdateTarget, built_in_target_triple, file_digest, public_key_hex,
        sign_update,
    };
    use tempfile::TempDir;

    use super::{apply_update, apply_update_with_key, rollback_with_key, status};
    use crate::OutputFormat;

    const SEED: [u8; 32] = [11_u8; 32];

    fn write_signed_release(
        root: &TempDir,
        artifact_bytes: &[u8],
        app_version: &str,
        metadata_version: u64,
    ) -> (PathBuf, PathBuf) {
        let artifact = root.path().join(format!("release-{app_version}.bin"));
        fs::write(&artifact, artifact_bytes).expect("artifact");
        let (sha256, size) = file_digest(&artifact).expect("digest");
        let payload = UpdateMetadata {
            schema_version: 1,
            role: "targets".to_owned(),
            version: metadata_version,
            expires_at_unix_s: u64::MAX,
            app_version: app_version.to_owned(),
            platform: built_in_target_triple(),
            install_target: if cfg!(windows) {
                format!("koe-cli-{}.exe", built_in_target_triple())
            } else {
                format!("koe-cli-{}", built_in_target_triple())
            },
            targets: vec![UpdateTarget {
                path: if cfg!(windows) {
                    format!("koe-cli-{}.exe", built_in_target_triple())
                } else {
                    format!("koe-cli-{}", built_in_target_triple())
                },
                sha256,
                size,
            }],
        };
        let signed = sign_update(&payload, &SEED).expect("sign");
        let metadata = root.path().join(format!("metadata-{app_version}.json"));
        fs::write(&metadata, serde_json::to_vec(&signed).expect("json")).expect("write");
        (metadata, artifact)
    }

    fn run_json(
        invoke: impl FnOnce(&mut Vec<u8>) -> Result<(), super::UpdateCliError>
    ) -> serde_json::Value {
        let mut output = Vec::new();
        invoke(&mut output).expect("run");
        serde_json::from_slice(&output).expect("json")
    }

    #[test]
    fn status_on_fresh_root_reports_no_update() {
        let root = TempDir::new().expect("temp");
        let value = run_json(|output| status(root.path(), OutputFormat::Json, output));
        assert_eq!(value["current_version"], serde_json::Value::Null);
        assert_eq!(value["last_verified_update_version"], 0);
    }

    #[test]
    fn apply_requires_fresh_consent() {
        let root = TempDir::new().expect("temp");
        let mut output = Vec::new();
        let error = apply_update(
            root.path(),
            &root.path().join("m.json"),
            &root.path().join("t.bin"),
            false,
            OutputFormat::Json,
            &mut output,
        )
        .expect_err("consent");
        assert_eq!(error.code(), "KOE-UPDATE-CONSENT-REQUIRED");
    }

    #[test]
    fn apply_then_rollback_surfaces_previous() {
        let root = TempDir::new().expect("temp");
        let (first_metadata, first_artifact) =
            write_signed_release(&root, b"release one", "0.1.0", 1);
        let first = run_json(|output| {
            apply_update_with_key(
                root.path(),
                &first_metadata,
                &first_artifact,
                &public_key_hex(&SEED),
                OutputFormat::Json,
                output,
            )
        });
        assert_eq!(first["current_version"], "0.1.0");
        assert_eq!(first["previous_version"], env!("CARGO_PKG_VERSION"));

        let (second_metadata, second_artifact) =
            write_signed_release(&root, b"release two", "0.2.0", 2);
        let second = run_json(|output| {
            apply_update_with_key(
                root.path(),
                &second_metadata,
                &second_artifact,
                &public_key_hex(&SEED),
                OutputFormat::Json,
                output,
            )
        });
        assert_eq!(second["current_version"], "0.2.0");
        assert_eq!(second["previous_version"], "0.1.0");

        let rolled = run_json(|output| {
            rollback_with_key(
                root.path(),
                &public_key_hex(&SEED),
                OutputFormat::Json,
                output,
            )
        });
        assert_eq!(rolled["current_version"], "0.1.0");
        assert_eq!(rolled["previous_version"], "0.2.0");
    }
}
