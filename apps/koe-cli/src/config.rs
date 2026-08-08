//! Session storage configuration, migration, and retention policy.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use koe_core::SessionState;
use koe_core::{NetworkPolicy, SessionId};
use koe_recording::SessionManifest;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const CONFIG_SCHEMA_VERSION: u32 = 1;
const CONFIG_FILE_NAME: &str = "config.json";

/// User-facing policy for how long completed sessions are kept.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionPolicy {
    /// Keep sessions until the user explicitly deletes them.
    #[default]
    Forever,
    /// Delete sessions whose `ended_unix_ms` is older than this many days.
    Days(u32),
}

/// Stored configuration for a `koe` data root.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Config {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    #[serde(default)]
    pub retention: RetentionPolicy,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub offline_policy: NetworkPolicy,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            retention: RetentionPolicy::default(),
            defaults: Defaults::default(),
            offline_policy: NetworkPolicy::Denied,
        }
    }
}

const fn default_schema_version() -> u32 {
    CONFIG_SCHEMA_VERSION
}

/// Default sources and model selected by the user for quick record commands.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Defaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphone_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_audio_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_selector: Option<String>,
}

/// Configuration file operation failure.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("config serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported config schema version {0}")]
    UnsupportedSchema(u32),
    #[error("data root path rejected")]
    PathRejected,
}

impl ConfigError {
    /// Stable error code for CLI reporting.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "KOE-CONFIG-IO-FAILED",
            Self::Json(_) => "KOE-CONFIG-JSON-FAILED",
            Self::UnsupportedSchema(_) => "KOE-CONFIG-UNSUPPORTED-SCHEMA",
            Self::PathRejected => "KOE-CONFIG-PATH-REJECTED",
        }
    }
}

/// Reads the configuration file or returns defaults for a fresh data root.
///
/// Older schemas are migrated in-place before returning.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be parsed or its schema
/// is unsupported.
pub fn load_or_migrate(data_root: &Path) -> Result<Config, ConfigError> {
    let path = config_path(data_root)?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let bytes = fs::read(&path)?;
    let mut config: Config = serde_json::from_slice(&bytes)?;
    match config.schema_version {
        0 => {
            // Version 0 had no explicit retention policy. Keep sessions
            // forever and publish the current schema.
            config.retention = RetentionPolicy::Forever;
            config.schema_version = CONFIG_SCHEMA_VERSION;
            save(data_root, &config)?;
        },
        1 => {},
        version => return Err(ConfigError::UnsupportedSchema(version)),
    }
    Ok(config)
}

/// Persists the configuration to the data root atomically.
///
/// # Errors
///
/// Returns an error when the directory cannot be created or the file cannot
/// be written.
pub fn save(
    data_root: &Path,
    config: &Config,
) -> Result<(), ConfigError> {
    let path = config_path(data_root)?;
    let temp = temp_config_path(data_root);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp)?;
    serde_json::to_writer_pretty(&mut file, config)?;
    file.flush()?;
    drop(file);
    fs::rename(temp, path)?;
    Ok(())
}

/// Applies the configured retention policy and returns deleted session IDs.
///
/// Active sessions (states that are not terminal) are never deleted. Export
/// in progress is not tracked by the CLI store, so callers should check the
/// destination directory before running this.
///
/// # Errors
///
/// Returns an error when the session directory cannot be scanned or a session
/// cannot be deleted.
pub fn apply_retention(
    data_root: &Path,
    config: &Config,
) -> Result<Vec<SessionId>, ConfigError> {
    let candidates = retention_candidates(data_root, config)?;
    let mut deleted = Vec::with_capacity(candidates.len());
    for session_id in candidates {
        delete_session_directory(data_root, &session_id)?;
        deleted.push(session_id);
    }
    Ok(deleted)
}

/// Lists sessions eligible for retention without deleting anything.
///
/// # Errors
///
/// Returns an error when the session directory cannot be scanned or a
/// manifest cannot be parsed.
pub fn retention_candidates(
    data_root: &Path,
    config: &Config,
) -> Result<Vec<SessionId>, ConfigError> {
    let RetentionPolicy::Days(days) = config.retention else {
        return Ok(Vec::new());
    };
    let cutoff = unix_millis().saturating_sub(u128::from(days) * 24 * 60 * 60 * 1_000);
    let sessions_dir = sessions_dir(data_root)?;
    let mut candidates = Vec::new();
    let entries = match fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(session_id) = SessionId::parse(&name) else {
            continue;
        };
        let manifest_path = entry.path().join("session.json");
        let manifest: SessionManifest = match fs::read(&manifest_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(_) => continue,
        };
        if is_active(manifest.state) {
            continue;
        }
        let ended = manifest.ended_unix_ms.unwrap_or(manifest.started_unix_ms);
        if ended < cutoff {
            candidates.push(session_id);
        }
    }
    Ok(candidates)
}

const fn is_active(state: SessionState) -> bool {
    !state.is_terminal()
}

/// Removes a session directory after verifying it lives under the data root.
///
/// # Errors
///
/// Returns an error when the path is rejected or removal fails.
pub fn delete_session_directory(
    data_root: &Path,
    session_id: &SessionId,
) -> Result<(), ConfigError> {
    let expected = sessions_dir(data_root)?.join(session_id.to_string());
    let canonical_expected = expected.canonicalize()?;
    let canonical_data_root = data_root.canonicalize()?;
    if !canonical_expected.starts_with(&canonical_data_root) {
        return Err(ConfigError::PathRejected);
    }
    fs::remove_dir_all(&canonical_expected)?;
    Ok(())
}

fn config_path(data_root: &Path) -> Result<PathBuf, ConfigError> {
    ensure_data_root(data_root)?;
    Ok(data_root.join(CONFIG_FILE_NAME))
}

fn temp_config_path(data_root: &Path) -> PathBuf {
    data_root.join(format!(".{CONFIG_FILE_NAME}.tmp"))
}

fn sessions_dir(data_root: &Path) -> Result<PathBuf, ConfigError> {
    ensure_data_root(data_root)?;
    Ok(data_root.join("sessions"))
}

fn ensure_data_root(data_root: &Path) -> Result<(), ConfigError> {
    let metadata = fs::symlink_metadata(data_root)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ConfigError::PathRejected);
    }
    Ok(())
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use koe_core::SessionState;
    use tempfile::TempDir;

    use std::fs;

    use super::{
        Config, RetentionPolicy, apply_retention, load_or_migrate, retention_candidates, save,
    };

    fn manifest(
        state: SessionState,
        ended_ms_ago: u128,
    ) -> koe_recording::SessionManifest {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        koe_recording::SessionManifest {
            schema_version: 2,
            session_id: koe_core::SessionId::new(),
            state,
            started_unix_ms: now.saturating_sub(ended_ms_ago),
            ended_unix_ms: Some(now.saturating_sub(ended_ms_ago)),
            app_version: "0.1.0".to_owned(),
            platform: "test".to_owned(),
            backend: "test".to_owned(),
            source_device_id: "fixture".to_owned(),
            permission_result: "granted".to_owned(),
            sample_rate: 16_000,
            channels: 1,
            native_sample_format: "signed-16-bit-pcm".to_owned(),
            stored_sample_format: "wav-pcm-s16le".to_owned(),
            timeline_unit: "microsecond".to_owned(),
            normalization: "none".to_owned(),
            mix: "isolated-microphone".to_owned(),
            discontinuities: Vec::new(),
            consent_record: "fresh-application-consent".to_owned(),
            queue_capacity: 64,
            overflow_count: 0,
            network_policy: koe_core::NetworkPolicy::Denied,
            audio_files: Vec::new(),
            failure_code: None,
            gaps: Vec::new(),
            drift_corrections: Vec::new(),
            sources: Vec::new(),
            timeline_blocks: Vec::new(),
            alignment_quality: "exact_block_timeline".to_owned(),
        }
    }

    #[test]
    fn fresh_root_returns_default_config() {
        let root = TempDir::new().expect("temp");
        let config = load_or_migrate(root.path()).expect("load");
        assert_eq!(config.schema_version, super::CONFIG_SCHEMA_VERSION);
        assert_eq!(config.retention, RetentionPolicy::Forever);
    }

    #[test]
    fn round_trip_persists_config() {
        let root = TempDir::new().expect("temp");
        let config = Config {
            retention: RetentionPolicy::Days(7),
            ..Config::default()
        };
        save(root.path(), &config).expect("save");
        let loaded = load_or_migrate(root.path()).expect("load");
        assert_eq!(loaded.retention, RetentionPolicy::Days(7));
    }

    #[test]
    fn retention_deletes_old_terminal_sessions() {
        let root = TempDir::new().expect("temp");
        let sessions = root.path().join("sessions");
        fs::create_dir_all(&sessions).expect("sessions dir");
        let old_id = koe_core::SessionId::new();
        let old_dir = sessions.join(old_id.to_string());
        fs::create_dir_all(&old_dir).expect("old dir");
        let old_manifest = manifest(SessionState::Completed, 10 * 24 * 60 * 60 * 1_000);
        fs::write(
            old_dir.join("session.json"),
            serde_json::to_vec(&old_manifest).expect("json"),
        )
        .expect("write");
        let fresh_id = koe_core::SessionId::new();
        let fresh_dir = sessions.join(fresh_id.to_string());
        fs::create_dir_all(&fresh_dir).expect("fresh dir");
        let fresh_manifest = manifest(SessionState::Completed, 1_000);
        fs::write(
            fresh_dir.join("session.json"),
            serde_json::to_vec(&fresh_manifest).expect("json"),
        )
        .expect("write");

        let config = Config {
            retention: RetentionPolicy::Days(7),
            ..Config::default()
        };
        let preview = retention_candidates(root.path(), &config).expect("preview");
        assert_eq!(preview, vec![old_id]);
        assert!(old_dir.exists(), "preview must not delete data");
        let deleted = apply_retention(root.path(), &config).expect("apply");
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0], old_id);
        assert!(!old_dir.exists());
        assert!(fresh_dir.exists());
    }

    #[test]
    fn retention_skips_active_sessions() {
        let root = TempDir::new().expect("temp");
        let sessions = root.path().join("sessions");
        fs::create_dir_all(&sessions).expect("sessions dir");
        let id = koe_core::SessionId::new();
        let dir = sessions.join(id.to_string());
        fs::create_dir_all(&dir).expect("dir");
        let old_manifest = manifest(SessionState::Recording, 10 * 24 * 60 * 60 * 1_000);
        fs::write(
            dir.join("session.json"),
            serde_json::to_vec(&old_manifest).expect("json"),
        )
        .expect("write");

        let config = Config {
            retention: RetentionPolicy::Days(7),
            ..Config::default()
        };
        let deleted = apply_retention(root.path(), &config).expect("apply");
        assert!(deleted.is_empty());
        assert!(dir.exists());
    }

    #[test]
    fn retention_protects_every_nonterminal_state() {
        for state in [
            SessionState::Idle,
            SessionState::Preparing,
            SessionState::PermissionRequired,
            SessionState::Starting,
            SessionState::Recording,
            SessionState::Degraded,
            SessionState::Stopping,
            SessionState::Finalizing,
            SessionState::Cancelling,
        ] {
            let root = TempDir::new().expect("temp");
            let sessions = root.path().join("sessions");
            fs::create_dir_all(&sessions).expect("sessions dir");
            let id = koe_core::SessionId::new();
            let dir = sessions.join(id.to_string());
            fs::create_dir_all(&dir).expect("dir");
            fs::write(
                dir.join("session.json"),
                serde_json::to_vec(&manifest(state, 10 * 24 * 60 * 60 * 1_000)).expect("json"),
            )
            .expect("write");
            let config = Config {
                retention: RetentionPolicy::Days(7),
                ..Config::default()
            };
            assert!(
                retention_candidates(root.path(), &config)
                    .expect("candidates")
                    .is_empty(),
                "state {state:?} must be protected"
            );
            assert!(dir.exists());
        }
    }
}
