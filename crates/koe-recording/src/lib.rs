//! Crash-aware segmented PCM WAV storage.

use std::{
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions as CapOpenOptions},
};
use hound::{SampleFormat, WavSpec, WavWriter};
use koe_core::{NetworkPolicy, SessionId, SessionState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const MANIFEST_SCHEMA: u32 = 1;
const ACTIVE_SCHEMA: u32 = 1;

/// Configuration frozen before the first audio writer is opened.
#[derive(Clone, Debug)]
pub struct RecordingConfig {
    /// App-owned data root. Sessions are always written below `sessions/`.
    pub data_root: PathBuf,
    /// Complete interleaved sample values per file.
    pub samples_per_segment: u64,
    /// Native PCM sample rate.
    pub sample_rate: u32,
    /// Native PCM channel count.
    pub channels: u16,
    /// Native device payload format before adapter conversion.
    pub native_sample_format: String,
    /// Bounded handoff capacity recorded for diagnostics.
    pub queue_capacity: usize,
    /// Explicit session network policy.
    pub network_policy: NetworkPolicy,
    /// Runtime adapter that produced the native PCM.
    pub backend: String,
    /// Opaque device ID selected explicitly by the caller.
    pub source_device_id: String,
    /// Stable result of the OS permission/open probe.
    pub permission_result: String,
}

impl RecordingConfig {
    /// Creates the initial microphone defaults with 15-minute segments.
    #[must_use]
    pub fn microphone(
        data_root: impl Into<PathBuf>,
        sample_rate: u32,
        channels: u16,
    ) -> Self {
        let samples_per_segment = u64::from(sample_rate) * u64::from(channels) * 15_u64 * 60_u64;
        Self {
            data_root: data_root.into(),
            samples_per_segment,
            sample_rate,
            channels,
            native_sample_format: "signed-16-bit-pcm".to_owned(),
            queue_capacity: 64,
            network_policy: NetworkPolicy::Denied,
            backend: "cpal".to_owned(),
            source_device_id: "unspecified".to_owned(),
            permission_result: "granted".to_owned(),
        }
    }

    const fn validate(&self) -> Result<(), RecordingError> {
        if self.samples_per_segment == 0
            || self.sample_rate == 0
            || self.channels == 0
            || self.queue_capacity == 0
            || !self
                .samples_per_segment
                .is_multiple_of(self.channels as u64)
        {
            return Err(RecordingError::InvalidConfiguration);
        }
        Ok(())
    }
}

/// Durable description of one WAV segment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AudioFile {
    pub path: String,
    pub sample_count: u64,
    #[serde(default)]
    pub timeline_start_sample: u64,
    #[serde(default)]
    pub timeline_end_sample: u64,
    pub size: u64,
    pub sha256: String,
}

/// Versioned Milestone 1 session manifest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionManifest {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub state: SessionState,
    pub started_unix_ms: u128,
    pub ended_unix_ms: Option<u128>,
    pub app_version: String,
    pub platform: String,
    pub backend: String,
    pub source_device_id: String,
    pub permission_result: String,
    pub sample_rate: u32,
    pub channels: u16,
    #[serde(default = "default_native_sample_format")]
    pub native_sample_format: String,
    #[serde(default = "default_stored_sample_format")]
    pub stored_sample_format: String,
    #[serde(default = "default_timeline_unit")]
    pub timeline_unit: String,
    #[serde(default = "default_normalization")]
    pub normalization: String,
    #[serde(default = "default_mix")]
    pub mix: String,
    #[serde(default)]
    pub discontinuities: Vec<u64>,
    #[serde(default = "default_consent_record")]
    pub consent_record: String,
    pub queue_capacity: usize,
    pub overflow_count: u64,
    pub network_policy: NetworkPolicy,
    pub audio_files: Vec<AudioFile>,
    pub failure_code: Option<String>,
}

fn default_native_sample_format() -> String {
    "signed-16-bit-pcm".to_owned()
}

fn default_stored_sample_format() -> String {
    "wav-pcm-s16le".to_owned()
}

fn default_timeline_unit() -> String {
    "nanosecond".to_owned()
}

fn default_normalization() -> String {
    "none".to_owned()
}

fn default_mix() -> String {
    "isolated-microphone".to_owned()
}

fn default_consent_record() -> String {
    "fresh-application-consent".to_owned()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ActiveMarker {
    schema_version: u32,
    session_id: SessionId,
    checkpointed_samples: u64,
}

/// A session writer. Finalization consumes it so a WAV header is finalized once.
pub struct SessionRecorder {
    session_dir: PathBuf,
    session_cap: Dir,
    manifest: SessionManifest,
    config: RecordingConfig,
    writer: Option<WavWriter<BufWriter<File>>>,
    segment_index: u32,
    segment_samples: u64,
    total_samples: u64,
    last_checkpoint_samples: u64,
    finalized: bool,
}

impl SessionRecorder {
    /// Creates an isolated session directory and publishes `active.json` before
    /// opening the first audio file.
    ///
    /// # Errors
    ///
    /// Fails for invalid configuration, filesystem errors, or WAV setup errors.
    pub fn start(config: RecordingConfig) -> Result<Self, RecordingError> {
        config.validate()?;
        create_private_directory(&config.data_root)?;
        let canonical_root = config.data_root.canonicalize()?;
        let root = Dir::open_ambient_dir(&canonical_root, ambient_authority())?;
        match root.create_dir("sessions") {
            Ok(()) => set_private_directory_permissions(&canonical_root.join("sessions"))?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {},
            Err(error) => return Err(error.into()),
        }
        let collection = root.open_dir("sessions")?;
        if !collection.dir_metadata()?.is_dir() {
            return Err(RecordingError::PathRejected);
        }

        let session_id = SessionId::new();
        let session_name = session_id.to_string();
        collection.create_dir(&session_name)?;
        let session_dir = canonical_root.join("sessions").join(&session_name);
        set_private_directory_permissions(&session_dir)?;
        let session_cap = collection.open_dir(&session_name)?;
        for child in ["audio", "transcript", "recovery"] {
            session_cap.create_dir(child)?;
            set_private_directory_permissions(&session_dir.join(child))?;
        }

        let manifest = SessionManifest {
            schema_version: MANIFEST_SCHEMA,
            session_id,
            state: SessionState::Recording,
            started_unix_ms: unix_millis()?,
            ended_unix_ms: None,
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            platform: std::env::consts::OS.to_owned(),
            backend: config.backend.clone(),
            source_device_id: config.source_device_id.clone(),
            permission_result: config.permission_result.clone(),
            sample_rate: config.sample_rate,
            channels: config.channels,
            native_sample_format: config.native_sample_format.clone(),
            stored_sample_format: "wav-pcm-s16le".to_owned(),
            timeline_unit: "nanosecond".to_owned(),
            normalization: "none".to_owned(),
            mix: "isolated-microphone".to_owned(),
            discontinuities: Vec::new(),
            consent_record: "fresh-application-consent".to_owned(),
            queue_capacity: config.queue_capacity,
            overflow_count: 0,
            network_policy: config.network_policy,
            audio_files: Vec::new(),
            failure_code: None,
        };

        write_json_atomic(
            &session_cap,
            Path::new("recovery/active.json"),
            &ActiveMarker {
                schema_version: ACTIVE_SCHEMA,
                session_id,
                checkpointed_samples: 0,
            },
        )?;
        write_json_atomic(&session_cap, Path::new("session.json"), &manifest)?;

        let writer = open_segment(&session_cap, 1, &config)?;
        Ok(Self {
            session_dir,
            session_cap,
            manifest,
            config,
            writer: Some(writer),
            segment_index: 1,
            segment_samples: 0,
            total_samples: 0,
            last_checkpoint_samples: 0,
            finalized: false,
        })
    }

    /// Current session identifier.
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.manifest.session_id
    }

    /// Session directory, intended for display only after caller redaction.
    #[must_use]
    pub fn session_directory(&self) -> &Path {
        &self.session_dir
    }

    /// Records interleaved signed 16-bit PCM and rotates only between samples.
    ///
    /// # Errors
    ///
    /// Returns a storage error if writing or segment finalization fails.
    pub fn write_samples(
        &mut self,
        samples: &[i16],
    ) -> Result<(), RecordingError> {
        if !samples
            .len()
            .is_multiple_of(usize::from(self.config.channels))
        {
            return Err(RecordingError::IncompleteSampleFrame);
        }
        for sample in samples {
            if self.segment_samples == self.config.samples_per_segment {
                self.rotate()?;
            }
            self.writer_mut()?.write_sample(*sample)?;
            self.segment_samples += 1;
            self.total_samples += 1;
        }
        let checkpoint_interval =
            u64::from(self.config.sample_rate) * u64::from(self.config.channels) * 5;
        if self
            .total_samples
            .saturating_sub(self.last_checkpoint_samples)
            >= checkpoint_interval
        {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Adds callback overflow observations to the durable per-source metric.
    pub const fn record_overflow(
        &mut self,
        count: u64,
    ) {
        self.manifest.overflow_count = self.manifest.overflow_count.saturating_add(count);
    }

    /// Records a capture-clock discontinuity on the session sample timeline.
    pub fn record_discontinuity(
        &mut self,
        timeline_timestamp_ns: u64,
    ) {
        self.manifest.discontinuities.push(timeline_timestamp_ns);
    }

    /// Flushes the WAV header and atomically checkpoints the manifest.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the checkpoint cannot be persisted.
    pub fn checkpoint(&mut self) -> Result<(), RecordingError> {
        self.writer_mut()?.flush()?;
        self.session_cap
            .open(segment_relative_path(self.segment_index))?
            .sync_all()?;
        sync_directory(&self.session_cap.open_dir("audio")?)?;
        write_json_atomic(
            &self.session_cap,
            Path::new("recovery/active.json"),
            &ActiveMarker {
                schema_version: ACTIVE_SCHEMA,
                session_id: self.manifest.session_id,
                checkpointed_samples: self.total_samples,
            },
        )?;
        write_json_atomic(&self.session_cap, Path::new("session.json"), &self.manifest)?;
        self.last_checkpoint_samples = self.total_samples;
        Ok(())
    }

    /// Finalizes all headers and publishes the terminal manifest exactly once.
    ///
    /// # Errors
    ///
    /// Returns a stable recording error if a header, digest, or manifest cannot
    /// be finalized.
    pub fn finalize(
        &mut self,
        cancelled: bool,
    ) -> Result<SessionManifest, RecordingError> {
        if self.finalized {
            return Ok(self.manifest.clone());
        }
        self.finish_current_segment()?;
        self.manifest.state = if cancelled {
            SessionState::Cancelled
        } else {
            SessionState::Completed
        };
        self.manifest.ended_unix_ms = Some(unix_millis()?);
        write_json_atomic(&self.session_cap, Path::new("session.json"), &self.manifest)?;
        let recovery = self.session_cap.open_dir("recovery")?;
        recovery.remove_file("active.json")?;
        sync_directory(&recovery)?;
        self.finalized = true;
        Ok(self.manifest.clone())
    }

    /// Best-effort durable failure state used when normal finalization fails.
    ///
    /// # Errors
    ///
    /// Returns the filesystem/serialization error if the failure snapshot
    /// itself cannot be published.
    pub fn mark_failed(&mut self) -> Result<SessionManifest, RecordingError> {
        self.manifest.state = SessionState::Failed;
        self.manifest.ended_unix_ms = Some(unix_millis()?);
        self.manifest.failure_code = Some("KOE-STORE-FINALIZE-FAILED".to_owned());
        write_json_atomic(&self.session_cap, Path::new("session.json"), &self.manifest)?;
        Ok(self.manifest.clone())
    }

    fn writer_mut(&mut self) -> Result<&mut WavWriter<BufWriter<File>>, RecordingError> {
        self.writer.as_mut().ok_or(RecordingError::AlreadyFinalized)
    }

    fn rotate(&mut self) -> Result<(), RecordingError> {
        self.finish_current_segment()?;
        self.segment_index = self
            .segment_index
            .checked_add(1)
            .ok_or(RecordingError::SegmentLimit)?;
        self.writer = Some(open_segment(
            &self.session_cap,
            self.segment_index,
            &self.config,
        )?);
        self.segment_samples = 0;
        Ok(())
    }

    fn finish_current_segment(&mut self) -> Result<(), RecordingError> {
        let Some(writer) = self.writer.take() else {
            return Ok(());
        };
        writer.finalize()?;
        let relative = segment_relative_path(self.segment_index);
        let mut file = self.session_cap.open(&relative)?.into_std();
        let metadata = file.metadata()?;
        file.sync_all()?;
        let timeline_start_sample = self.total_samples.saturating_sub(self.segment_samples);
        self.manifest.audio_files.push(AudioFile {
            path: relative,
            sample_count: self.segment_samples,
            timeline_start_sample,
            timeline_end_sample: self.total_samples,
            size: metadata.len(),
            sha256: sha256_reader(&mut file)?,
        });
        write_json_atomic(&self.session_cap, Path::new("session.json"), &self.manifest)?;
        Ok(())
    }
}

/// Finds abandoned sessions and marks readable WAV data as recovered partial.
///
/// # Errors
///
/// Returns an error when the root cannot be read or a discovered session cannot
/// be safely inspected.
pub fn recover_sessions(data_root: &Path) -> Result<Vec<SessionManifest>, RecordingError> {
    let root = match Dir::open_ambient_dir(data_root, ambient_authority()) {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let sessions = match root.open_dir("sessions") {
        Ok(sessions) => sessions,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut recovered = Vec::new();
    for entry in sessions.entries()? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if SessionId::parse(&name).is_err() {
            continue;
        }
        let session = sessions.open_dir(&name)?;
        let recovery = session.open_dir("recovery")?;
        let active_metadata = match recovery.symlink_metadata("active.json") {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !active_metadata.is_file() || active_metadata.file_type().is_symlink() {
            return Err(RecordingError::PathRejected);
        }
        let active_file = recovery.open("active.json")?.into_std();
        reject_hard_link(&active_file)?;
        let active: ActiveMarker = serde_json::from_reader(BufReader::new(active_file))?;
        let directory_id = SessionId::parse(&name).map_err(|_| RecordingError::PathRejected)?;
        if active.schema_version != ACTIVE_SCHEMA || active.session_id != directory_id {
            return Err(RecordingError::InvalidRecoveryMarker);
        }
        let manifest_metadata = session.symlink_metadata("session.json")?;
        if !manifest_metadata.is_file() || manifest_metadata.file_type().is_symlink() {
            return Err(RecordingError::PathRejected);
        }
        let manifest_file = session.open("session.json")?.into_std();
        reject_hard_link(&manifest_file)?;
        let mut manifest: SessionManifest = serde_json::from_reader(BufReader::new(manifest_file))?;
        if manifest.schema_version != MANIFEST_SCHEMA || manifest.session_id != directory_id {
            return Err(RecordingError::InvalidRecoveryMarker);
        }
        if matches!(
            manifest.state,
            SessionState::Completed
                | SessionState::Cancelled
                | SessionState::Failed
                | SessionState::RecoveredPartial
        ) {
            recovery.remove_file("active.json")?;
            sync_directory(&recovery)?;
            continue;
        }
        let audio_dir = session.open_dir("audio")?;
        manifest.audio_files = inspect_wav_segments(&audio_dir, &manifest)?;
        let recovered_samples = manifest
            .audio_files
            .iter()
            .map(|file| file.sample_count)
            .sum::<u64>();
        if active.checkpointed_samples != recovered_samples {
            return Err(RecordingError::InvalidRecoveryMarker);
        }
        manifest.state = SessionState::RecoveredPartial;
        manifest.ended_unix_ms = Some(unix_millis()?);
        manifest.failure_code = Some("KOE-STORE-RECOVERED-PARTIAL".to_owned());
        write_json_atomic(&session, Path::new("session.json"), &manifest)?;
        recovery.remove_file("active.json")?;
        sync_directory(&recovery)?;
        recovered.push(manifest);
    }
    Ok(recovered)
}

fn inspect_wav_segments(
    directory: &Dir,
    manifest: &SessionManifest,
) -> Result<Vec<AudioFile>, RecordingError> {
    let mut entries = Vec::new();
    for entry in directory.entries()? {
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or(RecordingError::PathRejected)?
            .to_owned();
        if !is_segment_filename(&name) {
            continue;
        }
        let file_type = entry.file_type()?;
        if !file_type.is_file() || file_type.is_symlink() {
            return Err(RecordingError::PathRejected);
        }
        let file = entry.open()?.into_std();
        reject_hard_link(&file)?;
        entries.push((name, file));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut files = Vec::with_capacity(entries.len());
    let mut timeline_start_sample = 0_u64;
    for (position, (name, mut file)) in entries.into_iter().enumerate() {
        let expected_name = format!("mic-{:06}.wav", position + 1);
        if name != expected_name {
            return Err(RecordingError::InvalidRecoveryMarker);
        }
        let reader = hound::WavReader::new(BufReader::new(file.try_clone()?))?;
        let spec = reader.spec();
        if spec.channels != manifest.channels
            || spec.sample_rate != manifest.sample_rate
            || spec.bits_per_sample != 16
            || spec.sample_format != SampleFormat::Int
        {
            return Err(RecordingError::InvalidRecoveryMarker);
        }
        let sample_count = u64::from(reader.duration()) * u64::from(spec.channels);
        if !sample_count.is_multiple_of(u64::from(spec.channels)) {
            return Err(RecordingError::InvalidRecoveryMarker);
        }
        let timeline_end_sample = timeline_start_sample
            .checked_add(sample_count)
            .ok_or(RecordingError::InvalidRecoveryMarker)?;
        files.push(AudioFile {
            path: format!("audio/{name}"),
            sample_count,
            timeline_start_sample,
            timeline_end_sample,
            size: file.metadata()?.len(),
            sha256: sha256_reader(&mut file)?,
        });
        timeline_start_sample = timeline_end_sample;
    }
    Ok(files)
}

#[cfg(unix)]
fn reject_hard_link(file: &File) -> Result<(), RecordingError> {
    use std::os::unix::fs::MetadataExt;

    if file.metadata()?.nlink() == 1 {
        Ok(())
    } else {
        Err(RecordingError::PathRejected)
    }
}

#[cfg(not(unix))]
fn reject_hard_link(_file: &File) -> Result<(), RecordingError> {
    Ok(())
}

fn is_segment_filename(name: &str) -> bool {
    let Some(index) = name
        .strip_prefix("mic-")
        .and_then(|value| value.strip_suffix(".wav"))
    else {
        return false;
    };
    index.len() == 6 && index.bytes().all(|byte| byte.is_ascii_digit())
}

fn open_segment(
    session_dir: &Dir,
    index: u32,
    config: &RecordingConfig,
) -> Result<WavWriter<BufWriter<File>>, RecordingError> {
    let path = segment_relative_path(index);
    let mut options = CapOpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = session_dir.open_with(path, &options)?.into_std();
    let writer = WavWriter::new(
        BufWriter::new(file),
        WavSpec {
            channels: config.channels,
            sample_rate: config.sample_rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        },
    )?;
    Ok(writer)
}

fn segment_relative_path(index: u32) -> String {
    format!("audio/mic-{index:06}.wav")
}

fn write_json_atomic<T: Serialize>(
    directory: &Dir,
    path: &Path,
    value: &T,
) -> Result<(), RecordingError> {
    let parent_path = path.parent().ok_or(RecordingError::PathRejected)?;
    let parent = if parent_path.as_os_str().is_empty() {
        directory.try_clone()?
    } else {
        directory.open_dir(parent_path)?
    };
    let target = path.file_name().ok_or(RecordingError::PathRejected)?;
    let temporary = format!(".{}.{}.tmp", target.to_string_lossy(), uuid::Uuid::new_v4());
    let mut options = CapOpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = parent.open_with(&temporary, &options)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    parent.rename(&temporary, &parent, target)?;
    sync_directory(&parent)?;
    Ok(())
}

fn sha256_reader(file: &mut File) -> Result<String, RecordingError> {
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let result = digest.finalize();
    Ok(format!("{result:x}"))
}

fn unix_millis() -> Result<u128, RecordingError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(|_| RecordingError::Clock)
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    set_private_directory_permissions(path)
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    let identity = std::process::Command::new("whoami").output()?;
    if !identity.status.success() {
        return Err(io::Error::other("failed to determine Windows identity"));
    }
    let user = String::from_utf8(identity.stdout)
        .map_err(|_| io::Error::other("Windows identity was not UTF-8"))?;
    let grant = format!("{}:(OI)(CI)F", user.trim());
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(grant)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("failed to apply owner-only Windows ACL"))
    }
}

fn sync_directory(directory: &Dir) -> io::Result<()> {
    directory.try_clone()?.into_std_file().sync_all()
}

/// Stable recording failures without sensitive path details.
#[derive(Debug, Error)]
pub enum RecordingError {
    #[error("invalid recording configuration")]
    InvalidConfiguration,
    #[error("session path was rejected")]
    PathRejected,
    #[error("recording writer was already finalized")]
    AlreadyFinalized,
    #[error("recording exceeded the segment limit")]
    SegmentLimit,
    #[error("audio payload ended inside a sample frame")]
    IncompleteSampleFrame,
    #[error("system clock is before the Unix epoch")]
    Clock,
    #[error("recording filesystem operation failed")]
    Io(#[from] io::Error),
    #[error("WAV operation failed")]
    Wav(#[from] hound::Error),
    #[error("manifest serialization failed")]
    Json(#[from] serde_json::Error),
    #[error("recovery marker is invalid")]
    InvalidRecoveryMarker,
}

impl RecordingError {
    /// Stable code for presentation layers.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::PathRejected => "KOE-STORE-PATH-REJECTED",
            Self::AlreadyFinalized
            | Self::Clock
            | Self::IncompleteSampleFrame
            | Self::InvalidConfiguration
            | Self::InvalidRecoveryMarker
            | Self::Io(_)
            | Self::Json(_)
            | Self::SegmentLimit
            | Self::Wav(_) => "KOE-STORE-FINALIZE-FAILED",
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::fs;

    use koe_core::{NetworkPolicy, SessionState};
    use tempfile::TempDir;

    use super::{RecordingConfig, SessionRecorder, recover_sessions};

    fn config(root: &TempDir) -> RecordingConfig {
        RecordingConfig {
            data_root: root.path().to_path_buf(),
            samples_per_segment: 4,
            sample_rate: 16_000,
            channels: 1,
            native_sample_format: "signed-16-bit-pcm".to_owned(),
            queue_capacity: 8,
            network_policy: NetworkPolicy::Denied,
            backend: "test".to_owned(),
            source_device_id: "fixture".to_owned(),
            permission_result: "granted".to_owned(),
        }
    }

    #[test]
    fn writes_rotated_wav_and_manifest() {
        let root = TempDir::new().expect("temporary directory");
        let mut recorder = SessionRecorder::start(config(&root)).expect("start");
        recorder.write_samples(&[1, 2, 3, 4, 5, 6]).expect("write");
        recorder.checkpoint().expect("checkpoint");
        let session_dir = recorder.session_directory().to_path_buf();
        let manifest = recorder.finalize(false).expect("finalize");

        assert_eq!(manifest.state, SessionState::Completed);
        assert_eq!(manifest.audio_files.len(), 2);
        assert_eq!(manifest.audio_files[0].sample_count, 4);
        assert_eq!(manifest.audio_files[1].sample_count, 2);
        assert!(!session_dir.join("recovery/active.json").exists());
        assert!(session_dir.join("session.json").is_file());
        assert_eq!(manifest.native_sample_format, "signed-16-bit-pcm");
        assert_eq!(manifest.stored_sample_format, "wav-pcm-s16le");
        assert_eq!(manifest.timeline_unit, "nanosecond");
        assert_eq!(manifest.normalization, "none");
        assert_eq!(manifest.mix, "isolated-microphone");
        assert_eq!(manifest.consent_record, "fresh-application-consent");
        assert_eq!(manifest.audio_files[0].timeline_start_sample, 0);
        assert_eq!(manifest.audio_files[1].timeline_end_sample, 6);
    }

    #[test]
    fn cancelled_recording_keeps_partial_artifacts() {
        let root = TempDir::new().expect("temporary directory");
        let mut recorder = SessionRecorder::start(config(&root)).expect("start");
        recorder.write_samples(&[1, 2]).expect("write");
        let manifest = recorder.finalize(true).expect("finalize");
        assert_eq!(manifest.state, SessionState::Cancelled);
        assert_eq!(manifest.audio_files[0].sample_count, 2);
    }

    #[test]
    fn recovers_abandoned_checkpoint() {
        let root = TempDir::new().expect("temporary directory");
        let session_dir = {
            let mut recorder = SessionRecorder::start(config(&root)).expect("start");
            recorder.write_samples(&[1, 2, 3]).expect("write");
            recorder.checkpoint().expect("checkpoint");
            recorder.session_directory().to_path_buf()
        };

        let recovered = recover_sessions(root.path()).expect("recover");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].state, SessionState::RecoveredPartial);
        assert!(!session_dir.join("recovery/active.json").exists());

        fs::write(
            session_dir.join("recovery/active.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": super::ACTIVE_SCHEMA,
                "session_id": recovered[0].session_id,
                "checkpointed_samples": 3
            }))
            .expect("marker JSON"),
        )
        .expect("restore marker");
        assert!(
            recover_sessions(root.path())
                .expect("idempotent recovery")
                .is_empty()
        );
        assert!(!session_dir.join("recovery/active.json").exists());
    }

    #[test]
    #[cfg(unix)]
    fn recovery_refuses_symlinked_manifest() {
        let root = TempDir::new().expect("temporary directory");
        let recorder = SessionRecorder::start(config(&root)).expect("start");
        let session_dir = recorder.session_directory().to_path_buf();
        drop(recorder);
        let manifest = session_dir.join("session.json");
        let outside = root.path().join("outside.json");
        fs::write(&outside, b"do not replace").expect("outside file");
        fs::remove_file(&manifest).expect("remove manifest");
        std::os::unix::fs::symlink(&outside, &manifest).expect("symlink");

        let result = recover_sessions(root.path());
        assert!(result.is_err());
        assert_eq!(fs::read(&outside).expect("outside file"), b"do not replace");
    }

    #[test]
    fn automatic_checkpoint_is_sample_driven() {
        let root = TempDir::new().expect("temporary directory");
        let mut recording_config = config(&root);
        recording_config.sample_rate = 1;
        let mut recorder = SessionRecorder::start(recording_config).expect("start");
        recorder
            .write_samples(&[1, 2, 3, 4, 5])
            .expect("write and checkpoint");
        let marker: serde_json::Value = serde_json::from_slice(
            &fs::read(recorder.session_directory().join("recovery/active.json"))
                .expect("active marker"),
        )
        .expect("valid marker");
        assert_eq!(marker["checkpointed_samples"], 5);
    }

    #[test]
    fn recovery_preserves_terminal_manifest_after_marker_removal_crash_window() {
        let root = TempDir::new().expect("temporary directory");
        let mut recorder = SessionRecorder::start(config(&root)).expect("start");
        let session_id = recorder.session_id();
        let session_dir = recorder.session_directory().to_path_buf();
        let completed = recorder.finalize(false).expect("finalize");
        fs::write(
            session_dir.join("recovery/active.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": super::ACTIVE_SCHEMA,
                "session_id": session_id,
                "checkpointed_samples": 0
            }))
            .expect("marker JSON"),
        )
        .expect("restore marker");

        assert!(recover_sessions(root.path()).expect("recover").is_empty());
        let persisted: super::SessionManifest =
            serde_json::from_slice(&fs::read(session_dir.join("session.json")).expect("manifest"))
                .expect("valid manifest");
        assert_eq!(persisted, completed);
    }

    #[test]
    #[cfg(unix)]
    fn recovery_refuses_symlinked_audio_directory() {
        let root = TempDir::new().expect("temporary directory");
        let recorder = SessionRecorder::start(config(&root)).expect("start");
        let session_dir = recorder.session_directory().to_path_buf();
        drop(recorder);
        fs::remove_dir_all(session_dir.join("audio")).expect("remove audio directory");
        std::os::unix::fs::symlink(root.path(), session_dir.join("audio")).expect("symlink");
        assert!(recover_sessions(root.path()).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn recovery_refuses_hard_linked_audio_file() {
        let root = TempDir::new().expect("temporary directory");
        let recorder = SessionRecorder::start(config(&root)).expect("start");
        let session_dir = recorder.session_directory().to_path_buf();
        drop(recorder);
        fs::hard_link(
            session_dir.join("audio/mic-000001.wav"),
            root.path().join("outside.wav"),
        )
        .expect("hard link");
        assert!(recover_sessions(root.path()).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn start_refuses_symlinked_sessions_collection() {
        let root = TempDir::new().expect("temporary directory");
        let outside = TempDir::new().expect("outside directory");
        std::os::unix::fs::symlink(outside.path(), root.path().join("sessions"))
            .expect("sessions symlink");

        assert!(SessionRecorder::start(config(&root)).is_err());
        assert!(
            fs::read_dir(outside.path())
                .expect("outside listing")
                .next()
                .is_none()
        );
    }

    #[test]
    fn recovery_refuses_non_contiguous_segments() {
        let root = TempDir::new().expect("temporary directory");
        let recorder = SessionRecorder::start(config(&root)).expect("start");
        let session_dir = recorder.session_directory().to_path_buf();
        drop(recorder);
        fs::rename(
            session_dir.join("audio/mic-000001.wav"),
            session_dir.join("audio/mic-000002.wav"),
        )
        .expect("rename segment");

        assert!(recover_sessions(root.path()).is_err());
    }

    #[test]
    fn recovery_refuses_wav_format_mismatch() {
        let root = TempDir::new().expect("temporary directory");
        let recorder = SessionRecorder::start(config(&root)).expect("start");
        let session_dir = recorder.session_directory().to_path_buf();
        drop(recorder);
        fs::remove_file(session_dir.join("audio/mic-000001.wav")).expect("remove segment");
        let writer = hound::WavWriter::create(
            session_dir.join("audio/mic-000001.wav"),
            hound::WavSpec {
                channels: 1,
                sample_rate: 8_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .expect("replacement WAV");
        writer.finalize().expect("finalize replacement");

        assert!(recover_sessions(root.path()).is_err());
    }

    #[test]
    fn recovery_refuses_data_beyond_checkpoint_boundary() {
        let root = TempDir::new().expect("temporary directory");
        let session_dir = {
            let mut recorder = SessionRecorder::start(config(&root)).expect("start");
            recorder.write_samples(&[1, 2, 3]).expect("write");
            recorder.checkpoint().expect("checkpoint");
            recorder.session_directory().to_path_buf()
        };
        let mut marker: serde_json::Value = serde_json::from_slice(
            &fs::read(session_dir.join("recovery/active.json")).expect("marker"),
        )
        .expect("marker JSON");
        marker["checkpointed_samples"] = serde_json::json!(2);
        fs::write(
            session_dir.join("recovery/active.json"),
            serde_json::to_vec_pretty(&marker).expect("marker JSON"),
        )
        .expect("write marker");

        assert!(recover_sessions(root.path()).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn atomic_json_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().expect("temporary directory");
        let recorder = SessionRecorder::start(config(&root)).expect("start");
        let session_dir = recorder.session_directory();
        for path in ["session.json", "recovery/active.json"] {
            let mode = fs::metadata(session_dir.join(path))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
