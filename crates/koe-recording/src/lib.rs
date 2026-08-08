//! Crash-aware segmented PCM WAV storage.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
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

const MANIFEST_SCHEMA: u32 = 2;
const ACTIVE_SCHEMA: u32 = 1;
const MAX_RECOVERY_SESSIONS: usize = 1_024;
const MAX_RECOVERY_SEGMENTS: usize = 10_000;
const MAX_RECOVERY_JSON_BYTES: u64 = 1024 * 1024;
const MAX_RECOVERY_WAV_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_RECOVERY_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_RECOVERY_WORK_ITEMS: usize = MAX_RECOVERY_SESSIONS + MAX_RECOVERY_SEGMENTS;

#[derive(Default)]
struct RecoveryBudget {
    work_items: usize,
    segments: usize,
    bytes: u64,
}

impl RecoveryBudget {
    fn charge_work(&mut self) -> Result<(), RecordingError> {
        self.work_items = self
            .work_items
            .checked_add(1)
            .ok_or(RecordingError::RecoveryLimitExceeded)?;
        if self.work_items > MAX_RECOVERY_WORK_ITEMS {
            return Err(RecordingError::RecoveryLimitExceeded);
        }
        Ok(())
    }

    fn charge_segment(
        &mut self,
        bytes: u64,
    ) -> Result<(), RecordingError> {
        self.segments = self
            .segments
            .checked_add(1)
            .ok_or(RecordingError::RecoveryLimitExceeded)?;
        if self.segments > MAX_RECOVERY_SEGMENTS {
            return Err(RecordingError::RecoveryLimitExceeded);
        }
        self.charge_bytes(bytes)
    }

    fn charge_bytes(
        &mut self,
        bytes: u64,
    ) -> Result<(), RecordingError> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(RecordingError::RecoveryLimitExceeded)?;
        if self.bytes > MAX_RECOVERY_TOTAL_BYTES {
            return Err(RecordingError::RecoveryLimitExceeded);
        }
        Ok(())
    }
}

/// Creates or tightens an application-owned directory to owner-only access.
///
/// This uses the same Unix mode and Windows protected-DACL implementation as
/// session storage. Symlinks and non-directories are rejected.
///
/// # Errors
///
/// Returns a stable path/storage error when the directory cannot be secured.
pub fn secure_app_directory(path: &Path) -> Result<(), RecordingError> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RecordingError::PathRejected);
    }
    let directory = Dir::open_ambient_dir(path, ambient_authority())?;
    set_private_directory_permissions(&directory)?;
    Ok(())
}

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
    /// Optional isolated system and canonical mixed tracks.
    pub additional_tracks: Vec<TrackConfig>,
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
            additional_tracks: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), RecordingError> {
        if self.samples_per_segment == 0
            || self.sample_rate == 0
            || self.channels == 0
            || self.queue_capacity == 0
            || !self
                .samples_per_segment
                .is_multiple_of(u64::from(self.channels))
        {
            return Err(RecordingError::InvalidConfiguration);
        }
        let mut kinds = BTreeSet::new();
        for track in &self.additional_tracks {
            track.validate()?;
            if !kinds.insert(track.kind.prefix()) {
                return Err(RecordingError::InvalidConfiguration);
            }
        }
        Ok(())
    }
}

/// Durable audio track category.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Microphone,
    System,
    Mix,
}

impl TrackKind {
    const fn prefix(self) -> &'static str {
        match self {
            Self::Microphone => "mic",
            Self::System => "system",
            Self::Mix => "mix",
        }
    }
}

/// Format and rotation policy for an additional session track.
#[derive(Clone, Debug)]
pub struct TrackConfig {
    pub kind: TrackKind,
    pub sample_rate: u32,
    pub channels: u16,
    pub samples_per_segment: u64,
    pub backend: String,
    pub source_device_id: String,
    pub permission_result: String,
    pub native_sample_format: String,
}

impl TrackConfig {
    fn validate(&self) -> Result<(), RecordingError> {
        if self.kind == TrackKind::Microphone
            || self.sample_rate == 0
            || self.channels == 0
            || self.samples_per_segment == 0
            || !self
                .samples_per_segment
                .is_multiple_of(u64::from(self.channels))
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

/// Timeline metadata persisted with one native PCM callback block.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TimelineBlock {
    /// Canonical session-monotonic position. The specification uses integer µs.
    pub session_start_us: u64,
    pub capture_epoch_id: u64,
    pub source_capture_start_ns: u64,
    pub callback_arrival_ns: u64,
    pub sequence: u64,
    pub frame_count: u64,
    pub discontinuity_before: bool,
}

/// A stored PCM range and its source-clock/session-clock mapping.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StoredTimelineBlock {
    pub source: TrackKind,
    pub pcm_start_frame: u64,
    pub pcm_frame_count: u64,
    pub session_start_us: u64,
    pub session_end_us: u64,
    pub capture_epoch_id: u64,
    pub source_capture_start_ns: u64,
    pub callback_arrival_ns: u64,
    pub sequence: u64,
    pub discontinuity_before: bool,
}

/// Versioned session manifest.
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
    #[serde(default)]
    pub gaps: Vec<AudioGap>,
    #[serde(default)]
    pub drift_corrections: Vec<DriftCorrection>,
    #[serde(default)]
    pub sources: Vec<AudioSourceManifest>,
    /// Exact block-to-session placement introduced in schema v2.
    #[serde(default)]
    pub timeline_blocks: Vec<StoredTimelineBlock>,
    /// v1 sessions have no trustworthy absolute source alignment.
    #[serde(default = "default_alignment_quality")]
    pub alignment_quality: String,
}

/// Source and format inventory frozen when the session starts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AudioSourceManifest {
    pub kind: TrackKind,
    pub backend: String,
    pub source_device_id: String,
    pub permission_result: String,
    pub native_sample_format: String,
    pub sample_rate: u32,
    pub channels: u16,
    #[serde(default)]
    pub overflow_count: u64,
}

/// Missing interval retained instead of joining audio across a discontinuity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AudioGap {
    pub source: TrackKind,
    pub start_us: u64,
    pub duration_us: u64,
    pub reason: String,
}

/// Measured source-clock correction applied by the synchronizer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DriftCorrection {
    pub source: TrackKind,
    pub timeline_us: u64,
    /// Signed parts-per-million, rounded after filtering.
    pub ppm: i32,
}

fn default_native_sample_format() -> String {
    "signed-16-bit-pcm".to_owned()
}

fn default_stored_sample_format() -> String {
    "wav-pcm-s16le".to_owned()
}

fn default_timeline_unit() -> String {
    "microsecond".to_owned()
}

fn default_alignment_quality() -> String {
    "legacy_unknown".to_owned()
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
    #[serde(default)]
    checkpointed_tracks: BTreeMap<String, u64>,
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
    last_checkpoint_tracks: BTreeMap<String, u64>,
    finalized: bool,
    additional_tracks: Vec<TrackWriter>,
}

struct TrackWriter {
    config: TrackConfig,
    writer: Option<WavWriter<BufWriter<File>>>,
    segment_index: u32,
    segment_samples: u64,
    total_samples: u64,
}

impl SessionRecorder {
    /// Creates an isolated session directory and publishes `active.json` before
    /// opening the first audio file.
    ///
    /// # Errors
    ///
    /// Fails for invalid configuration, filesystem errors, or WAV setup errors.
    #[allow(clippy::too_many_lines)]
    pub fn start(config: RecordingConfig) -> Result<Self, RecordingError> {
        config.validate()?;
        let root_created = create_private_directory(&config.data_root)?;
        let root_metadata = fs::symlink_metadata(&config.data_root)?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            return Err(RecordingError::PathRejected);
        }
        let root_identity = File::open(&config.data_root)?;
        let root = Dir::open_ambient_dir(&config.data_root, ambient_authority())?;
        verify_opened_identity(&root_identity, &root)?;
        if root_created {
            set_private_directory_permissions(&root)?;
        }
        let canonical_root = config.data_root.canonicalize()?;
        match root.create_dir("sessions") {
            Ok(()) => {},
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {},
            Err(error) => return Err(error.into()),
        }
        let collection = open_verified_dir(&root, Path::new("sessions"))?;
        set_private_directory_permissions(&collection)?;

        let session_id = SessionId::new();
        let session_name = session_id.to_string();
        collection.create_dir(&session_name)?;
        let session_dir = canonical_root.join("sessions").join(&session_name);
        let session_cap = open_verified_dir(&collection, Path::new(&session_name))?;
        set_private_directory_permissions(&session_cap)?;
        for child in ["audio", "transcript", "recovery"] {
            session_cap.create_dir(child)?;
            let child_cap = open_verified_dir(&session_cap, Path::new(child))?;
            set_private_directory_permissions(&child_cap)?;
        }

        let mut sources = vec![AudioSourceManifest {
            kind: TrackKind::Microphone,
            backend: config.backend.clone(),
            source_device_id: config.source_device_id.clone(),
            permission_result: config.permission_result.clone(),
            native_sample_format: config.native_sample_format.clone(),
            sample_rate: config.sample_rate,
            channels: config.channels,
            overflow_count: 0,
        }];
        sources.extend(
            config
                .additional_tracks
                .iter()
                .map(|track| AudioSourceManifest {
                    kind: track.kind,
                    backend: track.backend.clone(),
                    source_device_id: track.source_device_id.clone(),
                    permission_result: track.permission_result.clone(),
                    native_sample_format: track.native_sample_format.clone(),
                    sample_rate: track.sample_rate,
                    channels: track.channels,
                    overflow_count: 0,
                }),
        );
        let manifest = SessionManifest {
            schema_version: MANIFEST_SCHEMA,
            session_id,
            state: SessionState::Starting,
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
            timeline_unit: "microsecond".to_owned(),
            normalization: if config.additional_tracks.is_empty() {
                "none"
            } else {
                "downmix-linear-resample-16khz-drift-corrected"
            }
            .to_owned(),
            mix: if config.additional_tracks.is_empty() {
                "isolated-microphone"
            } else {
                "isolated-microphone+system+canonical-mix"
            }
            .to_owned(),
            discontinuities: Vec::new(),
            consent_record: "fresh-application-consent".to_owned(),
            queue_capacity: config.queue_capacity,
            overflow_count: 0,
            network_policy: config.network_policy,
            audio_files: Vec::new(),
            failure_code: None,
            gaps: Vec::new(),
            drift_corrections: Vec::new(),
            sources,
            timeline_blocks: Vec::new(),
            alignment_quality: "exact_block_timeline".to_owned(),
        };

        write_json_atomic(
            &session_cap,
            Path::new("recovery/active.json"),
            &ActiveMarker {
                schema_version: ACTIVE_SCHEMA,
                session_id,
                checkpointed_samples: 0,
                checkpointed_tracks: std::iter::once(("mic".to_owned(), 0))
                    .chain(
                        config
                            .additional_tracks
                            .iter()
                            .map(|track| (track.kind.prefix().to_owned(), 0)),
                    )
                    .collect(),
            },
        )?;
        write_json_atomic(&session_cap, Path::new("session.json"), &manifest)?;

        let writer = open_segment(&session_cap, 1, &config)?;
        let additional_tracks = config
            .additional_tracks
            .iter()
            .cloned()
            .map(|track| {
                Ok(TrackWriter {
                    writer: Some(open_track_segment(&session_cap, 1, &track)?),
                    config: track,
                    segment_index: 1,
                    segment_samples: 0,
                    total_samples: 0,
                })
            })
            .collect::<Result<Vec<_>, RecordingError>>()?;
        let last_checkpoint_tracks = std::iter::once(("mic".to_owned(), 0))
            .chain(
                config
                    .additional_tracks
                    .iter()
                    .map(|track| (track.kind.prefix().to_owned(), 0)),
            )
            .collect();
        Ok(Self {
            session_dir,
            session_cap,
            manifest,
            config,
            writer: Some(writer),
            segment_index: 1,
            segment_samples: 0,
            total_samples: 0,
            last_checkpoint_tracks,
            finalized: false,
            additional_tracks,
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
        self.write_samples_uncheckpointed(samples)?;
        self.checkpoint_if_due()
    }

    fn write_samples_uncheckpointed(
        &mut self,
        samples: &[i16],
    ) -> Result<(), RecordingError> {
        for sample in samples {
            if self.segment_samples == self.config.samples_per_segment {
                self.rotate()?;
            }
            self.writer_mut()?.write_sample(*sample)?;
            self.segment_samples += 1;
            self.total_samples += 1;
        }
        Ok(())
    }

    /// Writes microphone PCM together with its canonical session placement.
    ///
    /// # Errors
    ///
    /// Returns an error for inconsistent metadata or failed persistence.
    pub fn write_samples_block(
        &mut self,
        samples: &[i16],
        timeline: TimelineBlock,
    ) -> Result<(), RecordingError> {
        let channels = u64::from(self.config.channels);
        validate_timeline_block(samples, channels, &timeline)?;
        let pcm_start_frame = self.total_samples / channels;
        self.write_samples_uncheckpointed(samples)?;
        self.persist_timeline_block(
            TrackKind::Microphone,
            pcm_start_frame,
            self.config.sample_rate,
            timeline,
        )?;
        self.checkpoint_if_due()
    }

    /// Writes one optional isolated or mixed track configured at session start.
    ///
    /// # Errors
    ///
    /// Returns a configuration, frame, or storage error.
    pub fn write_track(
        &mut self,
        kind: TrackKind,
        samples: &[i16],
    ) -> Result<(), RecordingError> {
        self.write_track_uncheckpointed(kind, samples)?;
        self.checkpoint_if_due()
    }

    fn write_track_uncheckpointed(
        &mut self,
        kind: TrackKind,
        samples: &[i16],
    ) -> Result<(), RecordingError> {
        let track = self
            .additional_tracks
            .iter_mut()
            .find(|track| track.config.kind == kind)
            .ok_or(RecordingError::InvalidConfiguration)?;
        let channels = usize::from(track.config.channels);
        if !samples.len().is_multiple_of(channels) {
            return Err(RecordingError::IncompleteSampleFrame);
        }
        for sample in samples {
            if track.segment_samples == track.config.samples_per_segment {
                finish_track_segment(&self.session_cap, &mut self.manifest.audio_files, track)?;
                track.segment_index = track
                    .segment_index
                    .checked_add(1)
                    .ok_or(RecordingError::SegmentLimit)?;
                track.writer = Some(open_track_segment(
                    &self.session_cap,
                    track.segment_index,
                    &track.config,
                )?);
                track.segment_samples = 0;
            }
            track
                .writer
                .as_mut()
                .ok_or(RecordingError::AlreadyFinalized)?
                .write_sample(*sample)?;
            track.segment_samples += 1;
            track.total_samples += 1;
        }
        Ok(())
    }

    /// Writes an optional track together with its canonical session placement.
    ///
    /// # Errors
    ///
    /// Returns an error for inconsistent metadata or failed persistence.
    pub fn write_track_block(
        &mut self,
        kind: TrackKind,
        samples: &[i16],
        timeline: TimelineBlock,
    ) -> Result<(), RecordingError> {
        let (channels, sample_rate, pcm_start_frame) = self
            .additional_tracks
            .iter()
            .find(|track| track.config.kind == kind)
            .map(|track| {
                (
                    u64::from(track.config.channels),
                    track.config.sample_rate,
                    track.total_samples / u64::from(track.config.channels),
                )
            })
            .ok_or(RecordingError::InvalidConfiguration)?;
        validate_timeline_block(samples, channels, &timeline)?;
        self.write_track_uncheckpointed(kind, samples)?;
        self.persist_timeline_block(kind, pcm_start_frame, sample_rate, timeline)?;
        self.checkpoint_if_due()
    }

    fn persist_timeline_block(
        &mut self,
        source: TrackKind,
        pcm_start_frame: u64,
        sample_rate: u32,
        timeline: TimelineBlock,
    ) -> Result<(), RecordingError> {
        let duration_us = timeline
            .frame_count
            .saturating_mul(1_000_000)
            .checked_div(u64::from(sample_rate))
            .ok_or(RecordingError::InvalidConfiguration)?;
        self.manifest.timeline_blocks.push(StoredTimelineBlock {
            source,
            pcm_start_frame,
            pcm_frame_count: timeline.frame_count,
            session_start_us: timeline.session_start_us,
            session_end_us: timeline.session_start_us.saturating_add(duration_us),
            capture_epoch_id: timeline.capture_epoch_id,
            source_capture_start_ns: timeline.source_capture_start_ns,
            callback_arrival_ns: timeline.callback_arrival_ns,
            sequence: timeline.sequence,
            discontinuity_before: timeline.discontinuity_before,
        });
        // Do not rewrite and fsync the ever-growing manifest for every audio
        // callback. `write_samples`/`write_track` checkpoints it together with
        // the matching durable PCM lengths every five seconds, and finalization
        // publishes the complete timeline.
        Ok(())
    }

    /// Adds callback overflow observations to the durable per-source metric.
    pub fn record_overflow(
        &mut self,
        source: TrackKind,
        count: u64,
    ) {
        self.manifest.overflow_count = self.manifest.overflow_count.saturating_add(count);
        let mut index = 0;
        while index < self.manifest.sources.len() {
            if self.manifest.sources[index].kind == source {
                self.manifest.sources[index].overflow_count = self.manifest.sources[index]
                    .overflow_count
                    .saturating_add(count);
                break;
            }
            index += 1;
        }
    }

    /// Updates the durable result after an OS stream actually starts.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown track or failed manifest publication.
    pub fn record_permission_result(
        &mut self,
        source: TrackKind,
        result: &str,
    ) -> Result<(), RecordingError> {
        let source_manifest = self
            .manifest
            .sources
            .iter_mut()
            .find(|candidate| candidate.kind == source)
            .ok_or(RecordingError::InvalidConfiguration)?;
        result.clone_into(&mut source_manifest.permission_result);
        if source == TrackKind::Microphone {
            result.clone_into(&mut self.manifest.permission_result);
        }
        write_json_atomic(&self.session_cap, Path::new("session.json"), &self.manifest)
    }

    /// Records a capture-clock discontinuity on the session sample timeline.
    pub fn record_discontinuity(
        &mut self,
        timeline_timestamp_ns: u64,
    ) {
        self.manifest
            .discontinuities
            .push(timeline_timestamp_ns / 1_000);
    }

    /// Records a source-specific gap without joining audio across it.
    pub fn record_gap(
        &mut self,
        gap: AudioGap,
    ) {
        self.manifest.gaps.push(gap);
    }

    /// Publishes a durable degraded state while healthy sources keep recording.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest cannot be durably replaced.
    pub fn mark_degraded(&mut self) -> Result<(), RecordingError> {
        self.manifest.state = SessionState::Degraded;
        write_json_atomic(&self.session_cap, Path::new("session.json"), &self.manifest)
    }

    /// Publishes recovery after all configured capture sources are healthy.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest cannot be durably replaced.
    pub fn mark_recording(&mut self) -> Result<(), RecordingError> {
        self.manifest.state = SessionState::Recording;
        write_json_atomic(&self.session_cap, Path::new("session.json"), &self.manifest)
    }

    /// Records the filtered correction used for a source clock.
    pub fn record_drift_correction(
        &mut self,
        correction: DriftCorrection,
    ) {
        self.manifest.drift_corrections.push(correction);
    }

    /// Flushes the WAV header and atomically checkpoints the manifest.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the checkpoint cannot be persisted.
    pub fn checkpoint(&mut self) -> Result<(), RecordingError> {
        self.writer_mut()?.flush()?;
        for track in &mut self.additional_tracks {
            track
                .writer
                .as_mut()
                .ok_or(RecordingError::AlreadyFinalized)?
                .flush()?;
            self.session_cap
                .open(track_segment_relative_path(
                    track.config.kind,
                    track.segment_index,
                ))?
                .sync_all()?;
        }
        self.session_cap
            .open(segment_relative_path(self.segment_index))?
            .sync_all()?;
        sync_directory(&open_verified_dir(&self.session_cap, Path::new("audio"))?)?;
        write_json_atomic(
            &self.session_cap,
            Path::new("recovery/active.json"),
            &ActiveMarker {
                schema_version: ACTIVE_SCHEMA,
                session_id: self.manifest.session_id,
                checkpointed_samples: self.total_samples,
                checkpointed_tracks: self.checkpointed_track_lengths(),
            },
        )?;
        write_json_atomic(&self.session_cap, Path::new("session.json"), &self.manifest)?;
        self.last_checkpoint_tracks = self.checkpointed_track_lengths();
        Ok(())
    }

    fn checkpoint_if_due(&mut self) -> Result<(), RecordingError> {
        let mic_due = self
            .total_samples
            .saturating_sub(self.last_checkpoint_tracks.get("mic").copied().unwrap_or(0))
            >= samples_for_seconds(self.config.sample_rate, self.config.channels, 5);
        let additional_due = self.additional_tracks.iter().any(|track| {
            track.total_samples.saturating_sub(
                self.last_checkpoint_tracks
                    .get(track.config.kind.prefix())
                    .copied()
                    .unwrap_or(0),
            ) >= samples_for_seconds(track.config.sample_rate, track.config.channels, 5)
        });
        if mic_due || additional_due {
            self.checkpoint()?;
        }
        Ok(())
    }

    fn checkpointed_track_lengths(&self) -> BTreeMap<String, u64> {
        let mut lengths = BTreeMap::from([("mic".to_owned(), self.total_samples)]);
        lengths.extend(
            self.additional_tracks
                .iter()
                .map(|track| (track.config.kind.prefix().to_owned(), track.total_samples)),
        );
        lengths
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
        for track in &mut self.additional_tracks {
            finish_track_segment(&self.session_cap, &mut self.manifest.audio_files, track)?;
        }
        self.manifest.state = if cancelled {
            SessionState::Cancelled
        } else {
            SessionState::Completed
        };
        self.manifest.ended_unix_ms = Some(unix_millis()?);
        write_json_atomic(&self.session_cap, Path::new("session.json"), &self.manifest)?;
        let recovery = open_verified_dir(&self.session_cap, Path::new("recovery"))?;
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
        self.mark_failed_with_code("KOE-STORE-FINALIZE-FAILED")
    }

    /// Persists a fatal asynchronous capture/storage error with its stable code.
    ///
    /// # Errors
    ///
    /// Returns an error when the failure manifest cannot be durably published.
    pub fn mark_failed_with_code(
        &mut self,
        failure_code: &str,
    ) -> Result<SessionManifest, RecordingError> {
        let mut inventory_error = self.finish_current_segment().err();
        for track in &mut self.additional_tracks {
            if let Err(error) =
                finish_track_segment(&self.session_cap, &mut self.manifest.audio_files, track)
                && inventory_error.is_none()
            {
                inventory_error = Some(error);
            }
        }
        self.manifest.state = SessionState::Failed;
        self.manifest.ended_unix_ms = Some(unix_millis()?);
        self.manifest.failure_code = Some(failure_code.to_owned());
        write_json_atomic(&self.session_cap, Path::new("session.json"), &self.manifest)?;
        if let Some(error) = inventory_error {
            return Err(error);
        }
        let recovery = open_verified_dir(&self.session_cap, Path::new("recovery"))?;
        match recovery.remove_file("active.json") {
            Ok(()) => sync_directory(&recovery)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(error.into()),
        }
        self.finalized = true;
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
#[allow(clippy::too_many_lines)]
pub fn recover_sessions(data_root: &Path) -> Result<Vec<SessionManifest>, RecordingError> {
    let root_metadata = match fs::symlink_metadata(data_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(RecordingError::PathRejected);
    }
    let root_identity = File::open(data_root)?;
    let root = match Dir::open_ambient_dir(data_root, ambient_authority()) {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    verify_opened_identity(&root_identity, &root)?;
    let sessions = match open_verified_dir(&root, Path::new("sessions")) {
        Ok(sessions) => sessions,
        Err(RecordingError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        },
        Err(error) => return Err(error),
    };
    let mut recovered = Vec::new();
    let mut budget = RecoveryBudget::default();
    for (session_index, entry) in sessions.entries()?.enumerate() {
        if session_index >= MAX_RECOVERY_SESSIONS {
            return Err(RecordingError::RecoveryLimitExceeded);
        }
        budget.charge_work()?;
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
        let session = open_verified_dir(&sessions, Path::new(&name))?;
        let recovery = open_verified_dir(&session, Path::new("recovery"))?;
        let active_file = match open_regular_file(&recovery, Path::new("active.json")) {
            Ok(file) => file,
            Err(RecordingError::Io(error)) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let active: ActiveMarker = read_json_bounded(active_file, &mut budget)?;
        let directory_id = SessionId::parse(&name).map_err(|_| RecordingError::PathRejected)?;
        if active.schema_version != ACTIVE_SCHEMA || active.session_id != directory_id {
            return Err(RecordingError::InvalidRecoveryMarker);
        }
        let manifest_file = open_regular_file(&session, Path::new("session.json"))?;
        let mut manifest: SessionManifest = read_json_bounded(manifest_file, &mut budget)?;
        if !matches!(manifest.schema_version, 1 | MANIFEST_SCHEMA)
            || manifest.session_id != directory_id
        {
            return Err(RecordingError::InvalidRecoveryMarker);
        }
        if manifest.schema_version == 1 {
            "legacy_unknown".clone_into(&mut manifest.alignment_quality);
        }
        if manifest.state == SessionState::Failed {
            let audio_dir = open_verified_dir(&session, Path::new("audio"))?;
            manifest.audio_files =
                inspect_wav_segments(&audio_dir, &manifest, None, None, &mut budget)?;
            write_json_atomic(&session, Path::new("session.json"), &manifest)?;
            recovery.remove_file("active.json")?;
            sync_directory(&recovery)?;
            continue;
        }
        if matches!(
            manifest.state,
            SessionState::Completed | SessionState::Cancelled | SessionState::RecoveredPartial
        ) {
            recovery.remove_file("active.json")?;
            sync_directory(&recovery)?;
            continue;
        }
        let expected_tracks = if active.checkpointed_tracks.is_empty() {
            BTreeMap::from([("mic".to_owned(), active.checkpointed_samples)])
        } else {
            if active.checkpointed_tracks.get("mic").copied() != Some(active.checkpointed_samples) {
                return Err(RecordingError::InvalidRecoveryMarker);
            }
            active.checkpointed_tracks
        };
        let audio_dir = open_verified_dir(&session, Path::new("audio"))?;
        let recovery_artifact_name = format!("recovered-{}", uuid::Uuid::new_v4());
        recovery.create_dir(&recovery_artifact_name)?;
        let recovery_artifact = open_verified_dir(&recovery, Path::new(&recovery_artifact_name))?;
        set_private_directory_permissions(&recovery_artifact)?;
        manifest.audio_files = inspect_wav_segments(
            &audio_dir,
            &manifest,
            Some(&expected_tracks),
            Some((&recovery_artifact, &recovery_artifact_name)),
            &mut budget,
        )?;
        let recovered_tracks = recovered_track_lengths(&manifest.audio_files);
        if expected_tracks
            .iter()
            .any(|(track, expected)| recovered_tracks.get(track).copied().unwrap_or(0) != *expected)
            || recovered_tracks
                .keys()
                .any(|track| !expected_tracks.contains_key(track))
        {
            return Err(RecordingError::InvalidRecoveryMarker);
        }
        manifest.state = SessionState::RecoveredPartial;
        manifest.ended_unix_ms = Some(unix_millis()?);
        manifest.failure_code = Some("KOE-STORE-RECOVERED-PARTIAL".to_owned());
        write_json_atomic(&recovery_artifact, Path::new("session.json"), &manifest)?;
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
    recovery_limits: Option<&BTreeMap<String, u64>>,
    recovery_artifact: Option<(&Dir, &str)>,
    budget: &mut RecoveryBudget,
) -> Result<Vec<AudioFile>, RecordingError> {
    let mut entries = Vec::new();
    for entry in directory.entries()? {
        budget.charge_work()?;
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or(RecordingError::PathRejected)?
            .to_owned();
        if parse_segment_filename(&name).is_none() {
            continue;
        }
        let file = open_regular_file(directory, Path::new(&name))?;
        let file_len = file.metadata()?.len();
        if file_len > MAX_RECOVERY_WAV_BYTES {
            return Err(RecordingError::RecoveryLimitExceeded);
        }
        budget.charge_segment(file_len)?;
        entries.push((name, file));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut files = Vec::with_capacity(entries.len());
    let mut track_positions = BTreeMap::<String, (u32, u64)>::new();
    for (name, mut file) in entries {
        let (prefix, index) =
            parse_segment_filename(&name).ok_or(RecordingError::InvalidRecoveryMarker)?;
        let position = track_positions.entry(prefix.to_owned()).or_insert((0, 0));
        if index != position.0.saturating_add(1) {
            return Err(RecordingError::InvalidRecoveryMarker);
        }
        position.0 = index;
        if recovery_limits
            .and_then(|limits| limits.get(prefix))
            .is_some_and(|limit| position.1 >= *limit && index > 1)
        {
            continue;
        }
        let reader = hound::WavReader::new(BufReader::new(file.try_clone()?))?;
        let spec = reader.spec();
        let (expected_channels, expected_rate) =
            expected_track_format(manifest, prefix).ok_or(RecordingError::InvalidRecoveryMarker)?;
        if spec.channels != expected_channels
            || spec.sample_rate != expected_rate
            || spec.bits_per_sample != 16
            || spec.sample_format != SampleFormat::Int
        {
            return Err(RecordingError::InvalidRecoveryMarker);
        }
        let available_samples = u64::from(reader.duration()) * u64::from(spec.channels);
        let sample_count = if let Some(limits) = recovery_limits {
            let remaining = limits
                .get(prefix)
                .copied()
                .ok_or(RecordingError::InvalidRecoveryMarker)?
                .saturating_sub(position.1);
            available_samples.min(remaining)
        } else {
            available_samples
        };
        drop(reader);
        let (mut artifact, path) =
            if let Some((artifact_directory, artifact_name)) = recovery_artifact {
                let copied = copy_recovery_wav(&mut file, artifact_directory, &name)?;
                (copied, format!("recovery/{artifact_name}/{name}"))
            } else {
                (file, format!("audio/{name}"))
            };
        if recovery_limits.is_some() {
            truncate_wav_to_samples(&mut artifact, sample_count)?;
        }
        if !sample_count.is_multiple_of(u64::from(spec.channels)) {
            return Err(RecordingError::InvalidRecoveryMarker);
        }
        let timeline_start_sample = position.1;
        let timeline_end_sample = timeline_start_sample
            .checked_add(sample_count)
            .ok_or(RecordingError::InvalidRecoveryMarker)?;
        files.push(AudioFile {
            path,
            sample_count,
            timeline_start_sample,
            timeline_end_sample,
            size: artifact.metadata()?.len(),
            sha256: sha256_reader(&mut artifact)?,
        });
        position.1 = timeline_end_sample;
    }
    Ok(files)
}

fn copy_recovery_wav(
    source: &mut File,
    destination: &Dir,
    name: &str,
) -> Result<File, RecordingError> {
    source.rewind()?;
    let mut options = CapOpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut copy = destination.open_with(name, &options)?.into_std();
    io::copy(source, &mut copy)?;
    copy.sync_all()?;
    copy.rewind()?;
    Ok(copy)
}

fn samples_for_seconds(
    sample_rate: u32,
    channels: u16,
    seconds: u64,
) -> u64 {
    u64::from(sample_rate)
        .saturating_mul(u64::from(channels))
        .saturating_mul(seconds)
}

fn validate_timeline_block(
    samples: &[i16],
    channels: u64,
    timeline: &TimelineBlock,
) -> Result<(), RecordingError> {
    if channels == 0 {
        return Err(RecordingError::InvalidConfiguration);
    }
    let sample_count =
        u64::try_from(samples.len()).map_err(|_| RecordingError::InvalidConfiguration)?;
    if sample_count % channels != 0 || sample_count / channels != timeline.frame_count {
        return Err(RecordingError::IncompleteSampleFrame);
    }
    Ok(())
}

fn expected_track_format(
    manifest: &SessionManifest,
    prefix: &str,
) -> Option<(u16, u32)> {
    manifest
        .sources
        .iter()
        .find(|source| source.kind.prefix() == prefix)
        .map(|source| (source.channels, source.sample_rate))
        .or_else(|| (prefix == "mic").then_some((manifest.channels, manifest.sample_rate)))
}

fn read_json_bounded<T: for<'de> Deserialize<'de>>(
    file: File,
    budget: &mut RecoveryBudget,
) -> Result<T, RecordingError> {
    let len = file.metadata()?.len();
    if len > MAX_RECOVERY_JSON_BYTES {
        return Err(RecordingError::RecoveryLimitExceeded);
    }
    budget.charge_bytes(len)?;
    Ok(serde_json::from_reader(BufReader::new(
        file.take(MAX_RECOVERY_JSON_BYTES + 1),
    ))?)
}

fn truncate_wav_to_samples(
    file: &mut File,
    sample_count: u64,
) -> Result<(), RecordingError> {
    const MAX_WAV_HEADER: u64 = 1024 * 1024;
    file.rewind()?;
    let mut riff = [0_u8; 12];
    file.read_exact(&mut riff)?;
    if &riff[..4] != b"RIFF" || &riff[8..] != b"WAVE" {
        return Err(RecordingError::InvalidRecoveryMarker);
    }
    let physical_len = file.metadata()?.len();
    let declared_riff_end = 8_u64
        .checked_add(u64::from(u32::from_le_bytes(
            riff[4..8]
                .try_into()
                .map_err(|_| RecordingError::InvalidRecoveryMarker)?,
        )))
        .ok_or(RecordingError::InvalidRecoveryMarker)?;
    if declared_riff_end > physical_len {
        return Err(RecordingError::InvalidRecoveryMarker);
    }
    let mut cursor = 12_u64;
    let (data_size_offset, data_offset) = loop {
        if cursor > MAX_WAV_HEADER || cursor.saturating_add(8) > declared_riff_end {
            return Err(RecordingError::InvalidRecoveryMarker);
        }
        file.seek(SeekFrom::Start(cursor))?;
        let mut chunk = [0_u8; 8];
        file.read_exact(&mut chunk)?;
        let size = u64::from(u32::from_le_bytes(
            chunk[4..8]
                .try_into()
                .map_err(|_| RecordingError::InvalidRecoveryMarker)?,
        ));
        let chunk_end = cursor
            .checked_add(8)
            .and_then(|position| position.checked_add(size))
            .ok_or(RecordingError::InvalidRecoveryMarker)?;
        if chunk_end > declared_riff_end || chunk_end > physical_len {
            return Err(RecordingError::InvalidRecoveryMarker);
        }
        if &chunk[..4] == b"data" {
            break (cursor + 4, cursor + 8);
        }
        cursor = cursor
            .checked_add(8)
            .and_then(|position| position.checked_add(size + (size & 1)))
            .ok_or(RecordingError::InvalidRecoveryMarker)?;
    };
    let data_bytes = sample_count
        .checked_mul(2)
        .ok_or(RecordingError::RecoveryLimitExceeded)?;
    let data_size = u32::try_from(data_bytes).map_err(|_| RecordingError::RecoveryLimitExceeded)?;
    let final_len = data_offset
        .checked_add(data_bytes)
        .ok_or(RecordingError::RecoveryLimitExceeded)?;
    if final_len > physical_len {
        return Err(RecordingError::InvalidRecoveryMarker);
    }
    let riff_size = u32::try_from(final_len.saturating_sub(8))
        .map_err(|_| RecordingError::RecoveryLimitExceeded)?;
    file.set_len(final_len)?;
    file.seek(SeekFrom::Start(4))?;
    file.write_all(&riff_size.to_le_bytes())?;
    file.seek(SeekFrom::Start(data_size_offset))?;
    file.write_all(&data_size.to_le_bytes())?;
    file.sync_all()?;
    file.rewind()?;
    Ok(())
}

fn open_regular_file(
    directory: &Dir,
    path: &Path,
) -> Result<File, RecordingError> {
    let before = directory.symlink_metadata(path)?;
    if !before.is_file() || before.file_type().is_symlink() {
        return Err(RecordingError::PathRejected);
    }
    #[cfg(windows)]
    let identity = directory.open(path)?.into_std();
    let file = directory.open(path)?.into_std();
    reject_hard_link(&file)?;
    #[cfg(windows)]
    verify_windows_file_identity(&identity, &file)?;
    #[cfg(not(windows))]
    verify_file_identity(&before, &file.metadata()?)?;
    Ok(file)
}

#[cfg(unix)]
fn verify_opened_identity(
    before: &File,
    opened: &Dir,
) -> Result<(), RecordingError> {
    use std::os::unix::fs::MetadataExt;

    let before = before.metadata()?;
    let after = opened.try_clone()?.into_std_file().metadata()?;
    if before.dev() == after.dev() && before.ino() == after.ino() {
        Ok(())
    } else {
        Err(RecordingError::PathRejected)
    }
}

#[cfg(windows)]
fn verify_opened_identity(
    before: &File,
    opened: &Dir,
) -> Result<(), RecordingError> {
    let after = opened.try_clone()?.into_std_file();
    if same_file::Handle::from_file(before.try_clone()?)? == same_file::Handle::from_file(after)? {
        Ok(())
    } else {
        Err(RecordingError::PathRejected)
    }
}

#[cfg(not(any(unix, windows)))]
fn verify_opened_identity(
    _before: &File,
    opened: &Dir,
) -> Result<(), RecordingError> {
    opened
        .dir_metadata()?
        .is_dir()
        .then_some(())
        .ok_or(RecordingError::PathRejected)
}

#[cfg(unix)]
fn verify_file_identity(
    before: &cap_std::fs::Metadata,
    after: &fs::Metadata,
) -> Result<(), RecordingError> {
    use cap_std::fs::MetadataExt as CapMetadataExt;
    use std::os::unix::fs::MetadataExt as StdMetadataExt;

    if CapMetadataExt::dev(before) == StdMetadataExt::dev(after)
        && CapMetadataExt::ino(before) == StdMetadataExt::ino(after)
    {
        Ok(())
    } else {
        Err(RecordingError::PathRejected)
    }
}

#[cfg(windows)]
fn verify_windows_file_identity(
    before: &File,
    after: &File,
) -> Result<(), RecordingError> {
    if same_file::Handle::from_file(before.try_clone()?)?
        == same_file::Handle::from_file(after.try_clone()?)?
    {
        Ok(())
    } else {
        Err(RecordingError::PathRejected)
    }
}

#[cfg(not(any(unix, windows)))]
fn verify_file_identity(
    before: &cap_std::fs::Metadata,
    after: &fs::Metadata,
) -> Result<(), RecordingError> {
    (before.len() == after.len() && before.is_file() && after.is_file())
        .then_some(())
        .ok_or(RecordingError::PathRejected)
}

fn recovered_track_lengths(files: &[AudioFile]) -> BTreeMap<String, u64> {
    let mut lengths = BTreeMap::new();
    for file in files {
        if let Some(name) = Path::new(&file.path)
            .file_name()
            .and_then(|name| name.to_str())
            && let Some((prefix, _)) = parse_segment_filename(name)
        {
            let total = lengths.entry(prefix.to_owned()).or_insert(0_u64);
            *total = total.saturating_add(file.sample_count);
        }
    }
    lengths
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

#[cfg(windows)]
#[allow(unsafe_code)]
fn reject_hard_link(file: &File) -> Result<(), RecordingError> {
    use std::{
        mem::{size_of, zeroed},
        os::windows::io::AsRawHandle,
    };
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{FILE_STANDARD_INFO, FileStandardInfo, GetFileInformationByHandleEx},
    };

    // SAFETY: `information` is correctly sized for FileStandardInfo and the
    // raw handle remains owned by `file` for the duration of this call.
    let mut information: FILE_STANDARD_INFO = unsafe { zeroed() };
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileStandardInfo,
            (&raw mut information).cast(),
            u32::try_from(size_of::<FILE_STANDARD_INFO>())
                .map_err(|_| RecordingError::PathRejected)?,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error().into());
    }
    if information.NumberOfLinks == 1 {
        Ok(())
    } else {
        Err(RecordingError::PathRejected)
    }
}

#[cfg(not(any(unix, windows)))]
fn reject_hard_link(_file: &File) -> Result<(), RecordingError> {
    Ok(())
}

fn parse_segment_filename(name: &str) -> Option<(&str, u32)> {
    let (prefix, suffix) = name.split_once('-')?;
    if !matches!(prefix, "mic" | "system" | "mix") {
        return None;
    }
    let index = suffix.strip_suffix(".wav")?;
    if index.len() != 6 || !index.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    index.parse().ok().map(|index| (prefix, index))
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

fn open_track_segment(
    session_dir: &Dir,
    index: u32,
    config: &TrackConfig,
) -> Result<WavWriter<BufWriter<File>>, RecordingError> {
    let path = track_segment_relative_path(config.kind, index);
    let mut options = CapOpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = session_dir.open_with(path, &options)?.into_std();
    Ok(WavWriter::new(
        BufWriter::new(file),
        WavSpec {
            channels: config.channels,
            sample_rate: config.sample_rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        },
    )?)
}

fn finish_track_segment(
    session_dir: &Dir,
    audio_files: &mut Vec<AudioFile>,
    track: &mut TrackWriter,
) -> Result<(), RecordingError> {
    let Some(writer) = track.writer.take() else {
        return Ok(());
    };
    writer.finalize()?;
    let relative = track_segment_relative_path(track.config.kind, track.segment_index);
    let mut file = session_dir.open(&relative)?.into_std();
    let metadata = file.metadata()?;
    file.sync_all()?;
    audio_files.push(AudioFile {
        path: relative,
        sample_count: track.segment_samples,
        timeline_start_sample: track.total_samples.saturating_sub(track.segment_samples),
        timeline_end_sample: track.total_samples,
        size: metadata.len(),
        sha256: sha256_reader(&mut file)?,
    });
    Ok(())
}

fn segment_relative_path(index: u32) -> String {
    format!("audio/mic-{index:06}.wav")
}

fn track_segment_relative_path(
    kind: TrackKind,
    index: u32,
) -> String {
    format!("audio/{}-{index:06}.wav", kind.prefix())
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
        open_verified_dir(directory, parent_path)?
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
    replace_json_file(&parent, Path::new(&temporary), target)?;
    sync_directory(&parent)?;
    Ok(())
}

#[cfg(not(windows))]
fn replace_json_file(
    parent: &Dir,
    temporary: &Path,
    target: &std::ffi::OsStr,
) -> io::Result<()> {
    parent.rename(temporary, parent, target)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn replace_json_file(
    parent: &Dir,
    temporary: &Path,
    target: &std::ffi::OsStr,
) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    match parent.symlink_metadata(target) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return parent.rename(temporary, parent, target);
        },
        Err(error) => return Err(error),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(io::Error::other("JSON target is not a regular file"));
        },
        Ok(_) => {},
    }
    let directory = directory_path_from_handle(parent)?;
    let replaced = directory.join(target);
    let replacement = directory.join(temporary);
    let replaced_wide = replaced
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let replacement_wide = replacement
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both paths are NUL-terminated, derived from the already-opened
    // capability directory, and remain alive for the duration of the call.
    let result = unsafe {
        ReplaceFileW(
            replaced_wide.as_ptr(),
            replacement_wide.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn sha256_reader(file: &mut File) -> Result<String, RecordingError> {
    file.rewind()?;
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

fn create_private_directory(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(io::Error::other("data root must be a real directory"))
        },
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
            Ok(true)
        },
        Err(error) => Err(error),
    }
}

fn open_verified_dir(
    parent: &Dir,
    path: &Path,
) -> Result<Dir, RecordingError> {
    let metadata = parent.symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RecordingError::PathRejected);
    }
    #[cfg(windows)]
    let identity = parent.open_dir(path)?;
    let directory = parent.open_dir(path)?;
    let opened_metadata = directory.dir_metadata()?;
    if !opened_metadata.is_dir() {
        return Err(RecordingError::PathRejected);
    }
    #[cfg(windows)]
    {
        let identity = identity.into_std_file();
        let opened = directory.try_clone()?.into_std_file();
        if same_file::Handle::from_file(identity)? != same_file::Handle::from_file(opened)? {
            return Err(RecordingError::PathRejected);
        }
    }
    #[cfg(not(windows))]
    verify_cap_metadata_identity(&metadata, &opened_metadata)?;
    Ok(directory)
}

#[cfg(unix)]
fn verify_cap_metadata_identity(
    before: &cap_std::fs::Metadata,
    after: &cap_std::fs::Metadata,
) -> Result<(), RecordingError> {
    use cap_std::fs::MetadataExt;

    if before.dev() == after.dev() && before.ino() == after.ino() {
        Ok(())
    } else {
        Err(RecordingError::PathRejected)
    }
}

#[cfg(not(any(unix, windows)))]
fn verify_cap_metadata_identity(
    before: &cap_std::fs::Metadata,
    after: &cap_std::fs::Metadata,
) -> Result<(), RecordingError> {
    if before.is_dir() && after.is_dir() {
        Ok(())
    } else {
        Err(RecordingError::PathRejected)
    }
}

#[cfg(unix)]
fn set_private_directory_permissions(directory: &Dir) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    directory.set_permissions(
        Path::new("."),
        cap_std::fs::Permissions::from_std(fs::Permissions::from_mode(0o700)),
    )
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn set_private_directory_permissions(directory: &Dir) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::{ERROR_SUCCESS, HANDLE, LocalFree},
        Security::{
            ACL,
            Authorization::{
                EXPLICIT_ACCESS_W, GetSecurityInfo, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW,
                SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
            },
            DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
            PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
            SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        },
        Storage::FileSystem::FILE_ALL_ACCESS,
    };

    let handle = directory.try_clone()?.into_std_file();
    let raw = handle.as_raw_handle() as HANDLE;
    let mut owner = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: all output pointers refer to live local variables and `raw`
    // remains owned by `handle` throughout the security descriptor operation.
    let status = unsafe {
        GetSecurityInfo(
            raw,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &raw mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status.cast_signed()));
    }

    let access = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: owner.cast(),
        },
    };
    let mut acl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `access` contains the owner SID held alive by `descriptor`, and
    // `acl` is an out-pointer released with LocalFree below.
    let acl_status =
        unsafe { SetEntriesInAclW(1, &raw const access, std::ptr::null(), &raw mut acl) };
    if acl_status != ERROR_SUCCESS {
        // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
        unsafe {
            LocalFree(descriptor.cast());
        }
        return Err(io::Error::from_raw_os_error(acl_status.cast_signed()));
    }

    // SAFETY: `raw` is a live directory handle and `acl` is a valid ACL
    // produced by SetEntriesInAclW. Null SID/SACL pointers leave them unchanged.
    let set_status = unsafe {
        SetSecurityInfo(
            raw,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        )
    };
    // SAFETY: both buffers were allocated by Win32 APIs documented for LocalFree.
    unsafe {
        LocalFree(acl.cast());
        LocalFree(descriptor.cast());
    }
    if set_status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(set_status.cast_signed()))
    }
}

#[cfg(not(any(unix, windows)))]
fn set_private_directory_permissions(_directory: &Dir) -> io::Result<()> {
    Err(io::Error::other(
        "owner-only directory permissions are unsupported on this platform",
    ))
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn directory_path_from_handle(directory: &Dir) -> io::Result<PathBuf> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

    let handle = directory.try_clone()?.into_std_file();
    let raw = handle.as_raw_handle() as HANDLE;
    let mut buffer = vec![0_u16; 512];
    loop {
        // SAFETY: `raw` belongs to the live cloned directory handle and
        // `buffer` exposes its initialized writable allocation.
        let count = unsafe {
            GetFinalPathNameByHandleW(
                raw,
                buffer.as_mut_ptr(),
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                0,
            )
        };
        if count == 0 {
            return Err(io::Error::last_os_error());
        }
        let count = usize::try_from(count).map_err(|_| io::Error::other("path too long"))?;
        if count < buffer.len() {
            buffer.truncate(count);
            return Ok(PathBuf::from(String::from_utf16_lossy(&buffer)));
        }
        buffer.resize(count.saturating_add(1), 0);
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
    #[error("recovery resource limit exceeded")]
    RecoveryLimitExceeded,
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
            | Self::RecoveryLimitExceeded
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
    use std::{
        fs::{self, OpenOptions},
        path::Path,
    };

    use koe_core::{NetworkPolicy, SessionState};
    use tempfile::TempDir;

    use super::{
        AudioGap, DriftCorrection, RecordingConfig, SessionRecorder, TimelineBlock, TrackConfig,
        TrackKind, recover_sessions,
    };

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
            additional_tracks: Vec::new(),
        }
    }

    fn system_config(root: &TempDir) -> RecordingConfig {
        let mut value = config(root);
        value.sample_rate = 1;
        value.samples_per_segment = 20;
        value.additional_tracks.push(TrackConfig {
            kind: TrackKind::System,
            sample_rate: 1,
            channels: 1,
            samples_per_segment: 20,
            backend: "fixture".to_owned(),
            source_device_id: "system-fixture".to_owned(),
            permission_result: "granted".to_owned(),
            native_sample_format: "signed-16-bit-pcm".to_owned(),
        });
        value
    }

    #[test]
    fn duplicate_additional_track_kinds_are_rejected() {
        let root = TempDir::new().expect("temporary directory");
        let mut value = system_config(&root);
        value
            .additional_tracks
            .push(value.additional_tracks[0].clone());
        assert!(SessionRecorder::start(value).is_err());
    }

    #[test]
    fn system_progress_triggers_checkpoint_and_recovers() {
        let root = TempDir::new().expect("temporary directory");
        let session_dir = {
            let mut recorder = SessionRecorder::start(system_config(&root)).expect("start");
            recorder
                .write_track(TrackKind::System, &[1, 2, 3, 4, 5])
                .expect("system write");
            recorder.session_directory().to_path_buf()
        };
        let marker: serde_json::Value = serde_json::from_slice(
            &fs::read(session_dir.join("recovery/active.json")).expect("marker"),
        )
        .expect("marker JSON");
        assert_eq!(marker["checkpointed_tracks"]["system"], 5);
        let recovered = recover_sessions(root.path()).expect("recover");
        assert_eq!(recovered.len(), 1);
    }

    #[test]
    fn recovery_allows_a_missing_track_with_zero_checkpointed_samples() {
        let root = TempDir::new().expect("temporary directory");
        let session_dir = {
            let recorder = SessionRecorder::start(system_config(&root)).expect("start");
            recorder.session_directory().to_path_buf()
        };
        fs::remove_file(session_dir.join("audio/system-000001.wav"))
            .expect("remove empty system track");
        let recovered = recover_sessions(root.path()).expect("recover");
        assert_eq!(recovered.len(), 1);
        assert!(
            recovered[0]
                .audio_files
                .iter()
                .all(|file| !file.path.starts_with("audio/system-"))
        );
    }

    #[test]
    fn failed_session_finalizes_and_inventories_partial_wav_files() {
        let root = TempDir::new().expect("temporary directory");
        let mut recorder = SessionRecorder::start(config(&root)).expect("start");
        recorder.write_samples(&[1, 2, 3]).expect("write");
        let session_dir = recorder.session_directory().to_path_buf();
        let manifest = recorder
            .mark_failed_with_code("KOE-AUDIO-STREAM-RUNTIME-FAILED")
            .expect("mark failed");
        assert_eq!(manifest.state, SessionState::Failed);
        assert_eq!(manifest.audio_files[0].sample_count, 3);
        assert!(!session_dir.join("recovery/active.json").exists());
        let reader = hound::WavReader::open(session_dir.join("audio/mic-000001.wav")).expect("WAV");
        assert_eq!(reader.duration(), 3);
    }

    #[test]
    fn recovery_reconciles_failed_manifest_with_a_leftover_active_marker() {
        let root = TempDir::new().expect("temporary directory");
        let session_dir = {
            let mut recorder = SessionRecorder::start(config(&root)).expect("start");
            recorder.write_samples(&[1, 2, 3]).expect("write");
            recorder.checkpoint().expect("checkpoint");
            recorder.session_directory().to_path_buf()
        };
        let manifest_path = session_dir.join("session.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("manifest"))
                .expect("manifest JSON");
        manifest["state"] = serde_json::json!("failed");
        manifest["failure_code"] = serde_json::json!("KOE-AUDIO-STREAM-RUNTIME-FAILED");
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("manifest JSON"),
        )
        .expect("failed manifest");

        assert!(recover_sessions(root.path()).expect("reconcile").is_empty());
        assert!(!session_dir.join("recovery/active.json").exists());
        let reconciled: serde_json::Value =
            serde_json::from_slice(&fs::read(manifest_path).expect("reconciled manifest"))
                .expect("manifest JSON");
        assert_eq!(reconciled["audio_files"][0]["sample_count"], 3);
    }

    #[test]
    fn recovery_budget_is_aggregate() {
        let mut budget = super::RecoveryBudget {
            bytes: super::MAX_RECOVERY_TOTAL_BYTES - 1,
            ..super::RecoveryBudget::default()
        };
        budget.charge_bytes(1).expect("last byte");
        assert!(matches!(
            budget.charge_bytes(1),
            Err(super::RecordingError::RecoveryLimitExceeded)
        ));
        let mut work_budget = super::RecoveryBudget {
            work_items: super::MAX_RECOVERY_WORK_ITEMS,
            ..super::RecoveryBudget::default()
        };
        assert!(matches!(
            work_budget.charge_work(),
            Err(super::RecordingError::RecoveryLimitExceeded)
        ));
    }

    #[test]
    fn overflow_metrics_are_source_specific() {
        let root = TempDir::new().expect("temporary directory");
        let mut recorder = SessionRecorder::start(system_config(&root)).expect("start");
        recorder.record_overflow(TrackKind::Microphone, 2);
        recorder.record_overflow(TrackKind::System, 3);
        let manifest = recorder.finalize(false).expect("finalize");
        assert_eq!(manifest.overflow_count, 5);
        assert_eq!(manifest.sources[0].overflow_count, 2);
        assert_eq!(manifest.sources[1].overflow_count, 3);
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
        assert_eq!(manifest.timeline_unit, "microsecond");
        assert_eq!(manifest.normalization, "none");
        assert_eq!(manifest.mix, "isolated-microphone");
        assert_eq!(manifest.consent_record, "fresh-application-consent");
        assert_eq!(manifest.audio_files[0].timeline_start_sample, 0);
        assert_eq!(manifest.audio_files[1].timeline_end_sample, 6);
    }

    #[test]
    fn timeline_blocks_preserve_absolute_session_placement() {
        let root = TempDir::new().expect("temporary directory");
        let mut recorder = SessionRecorder::start(config(&root)).expect("start");
        recorder
            .write_samples_block(
                &[1, 2, 3, 4],
                TimelineBlock {
                    session_start_us: 2_500_000,
                    capture_epoch_id: 7,
                    source_capture_start_ns: 10,
                    callback_arrival_ns: 20,
                    sequence: 3,
                    frame_count: 4,
                    discontinuity_before: true,
                },
            )
            .expect("write timeline block");
        let manifest = recorder.finalize(false).expect("finalize");
        assert_eq!(manifest.schema_version, 2);
        assert_eq!(manifest.alignment_quality, "exact_block_timeline");
        assert_eq!(manifest.timeline_blocks.len(), 1);
        assert_eq!(manifest.timeline_blocks[0].session_start_us, 2_500_000);
        assert_eq!(manifest.timeline_blocks[0].session_end_us, 2_500_250);
        assert_eq!(manifest.timeline_blocks[0].capture_epoch_id, 7);
    }

    #[test]
    fn writes_isolated_system_and_canonical_mix_tracks() {
        let root = TempDir::new().expect("temporary directory");
        let mut recording_config = config(&root);
        recording_config.additional_tracks = vec![
            TrackConfig {
                kind: TrackKind::System,
                sample_rate: 48_000,
                channels: 2,
                samples_per_segment: 4,
                backend: "fixture".to_owned(),
                source_device_id: "system-fixture".to_owned(),
                permission_result: "granted".to_owned(),
                native_sample_format: "signed-16-bit-pcm".to_owned(),
            },
            TrackConfig {
                kind: TrackKind::Mix,
                sample_rate: 16_000,
                channels: 1,
                samples_per_segment: 4,
                backend: "fixture-mixer".to_owned(),
                source_device_id: "application-generated".to_owned(),
                permission_result: "not-applicable".to_owned(),
                native_sample_format: "signed-16-bit-pcm".to_owned(),
            },
        ];
        let mut recorder = SessionRecorder::start(recording_config).expect("start");
        recorder
            .write_track(TrackKind::System, &[1, 2, 3, 4, 5, 6])
            .expect("system");
        recorder
            .write_track(TrackKind::Mix, &[7, 8, 9])
            .expect("mix");
        recorder.record_gap(AudioGap {
            source: TrackKind::System,
            start_us: 10,
            duration_us: 20,
            reason: "fixture".to_owned(),
        });
        recorder.record_drift_correction(DriftCorrection {
            source: TrackKind::System,
            timeline_us: 30,
            ppm: 12,
        });
        let manifest = recorder.finalize(false).expect("finalize");

        assert!(
            manifest
                .audio_files
                .iter()
                .any(|file| file.path == "audio/system-000002.wav")
        );
        assert!(
            manifest
                .audio_files
                .iter()
                .any(|file| file.path == "audio/mix-000001.wav")
        );
        assert_eq!(manifest.gaps.len(), 1);
        assert_eq!(manifest.drift_corrections[0].ppm, 12);
        assert_eq!(manifest.sources.len(), 3);
        assert_eq!(
            manifest.normalization,
            "downmix-linear-resample-16khz-drift-corrected"
        );
        assert_eq!(manifest.mix, "isolated-microphone+system+canonical-mix");
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
    fn recovery_refuses_system_wav_format_mismatch() {
        let root = TempDir::new().expect("temporary directory");
        let recorder = SessionRecorder::start(system_config(&root)).expect("start");
        let session_dir = recorder.session_directory().to_path_buf();
        drop(recorder);
        fs::remove_file(session_dir.join("audio/system-000001.wav")).expect("remove segment");
        let writer = hound::WavWriter::create(
            session_dir.join("audio/system-000001.wav"),
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
    fn recovery_truncates_data_to_checkpoint_boundary() {
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
        marker["checkpointed_tracks"]["mic"] = serde_json::json!(2);
        fs::write(
            session_dir.join("recovery/active.json"),
            serde_json::to_vec_pretty(&marker).expect("marker JSON"),
        )
        .expect("write marker");
        let original_path = session_dir.join("audio/mic-000001.wav");
        let original = fs::read(&original_path).expect("original WAV");

        let recovered = recover_sessions(root.path()).expect("recover");
        assert_eq!(recovered[0].audio_files[0].sample_count, 2);
        assert_eq!(fs::read(original_path).expect("preserved WAV"), original);
        let recovered_path = session_dir.join(&recovered[0].audio_files[0].path);
        let reader = hound::WavReader::open(recovered_path).expect("recovered WAV");
        assert_eq!(reader.duration(), 2);
        assert!(
            session_dir
                .join(
                    Path::new(&recovered[0].audio_files[0].path)
                        .parent()
                        .expect("recovery artifact directory")
                )
                .join("session.json")
                .is_file()
        );
    }

    #[test]
    fn wav_repair_never_extends_the_original_file() {
        let root = TempDir::new().expect("temporary directory");
        let path = root.path().join("short.wav");
        let specification = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, specification).expect("create WAV");
        for sample in [1_i16, 2, 3] {
            writer.write_sample(sample).expect("write sample");
        }
        writer.finalize().expect("finalize WAV");

        let original = fs::read(&path).expect("original WAV");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open WAV");
        let result = super::truncate_wav_to_samples(&mut file, 4);

        assert!(matches!(
            result,
            Err(super::RecordingError::InvalidRecoveryMarker)
        ));
        assert_eq!(fs::read(path).expect("unchanged WAV"), original);
    }

    #[test]
    fn checkpoint_cadence_is_per_track_not_aggregate_progress() {
        let root = TempDir::new().expect("temporary directory");
        let mut recorder = SessionRecorder::start(system_config(&root)).expect("start");
        recorder.write_samples(&[1, 2, 3]).expect("mic write");
        recorder
            .write_track(TrackKind::System, &[1, 2, 3])
            .expect("system write");
        let marker: serde_json::Value = serde_json::from_slice(
            &fs::read(recorder.session_directory().join("recovery/active.json")).expect("marker"),
        )
        .expect("marker JSON");
        assert_eq!(marker["checkpointed_tracks"]["mic"], 0);
        assert_eq!(marker["checkpointed_tracks"]["system"], 0);
    }

    #[test]
    fn recovery_accepts_legacy_milestone_one_manifest_without_sources() {
        let root = TempDir::new().expect("temporary directory");
        let session_dir = {
            let mut recorder = SessionRecorder::start(config(&root)).expect("start");
            recorder.write_samples(&[1, 2]).expect("write");
            recorder.checkpoint().expect("checkpoint");
            recorder.session_directory().to_path_buf()
        };
        let manifest_path = session_dir.join("session.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("manifest"))
                .expect("manifest JSON");
        manifest.as_object_mut().expect("object").remove("sources");
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("manifest JSON"),
        )
        .expect("legacy manifest");
        let recovered = recover_sessions(root.path()).expect("recover legacy");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].audio_files[0].sample_count, 2);
    }

    #[test]
    #[cfg(unix)]
    fn recovery_rejects_symlinked_data_root() {
        let real = TempDir::new().expect("real root");
        let parent = TempDir::new().expect("parent");
        let linked = parent.path().join("linked-root");
        std::os::unix::fs::symlink(real.path(), &linked).expect("symlink");
        assert!(matches!(
            recover_sessions(&linked),
            Err(super::RecordingError::PathRejected)
        ));
    }

    #[test]
    fn recovery_rejects_oversized_marker_before_parsing() {
        let root = TempDir::new().expect("temporary directory");
        let session_dir = {
            let recorder = SessionRecorder::start(config(&root)).expect("start");
            recorder.session_directory().to_path_buf()
        };
        fs::write(
            session_dir.join("recovery/active.json"),
            vec![
                b' ';
                usize::try_from(super::MAX_RECOVERY_JSON_BYTES + 1).expect("test limit fits usize")
            ],
        )
        .expect("oversized marker");
        assert!(matches!(
            recover_sessions(root.path()),
            Err(super::RecordingError::RecoveryLimitExceeded)
        ));
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

    #[test]
    #[cfg(unix)]
    fn existing_data_root_permissions_are_not_changed() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().expect("temporary directory");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).expect("set mode");
        let recorder = SessionRecorder::start(config(&root)).expect("start");
        assert_eq!(
            fs::metadata(root.path())
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        drop(recorder);
    }
}
