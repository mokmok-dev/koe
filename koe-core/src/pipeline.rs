//! Recording pipeline orchestration.

mod chunk;
mod consumer;
mod disk_check;
mod error;
mod file_writer;
mod metrics;
mod mixer;
mod monitor;
mod shutdown;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use koe_ffi::{
    AudioCallback, AudioSourceConfig, OutputFormat, Permission, PermissionStatus, RecordingError,
    SpeechEngine, TranscriptFormat, TranscriptionCallback, TranscriptionSegment, check_permission,
    start_capture, start_transcription, validate_capture_source, validate_locale,
    validate_output_path,
};
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::codec::{AudioEncoder, OggComments, create_encoder};
use crate::transcript::{TranscriptFormatter, TranscriptMeta, create_formatter};

pub use chunk::AudioChunk;
pub use consumer::{
    ConsumerContext, NullSpeechFeeder, SpeechFeeder, TranscriptionFeeder, spawn_consumer,
};
pub use disk_check::{available_disk_space, check_disk_space};
pub use error::PipelineError;
pub use file_writer::FileWriter;
/// Progress payload types for [`RecordingPipeline::subscribe_progress`].
/// Segment live feed: [`RecordingPipeline::subscribe_segments`].
pub use koe_ffi::{RecordingState, RecordingStatus};
pub use metrics::{PipelineMetrics, PipelineMetricsSnapshot};
pub use monitor::{
    AudioMonitor, MONITOR_BUFFER_FRAMES, MONITOR_BYTES_PER_FRAME, MONITOR_CHANNEL_COUNT,
    MONITOR_SAMPLE_RATE_HZ, MonitorError, NullMonitor, create_monitor, create_monitor_or_null,
};
pub use shutdown::{FORCE_JOIN_BUDGET, SHUTDOWN_BUDGET, ShutdownMode, StopResult};

/// Configuration for a recording session.
///
/// Feature toggles (`enable_aec`, `comfort_noise`, `monitor`, `transcribe`) are
/// independent session flags; collapsing them into an enum would obscure that.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct PipelineConfig {
    /// Audio capture source.
    pub source: AudioSourceConfig,
    /// Path for encoded audio output.
    pub output_path: PathBuf,
    /// Optional path for transcript output.
    pub transcript_output_path: Option<PathBuf>,
    /// BCP-47 locale for speech recognition (ignored when [`Self::transcribe`] is false).
    pub locale: String,
    /// Which speech engine to use (on-device / network / auto).
    ///
    /// Ignored when [`Self::transcribe`] is false.
    pub speech_engine: SpeechEngine,
    /// Encoded audio format.
    pub audio_format: OutputFormat,
    /// Transcript file format (ignored when [`Self::transcribe`] is false).
    pub transcript_format: TranscriptFormat,
    /// Enable acoustic echo cancellation (for `Both` sources).
    pub enable_aec: bool,
    /// Inject comfort noise during echo-only periods.
    pub comfort_noise: bool,
    /// Route clean audio to the default output device.
    ///
    /// When `true`, the pipeline opens a native `AudioQueue` output at start.
    /// Create failures are logged and monitoring is disabled so recording still
    /// proceeds. Write failures after start are also non-fatal.
    pub monitor: bool,
    /// Run on-device speech recognition.
    ///
    /// When `false`, ASR is skipped and [`Self::transcript_output_path`] must be
    /// `None` (validated in [`RecordingPipeline::start`]).
    pub transcribe: bool,
    /// Optional estimated recording duration for disk-space checks.
    pub estimated_duration_hours: Option<f64>,
}

/// Lifecycle state of the recording pipeline.
#[derive(Debug)]
pub enum PipelineState {
    /// Pipeline created but not yet recording.
    Idle,
    /// Actively recording.
    Recording {
        start_time: Instant,
        bytes_written: u64,
        segments: Vec<TranscriptionSegment>,
    },
    /// Recording paused; tap remains alive.
    Paused {
        elapsed_before_pause: Duration,
        bytes_written: u64,
        segments: Vec<TranscriptionSegment>,
    },
    /// Recording has been stopped.
    Stopped,
}

/// Central orchestrator for capture, encoding, transcription, and file output.
pub struct RecordingPipeline {
    config: PipelineConfig,
    state: PipelineState,
    encoder: Arc<Mutex<Box<dyn AudioEncoder>>>,
    transcript_fmt: Arc<Mutex<Box<dyn TranscriptFormatter>>>,
    file_writer: Arc<AsyncMutex<FileWriter>>,
    capture_handles: Vec<Arc<koe_ffi::CaptureHandle>>,
    mixer_task: Option<JoinHandle<()>>,
    transcription_handle: Option<Arc<koe_ffi::TranscriptionHandle>>,
    consumer_task: Option<JoinHandle<Result<(), PipelineError>>>,
    shutdown: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    drop_counter: Arc<std::sync::atomic::AtomicU64>,
    metrics: Arc<PipelineMetrics>,
    segments: Arc<Mutex<Vec<TranscriptionSegment>>>,
    progress_tx: broadcast::Sender<RecordingStatus>,
    segment_tx: broadcast::Sender<TranscriptionSegment>,
    /// Pause-aware origin shared with the consumer progress clock.
    started_at: Arc<Mutex<Instant>>,
    bytes_written: Arc<AtomicU64>,
    /// Live pass-through sink (null when [`PipelineConfig::monitor`] is false).
    monitor: Arc<dyn AudioMonitor>,
}

struct PipelineAudioCallback {
    tx: broadcast::Sender<AudioChunk>,
    paused: Arc<AtomicBool>,
    drop_counter: Arc<std::sync::atomic::AtomicU64>,
}

impl AudioCallback for PipelineAudioCallback {
    fn on_audio(
        &self,
        pcm: Vec<f32>,
        timestamp_ms: u64,
    ) {
        if self.paused.load(Ordering::Relaxed) {
            return;
        }
        if self.tx.send(AudioChunk::new(pcm, timestamp_ms)).is_err() {
            self.drop_counter.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct SideAudioCallback {
    tx: mpsc::Sender<AudioChunk>,
    paused: Arc<AtomicBool>,
    drop_counter: Arc<std::sync::atomic::AtomicU64>,
}

impl AudioCallback for SideAudioCallback {
    fn on_audio(
        &self,
        pcm: Vec<f32>,
        timestamp_ms: u64,
    ) {
        if self.paused.load(Ordering::Relaxed) {
            return;
        }
        if self
            .tx
            .try_send(AudioChunk::new(pcm, timestamp_ms))
            .is_err()
        {
            self.drop_counter.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[allow(clippy::type_complexity)]
fn start_captures(
    config: &PipelineConfig,
    audio_tx: broadcast::Sender<AudioChunk>,
    paused: Arc<AtomicBool>,
    drop_counter: Arc<std::sync::atomic::AtomicU64>,
    shutdown: Arc<AtomicBool>,
) -> Result<(Vec<Arc<koe_ffi::CaptureHandle>>, Option<JoinHandle<()>>), PipelineError> {
    match &config.source {
        AudioSourceConfig::Both { bundle_id } => {
            let (far_tx, far_rx) = mpsc::channel(1024);
            let (near_tx, near_rx) = mpsc::channel(1024);
            let system = start_capture(
                AudioSourceConfig::AppAudio {
                    bundle_id: bundle_id.clone(),
                },
                Box::new(SideAudioCallback {
                    tx: far_tx,
                    paused: Arc::clone(&paused),
                    drop_counter: Arc::clone(&drop_counter),
                }),
            )?;
            let mic = start_capture(
                AudioSourceConfig::Microphone,
                Box::new(SideAudioCallback {
                    tx: near_tx,
                    paused,
                    drop_counter,
                }),
            )?;
            let mixer = mixer::spawn_both_mixer(
                far_rx,
                near_rx,
                audio_tx,
                config.enable_aec,
                config.comfort_noise,
                shutdown,
            );
            Ok((vec![system, mic], Some(mixer)))
        },
        source => {
            let handle = start_capture(
                source.clone(),
                Box::new(PipelineAudioCallback {
                    tx: audio_tx,
                    paused,
                    drop_counter,
                }),
            )?;
            Ok((vec![handle], None))
        },
    }
}

struct PipelineTranscriptionCallback {
    segments: Arc<Mutex<Vec<TranscriptionSegment>>>,
    transcript: Arc<Mutex<Box<dyn TranscriptFormatter>>>,
    metrics: Arc<PipelineMetrics>,
    /// Live feed for CLI/GUI progress (partials + finals).
    segment_tx: broadcast::Sender<TranscriptionSegment>,
}

impl TranscriptionCallback for PipelineTranscriptionCallback {
    fn on_segment(
        &self,
        segment: TranscriptionSegment,
    ) {
        // Forward partials for live preview (`current_output`); finals also
        // update the durable segment list and metrics.
        if let Ok(mut transcript) = self.transcript.lock() {
            transcript.write_segment(&segment);
        }
        let _ = self.segment_tx.send(segment.clone());
        if segment.is_final {
            if let Ok(mut segments) = self.segments.lock() {
                segments.push(segment);
            }
            self.metrics.record_segment();
        }
    }

    fn on_error(
        &self,
        error: String,
    ) {
        log::error!("transcription error: {error}");
    }
}

impl RecordingPipeline {
    /// Validates configuration, starts native capture, and spawns the consumer.
    ///
    /// # Errors
    ///
    /// Returns [`PipelineError`] when validation, permissions, or setup fails.
    pub async fn start(config: PipelineConfig) -> Result<Self, PipelineError> {
        validate_config(&config)?;
        check_permissions(&config.source)?;
        let audio_format = config.audio_format.clone();
        check_disk_space(
            &config.output_path,
            &audio_format,
            config.estimated_duration_hours,
        )?;

        if config.output_path.exists() {
            return Err(RecordingError::OutputExists {
                path: config
                    .output_path
                    .to_str()
                    .unwrap_or("<invalid utf-8>")
                    .to_owned(),
            }
            .into());
        }

        let comments = OggComments::for_session(&config.source, &config.locale);
        let encoder = create_encoder(&audio_format, Some(&comments))?;

        // Start capture before any `.await` so ScreenCaptureKit / CoreGraphics
        // run on the runtime's initial (CLI main) thread.
        let shutdown = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let drop_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let metrics = PipelineMetrics::new();
        let segments = Arc::new(Mutex::new(Vec::new()));
        let transcript_meta = TranscriptMeta::for_session(&config.source, &config.locale);
        let transcript_fmt = create_formatter(config.transcript_format, &transcript_meta);
        let transcript = Arc::new(Mutex::new(transcript_fmt));

        let (segment_tx, _) = broadcast::channel(256);
        let (transcription_handle, speech) = open_transcription(
            &config,
            &segments,
            &transcript,
            &metrics,
            segment_tx.clone(),
        )?;

        let (audio_tx, _audio_rx) = broadcast::channel(64);
        let (capture_handles, mixer_task) = start_captures(
            &config,
            audio_tx.clone(),
            Arc::clone(&paused),
            Arc::clone(&drop_counter),
            Arc::clone(&shutdown),
        )?;

        let file_writer = FileWriter::create(&config.output_path).await?;

        let encoder = Arc::new(Mutex::new(encoder));
        let file_writer = Arc::new(AsyncMutex::new(file_writer));
        let (progress_tx, _) = broadcast::channel(32);
        let started_at = Instant::now();
        let started_at = Arc::new(Mutex::new(started_at));
        let bytes_written = Arc::new(AtomicU64::new(0));
        let monitor = create_monitor_or_null(config.monitor);

        let consumer_ctx = ConsumerContext {
            encoder: Arc::clone(&encoder),
            speech,
            writer: Arc::clone(&file_writer),
            metrics: Arc::clone(&metrics),
            shutdown: Arc::clone(&shutdown),
            paused: Arc::clone(&paused),
            progress_tx: progress_tx.clone(),
            started_at: Arc::clone(&started_at),
            bytes_written: Arc::clone(&bytes_written),
            monitor: Arc::clone(&monitor),
        };
        let consumer_task = spawn_consumer(audio_tx.subscribe(), consumer_ctx);
        let start_time = *started_at
            .lock()
            .map_err(|_| PipelineError::InvalidState("started_at lock poisoned".to_owned()))?;

        Ok(Self {
            config,
            state: PipelineState::Recording {
                start_time,
                bytes_written: 0,
                segments: Vec::new(),
            },
            encoder,
            transcript_fmt: transcript,
            file_writer,
            capture_handles,
            mixer_task,
            transcription_handle,
            consumer_task: Some(consumer_task),
            shutdown,
            paused,
            drop_counter,
            metrics,
            segments,
            progress_tx,
            segment_tx,
            started_at,
            bytes_written,
            monitor,
        })
    }

    /// Pauses audio production while keeping the native tap alive.
    pub fn pause(&mut self) {
        if let PipelineState::Recording {
            start_time,
            bytes_written,
            segments,
        } = std::mem::replace(&mut self.state, PipelineState::Idle)
        {
            self.paused.store(true, Ordering::Relaxed);
            self.state = PipelineState::Paused {
                elapsed_before_pause: start_time.elapsed(),
                bytes_written,
                segments,
            };
            self.publish_status(RecordingState::Paused, 0.0, 0.0);
        }
    }

    /// Resumes recording after a pause.
    pub fn resume(&mut self) {
        if let PipelineState::Paused {
            elapsed_before_pause,
            bytes_written,
            segments,
        } = std::mem::replace(&mut self.state, PipelineState::Idle)
        {
            self.paused.store(false, Ordering::Relaxed);
            let start_time = Instant::now()
                .checked_sub(elapsed_before_pause)
                .unwrap_or_else(Instant::now);
            if let Ok(mut origin) = self.started_at.lock() {
                *origin = start_time;
            }
            self.state = PipelineState::Recording {
                start_time,
                bytes_written,
                segments,
            };
            self.publish_status(RecordingState::Recording, 0.0, 0.0);
        }
    }

    /// Returns whether the pipeline is paused.
    #[must_use]
    pub const fn is_paused(&self) -> bool {
        matches!(self.state, PipelineState::Paused { .. })
    }

    /// Current lifecycle state.
    #[must_use]
    pub const fn state(&self) -> &PipelineState {
        &self.state
    }

    /// Active configuration.
    #[must_use]
    pub const fn config(&self) -> &PipelineConfig {
        &self.config
    }

    /// Runtime metrics snapshot.
    #[must_use]
    pub fn metrics(&self) -> PipelineMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Returns the primary capture handle when a session is active.
    #[must_use]
    pub fn capture_handle(&self) -> Option<&Arc<koe_ffi::CaptureHandle>> {
        self.capture_handles.first()
    }

    /// Transcription handle for test injection of segments.
    #[must_use]
    pub const fn transcription_handle(&self) -> Option<&Arc<koe_ffi::TranscriptionHandle>> {
        self.transcription_handle.as_ref()
    }

    /// Subscribes to recording progress for CLI/GUI surfaces.
    ///
    /// Delivery is best-effort over a bounded broadcast channel: a slow
    /// subscriber may observe [`broadcast::error::RecvError::Lagged`] and miss
    /// intermediate meter updates. Lifecycle transitions (`Paused`, `Stopping`,
    /// `Stopped`) are emitted explicitly from pause/resume/stop.
    #[must_use]
    pub fn subscribe_progress(&self) -> broadcast::Receiver<RecordingStatus> {
        self.progress_tx.subscribe()
    }

    /// Subscribes to live transcription segments (partials and finals).
    ///
    /// Delivery is best-effort over a bounded broadcast channel. Unlike
    /// [`Self::subscribe_progress`] (where a missed meter tick is just a stale
    /// snapshot), a lagged subscriber **permanently skips** those segment
    /// events — finals are not resent. Handle
    /// [`broadcast::error::RecvError::Lagged`] accordingly.
    ///
    /// When transcription is disabled, no events are sent; `recv` stays pending
    /// until the pipeline (and this sender) are dropped — use `select!` rather
    /// than awaiting this channel alone.
    #[must_use]
    pub fn subscribe_segments(&self) -> broadcast::Receiver<TranscriptionSegment> {
        self.segment_tx.subscribe()
    }

    fn publish_status(
        &self,
        state: RecordingState,
        level_left: f32,
        level_right: f32,
    ) {
        let elapsed_ms = self.started_at.lock().map_or(0, |started_at| {
            u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
        });
        let _ = self.progress_tx.send(RecordingStatus {
            elapsed_ms,
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
            level_left,
            level_right,
            state,
        });
    }
}

fn validate_config(config: &PipelineConfig) -> Result<(), PipelineError> {
    validate_capture_source(&config.source)?;
    if config.transcribe {
        validate_locale(&config.locale)?;
    } else if config.transcript_output_path.is_some() {
        return Err(RecordingError::ConfigError {
            msg: "transcript_output_path requires transcribe=true".to_owned(),
        }
        .into());
    }
    let output = config
        .output_path
        .to_str()
        .ok_or_else(|| RecordingError::ConfigError {
            msg: "output path is not valid UTF-8".to_owned(),
        })?;
    validate_output_path(output)?;
    if let Some(path) = &config.transcript_output_path
        && path.exists()
    {
        return Err(RecordingError::OutputExists {
            path: path.to_str().unwrap_or("<invalid utf-8>").to_owned(),
        }
        .into());
    }
    Ok(())
}

fn open_transcription(
    config: &PipelineConfig,
    segments: &Arc<Mutex<Vec<TranscriptionSegment>>>,
    transcript: &Arc<Mutex<Box<dyn TranscriptFormatter>>>,
    metrics: &Arc<PipelineMetrics>,
    segment_tx: broadcast::Sender<TranscriptionSegment>,
) -> Result<TranscriptionSetup, PipelineError> {
    if !config.transcribe {
        return Ok((None, Arc::new(NullSpeechFeeder)));
    }
    let transcription_callback = PipelineTranscriptionCallback {
        segments: Arc::clone(segments),
        transcript: Arc::clone(transcript),
        metrics: Arc::clone(metrics),
        segment_tx,
    };
    let handle = start_transcription(
        config.locale.clone(),
        config.speech_engine,
        Box::new(transcription_callback),
    )?;
    let feeder = Arc::new(TranscriptionFeeder::new(Arc::clone(&handle)));
    Ok((Some(handle), feeder))
}

type TranscriptionSetup = (
    Option<Arc<koe_ffi::TranscriptionHandle>>,
    Arc<dyn SpeechFeeder>,
);

fn check_permissions(source: &AudioSourceConfig) -> Result<(), PipelineError> {
    for permission in required_permissions(source) {
        let status = check_permission(permission);
        if status != PermissionStatus::Authorized {
            let name = permission_name(permission);
            return Err(PipelineError::PermissionDenied(name.to_owned()));
        }
    }
    Ok(())
}

fn required_permissions(source: &AudioSourceConfig) -> Vec<Permission> {
    match source {
        AudioSourceConfig::Microphone => vec![Permission::Microphone],
        AudioSourceConfig::AppAudio { .. } | AudioSourceConfig::PidAudio { .. } => {
            vec![Permission::ScreenRecording]
        },
        AudioSourceConfig::Both { .. } => {
            vec![Permission::Microphone, Permission::ScreenRecording]
        },
    }
}

const fn permission_name(permission: Permission) -> &'static str {
    match permission {
        Permission::Microphone => "microphone",
        Permission::ScreenRecording => "screen recording",
        Permission::Accessibility => "accessibility",
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use koe_ffi::{
        AppInfo, NativeProvider, Permission, PermissionStatus, register_native_provider,
    };

    use super::*;

    struct TestProvider {
        permissions: Vec<(Permission, PermissionStatus)>,
    }

    impl NativeProvider for TestProvider {
        fn check_permission(
            &self,
            permission: Permission,
        ) -> PermissionStatus {
            self.permissions
                .iter()
                .find(|(perm, _)| *perm == permission)
                .map_or(PermissionStatus::NotDetermined, |(_, status)| *status)
        }

        fn request_permission(
            &self,
            permission: Permission,
        ) -> PermissionStatus {
            self.check_permission(permission)
        }

        fn enumerate_apps(&self) -> Vec<AppInfo> {
            Vec::new()
        }
    }

    fn install_provider(permissions: Vec<(Permission, PermissionStatus)>) {
        koe_ffi::set_capture_stub(true);
        koe_ffi::set_transcription_stub(true);
        register_native_provider(Box::new(TestProvider { permissions }));
    }

    fn test_config(output: &Path) -> PipelineConfig {
        PipelineConfig {
            source: AudioSourceConfig::Microphone,
            output_path: output.to_path_buf(),
            transcript_output_path: None,
            locale: "en-US".into(),
            speech_engine: koe_ffi::SpeechEngine::Auto,
            audio_format: OutputFormat::Wav {
                bits_per_sample: 16,
            },
            transcript_format: TranscriptFormat::Txt,
            enable_aec: false,
            comfort_noise: false,
            monitor: false,
            transcribe: true,
            estimated_duration_hours: None,
        }
    }

    #[tokio::test]
    async fn start_stop_with_authorized_permissions() {
        install_provider(vec![(Permission::Microphone, PermissionStatus::Authorized)]);
        let output = std::env::temp_dir().join(format!(
            "koe-pipeline-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));

        let mut pipeline = RecordingPipeline::start(test_config(&output))
            .await
            .expect("start");

        if let Some(handle) = pipeline.capture_handle() {
            handle.deliver_audio(vec![0.1, -0.1, 0.2, -0.2], 10);
            handle.deliver_audio(vec![0.3, -0.3], 20);
        }

        let summary = pipeline.stop().await.expect("stop");
        assert!(summary.bytes_written > 0);
        assert_eq!(summary.dropped_audio_frames, 0);

        let _ = std::fs::remove_file(output);
    }

    #[tokio::test]
    async fn pause_resume_cycle() {
        install_provider(vec![(Permission::Microphone, PermissionStatus::Authorized)]);
        let output = std::env::temp_dir().join(format!(
            "koe-pipeline-pause-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));

        let mut pipeline = RecordingPipeline::start(test_config(&output))
            .await
            .expect("start");
        assert!(!pipeline.is_paused());

        pipeline.pause();
        assert!(pipeline.is_paused());

        if let Some(handle) = pipeline.capture_handle() {
            handle.deliver_audio(vec![1.0, -1.0], 30);
        }

        pipeline.resume();
        assert!(!pipeline.is_paused());

        if let Some(handle) = pipeline.capture_handle() {
            handle.deliver_audio(vec![0.5, -0.5], 40);
        }

        let _ = pipeline.stop().await.expect("stop");
        let _ = std::fs::remove_file(output);
    }

    #[tokio::test]
    async fn progress_emits_lifecycle_states() {
        install_provider(vec![(Permission::Microphone, PermissionStatus::Authorized)]);
        let output = std::env::temp_dir().join(format!(
            "koe-pipeline-progress-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));

        let mut pipeline = RecordingPipeline::start(test_config(&output))
            .await
            .expect("start");
        let mut progress = pipeline.subscribe_progress();

        pipeline.pause();
        let paused = progress.try_recv().expect("paused status");
        assert_eq!(paused.state, RecordingState::Paused);

        pipeline.resume();
        let resumed = progress.try_recv().expect("recording status");
        assert_eq!(resumed.state, RecordingState::Recording);

        let _ = pipeline.stop().await.expect("stop");

        let mut saw_stopping = false;
        let mut saw_stopped = false;
        while let Ok(status) = progress.try_recv() {
            saw_stopping |= status.state == RecordingState::Stopping;
            saw_stopped |= status.state == RecordingState::Stopped;
        }
        assert!(saw_stopping);
        assert!(saw_stopped);

        let _ = std::fs::remove_file(output);
    }

    #[tokio::test]
    async fn start_without_transcription() {
        install_provider(vec![(Permission::Microphone, PermissionStatus::Authorized)]);
        let output = std::env::temp_dir().join(format!(
            "koe-pipeline-no-asr-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));

        let mut config = test_config(&output);
        config.transcribe = false;
        let mut pipeline = RecordingPipeline::start(config).await.expect("start");
        assert!(pipeline.transcription_handle().is_none());

        if let Some(handle) = pipeline.capture_handle() {
            handle.deliver_audio(vec![0.1, -0.1], 10);
        }

        let summary = pipeline.stop().await.expect("stop");
        assert!(summary.bytes_written > 0);
        assert_eq!(summary.transcript_segment_count, 0);

        let _ = std::fs::remove_file(output);
    }

    #[tokio::test]
    async fn start_with_denied_permission_fails() {
        install_provider(vec![(Permission::Microphone, PermissionStatus::Denied)]);
        let output = std::env::temp_dir().join("koe-pipeline-denied.wav");
        let Err(err) = RecordingPipeline::start(test_config(&output)).await else {
            panic!("permission denied");
        };
        assert!(matches!(err, PipelineError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn start_with_insufficient_disk_space_fails() {
        install_provider(vec![(Permission::Microphone, PermissionStatus::Authorized)]);
        let output = std::env::temp_dir().join("koe-pipeline-disk.wav");
        let mut config = test_config(&output);
        config.estimated_duration_hours = Some(1_000_000.0);
        config.audio_format = OutputFormat::Wav {
            bits_per_sample: 32,
        };

        let Err(err) = RecordingPipeline::start(config).await else {
            panic!("disk full");
        };
        assert!(matches!(
            err,
            PipelineError::Recording(RecordingError::InsufficientDiskSpace { .. })
        ));
    }
}
