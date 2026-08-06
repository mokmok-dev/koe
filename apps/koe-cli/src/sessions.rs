//! Session library operations: list, show, export, and delete.

use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use koe_core::SessionId;
use koe_core::SessionState;
use koe_recording::SessionManifest;
use serde::Serialize;
use thiserror::Error;

/// Summary of one session suitable for tabular or JSONL list output.
#[derive(Clone, Debug, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub state: String,
    pub started_at_ms: u128,
    pub ended_at_ms: Option<u128>,
    pub duration_ms: u64,
    pub source_device: String,
    pub audio_files: usize,
    pub has_transcript: bool,
}

/// Detailed session view including the manifest and transcript status.
#[derive(Clone, Debug, Serialize)]
pub struct SessionDetail {
    pub session_id: String,
    pub manifest: SessionManifest,
    pub transcript: Option<TranscriptSummary>,
}

/// Transcript metadata exposed by the CLI; never the raw transcript text.
#[derive(Clone, Debug, Serialize)]
pub struct TranscriptSummary {
    pub segment_count: usize,
    pub final_text_word_count: usize,
    pub has_final_json: bool,
    pub has_final_txt: bool,
}

/// Session library operation failure.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("session JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("session not found: {0}")]
    NotFound(SessionId),
    #[error("session path rejected")]
    PathRejected,
    #[error("active session cannot be exported or deleted")]
    Active,
    #[error("export destination rejected")]
    DestinationRejected,
    #[error("invalid session id: {0}")]
    InvalidId(String),
}

impl SessionError {
    /// Stable error code for CLI reporting.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "KOE-SESSION-IO-FAILED",
            Self::Json(_) => "KOE-SESSION-JSON-FAILED",
            Self::NotFound(_) => "KOE-SESSION-NOT-FOUND",
            Self::PathRejected => "KOE-SESSION-PATH-REJECTED",
            Self::Active => "KOE-SESSION-ACTIVE",
            Self::DestinationRejected => "KOE-SESSION-DESTINATION-REJECTED",
            Self::InvalidId(_) => "KOE-SESSION-INVALID-ID",
        }
    }
}

/// Lists all sessions under `data_root/sessions`, newest first.
///
/// # Errors
///
/// Returns an error when the data root cannot be read or a session manifest
/// cannot be parsed.
pub fn list_sessions(data_root: &Path) -> Result<Vec<SessionSummary>, SessionError> {
    let mut summaries = Vec::new();
    let sessions_dir = sessions_dir(data_root)?;
    let entries = match fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(summaries),
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
        let manifest = read_manifest(&entry.path().join("session.json"))?;
        if manifest.session_id != session_id {
            continue;
        }
        let has_transcript = transcript_summary(&entry.path()).is_some();
        summaries.push(summary_from_manifest(&manifest, has_transcript));
    }
    summaries.sort_by_key(|summary| std::cmp::Reverse(summary.started_at_ms));
    Ok(summaries)
}

/// Shows a single session by ID.
///
/// # Errors
///
/// Returns an error when the session does not exist or cannot be read.
pub fn show_session(
    data_root: &Path,
    id: &str,
) -> Result<SessionDetail, SessionError> {
    let session_id = SessionId::parse(id).map_err(|_| SessionError::InvalidId(id.to_owned()))?;
    let dir = session_dir(data_root, &session_id)?;
    if !dir.exists() {
        return Err(SessionError::NotFound(session_id));
    }
    let manifest = read_manifest(&dir.join("session.json"))?;
    let transcript = transcript_summary(&dir);
    Ok(SessionDetail {
        session_id: session_id.to_string(),
        manifest,
        transcript,
    })
}

/// Exports a session to a user-selected directory.
///
/// The export directory is created as `<destination>/<session_id>-export` and
/// contains a copy of the manifest, audio segments, and transcript files.
/// Existing export directories are not overwritten.
///
/// # Errors
///
/// Returns an error when the session is active, the destination is invalid, or
/// a copy fails.
pub fn export_session(
    data_root: &Path,
    id: &str,
    destination: &Path,
) -> Result<PathBuf, SessionError> {
    let session_id = SessionId::parse(id).map_err(|_| SessionError::InvalidId(id.to_owned()))?;
    let source_dir = session_dir(data_root, &session_id)?;
    if !source_dir.exists() {
        return Err(SessionError::NotFound(session_id));
    }
    let manifest = read_manifest(&source_dir.join("session.json"))?;
    if is_active(manifest.state) {
        return Err(SessionError::Active);
    }
    let destination = destination
        .canonicalize()
        .map_err(|_| SessionError::DestinationRejected)?;
    let source_canonical = source_dir.canonicalize()?;
    if destination.starts_with(&source_canonical) || source_canonical.starts_with(&destination) {
        return Err(SessionError::DestinationRejected);
    }
    let export_dir = destination.join(format!("{id}-export"));
    if export_dir.exists() {
        return Err(SessionError::DestinationRejected);
    }
    fs::create_dir_all(&export_dir)?;
    copy_dir_contents(&source_dir, &export_dir)?;
    Ok(export_dir)
}

/// Deletes a session directory after verifying it is under the data root and
/// not active.
///
/// # Errors
///
/// Returns an error when the session is active, not found, or cannot be
/// removed.
pub fn delete_session(
    data_root: &Path,
    id: &str,
) -> Result<(), SessionError> {
    let session_id = SessionId::parse(id).map_err(|_| SessionError::InvalidId(id.to_owned()))?;
    let dir = session_dir(data_root, &session_id)?;
    if !dir.exists() {
        return Err(SessionError::NotFound(session_id));
    }
    let manifest = read_manifest(&dir.join("session.json"))?;
    if is_active(manifest.state) {
        return Err(SessionError::Active);
    }
    let canonical = dir.canonicalize()?;
    let data_root_canonical = data_root
        .canonicalize()
        .map_err(|_| SessionError::PathRejected)?;
    if !canonical.starts_with(&data_root_canonical) {
        return Err(SessionError::PathRejected);
    }
    fs::remove_dir_all(&canonical)?;
    Ok(())
}

fn read_manifest(path: &Path) -> Result<SessionManifest, SessionError> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn transcript_summary(session_dir: &Path) -> Option<TranscriptSummary> {
    let transcript_dir = session_dir.join("transcript");
    let final_txt = transcript_dir.join("final.txt");
    let final_json = transcript_dir.join("final.json");
    let events = transcript_dir.join("events.jsonl");
    let has_final_txt = final_txt.is_file();
    let has_final_json = final_json.is_file();
    if !has_final_txt && !has_final_json && !events.is_file() {
        return None;
    }
    let word_count = if has_final_txt {
        fs::read_to_string(&final_txt).map_or(0, |text| text.split_whitespace().count())
    } else {
        0
    };
    let segment_count = if events.is_file() {
        fs::File::open(&events)
            .ok()
            .and_then(|mut file| {
                let mut text = String::new();
                file.read_to_string(&mut text).ok()?;
                Some(text.lines().filter(|line| !line.is_empty()).count())
            })
            .unwrap_or(0)
    } else {
        0
    };
    Some(TranscriptSummary {
        segment_count,
        final_text_word_count: word_count,
        has_final_json,
        has_final_txt,
    })
}

fn summary_from_manifest(
    manifest: &SessionManifest,
    has_transcript: bool,
) -> SessionSummary {
    let duration_ms = manifest.ended_unix_ms.map_or(0, |ended| {
        u64::try_from(ended.saturating_sub(manifest.started_unix_ms)).unwrap_or(u64::MAX)
    });
    let source_device = manifest.source_device_id.clone();
    SessionSummary {
        session_id: manifest.session_id.to_string(),
        state: state_label(manifest.state),
        started_at_ms: manifest.started_unix_ms,
        ended_at_ms: manifest.ended_unix_ms,
        duration_ms,
        source_device,
        audio_files: manifest.audio_files.len(),
        has_transcript,
    }
}

fn state_label(state: SessionState) -> String {
    format!("{state:?}").to_lowercase()
}

fn sessions_dir(data_root: &Path) -> Result<PathBuf, SessionError> {
    verify_dir(data_root)?;
    Ok(data_root.join("sessions"))
}

fn session_dir(
    data_root: &Path,
    session_id: &SessionId,
) -> Result<PathBuf, SessionError> {
    Ok(sessions_dir(data_root)?.join(session_id.to_string()))
}

fn verify_dir(path: &Path) -> Result<(), SessionError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SessionError::PathRejected);
    }
    Ok(())
}

const fn is_active(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::Idle
            | SessionState::Preparing
            | SessionState::Starting
            | SessionState::Recording
            | SessionState::Degraded
            | SessionState::Stopping
            | SessionState::Finalizing
    )
}

fn copy_dir_contents(
    source: &Path,
    destination: &Path,
) -> Result<(), SessionError> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        let name = entry.file_name();
        let source_path = entry.path();
        let destination_path = destination.join(&name);
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            copy_dir_contents(&source_path, &destination_path)?;
        } else {
            fs::copy(&source_path, &destination_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use koe_core::SessionId;
    use koe_core::SessionState;
    use koe_recording::SessionManifest;
    use tempfile::TempDir;

    use super::{delete_session, export_session, list_sessions, show_session};

    fn manifest(
        session_id: SessionId,
        state: SessionState,
        ended_ms_ago: u128,
    ) -> SessionManifest {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        SessionManifest {
            schema_version: 2,
            session_id,
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

    fn create_session(
        root: &TempDir,
        state: SessionState,
    ) -> SessionId {
        let id = SessionId::new();
        let dir = root.path().join("sessions").join(id.to_string());
        fs::create_dir_all(&dir).expect("session dir");
        fs::create_dir_all(dir.join("audio")).expect("audio dir");
        fs::create_dir_all(dir.join("transcript")).expect("transcript dir");
        fs::create_dir_all(dir.join("recovery")).expect("recovery dir");
        fs::write(
            dir.join("session.json"),
            serde_json::to_vec(&manifest(id, state, 0)).expect("json"),
        )
        .expect("manifest");
        id
    }

    #[test]
    fn list_empty_sessions() {
        let root = TempDir::new().expect("temp");
        fs::create_dir_all(root.path().join("sessions")).expect("sessions");
        let sessions = list_sessions(root.path()).expect("list");
        assert!(sessions.is_empty());
    }

    #[test]
    fn list_and_show_completed_session() {
        let root = TempDir::new().expect("temp");
        fs::create_dir_all(root.path().join("sessions")).expect("sessions");
        let id = create_session(&root, SessionState::Completed);
        let sessions = list_sessions(root.path()).expect("list");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, id.to_string());
        assert_eq!(sessions[0].state, "completed");
        let detail = show_session(root.path(), &id.to_string()).expect("show");
        assert_eq!(detail.session_id, id.to_string());
        assert_eq!(detail.manifest.state, SessionState::Completed);
    }

    #[test]
    fn delete_completed_session() {
        let root = TempDir::new().expect("temp");
        fs::create_dir_all(root.path().join("sessions")).expect("sessions");
        let id = create_session(&root, SessionState::Completed);
        delete_session(root.path(), &id.to_string()).expect("delete");
        assert!(!root.path().join("sessions").join(id.to_string()).exists());
    }

    #[test]
    fn delete_active_session_is_refused() {
        let root = TempDir::new().expect("temp");
        fs::create_dir_all(root.path().join("sessions")).expect("sessions");
        let id = create_session(&root, SessionState::Recording);
        let error = delete_session(root.path(), &id.to_string()).expect_err("active delete");
        assert_eq!(error.code(), "KOE-SESSION-ACTIVE");
    }

    #[test]
    fn export_completed_session_copies_files() {
        let root = TempDir::new().expect("temp");
        let export_root = TempDir::new().expect("export temp");
        fs::create_dir_all(root.path().join("sessions")).expect("sessions");
        let id = create_session(&root, SessionState::Completed);
        fs::write(
            root.path()
                .join("sessions")
                .join(id.to_string())
                .join("transcript")
                .join("final.txt"),
            "hello world",
        )
        .expect("final");
        let exported =
            export_session(root.path(), &id.to_string(), export_root.path()).expect("export");
        assert!(exported.exists());
        assert!(exported.join("session.json").exists());
        assert!(exported.join("transcript").join("final.txt").exists());
    }
}
