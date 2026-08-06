use std::{
    collections::VecDeque,
    io,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use clap::{Parser, Subcommand, ValueEnum};
use koe_app::{AppError, RecorderCoordinator, RecordingConsent};
use koe_audio::{
    AudioBackend, AudioCapability, AudioDevice, AudioError, AudioStream, CanonicalNormalizer,
    CpalBackend, DriftEstimator, FrameConsumer, OpenSource, frame_ring, mix_canonical,
    process_timeline_now_ns,
};
use koe_core::{CapabilityState, NetworkPolicy, SourceKind};
use koe_model::{
    AsrSessionSettings, DigestAllowlist, FoundryLocalAdapter, InstallOptions, KoeModelManager,
    ModelDescriptor, ModelManager,
};
use koe_recording::{
    AudioGap, DriftCorrection, RecordingConfig, RecordingError, TimelineBlock, TrackConfig,
    TrackKind, recover_sessions,
};
use koe_transcript::{SegmentId, TranscriptModel, TranscriptSegment, TranscriptStore};
use serde::Serialize;

const MIX_JITTER_CAPACITY: usize = 32_000;
const CANONICAL_SAMPLE_RATE: u64 = 16_000;

#[derive(Default)]
struct TimelineTrack {
    start_sample: Option<u64>,
    samples: VecDeque<i16>,
}

impl TimelineTrack {
    fn end_sample(&self) -> Option<u64> {
        self.start_sample.map(|start| {
            start.saturating_add(u64::try_from(self.samples.len()).unwrap_or(u64::MAX))
        })
    }

    fn push(
        &mut self,
        start_ns: u64,
        samples: &[i16],
    ) -> Option<(u64, u64)> {
        let requested_start = start_ns.saturating_mul(CANONICAL_SAMPLE_RATE) / 1_000_000_000;
        if self.start_sample.is_none() {
            self.start_sample = Some(requested_start);
        }
        let end = self.end_sample().unwrap_or(requested_start);
        let mut input = samples;
        let gap = if requested_start > end {
            let missing = usize::try_from(requested_start - end).unwrap_or(usize::MAX);
            if missing > MIX_JITTER_CAPACITY {
                self.samples.clear();
                self.start_sample = Some(requested_start);
            } else {
                self.samples.extend(std::iter::repeat_n(0, missing));
            }
            Some((
                end.saturating_mul(1_000_000_000) / CANONICAL_SAMPLE_RATE,
                (requested_start - end).saturating_mul(1_000_000_000) / CANONICAL_SAMPLE_RATE,
            ))
        } else {
            let overlap = usize::try_from(end - requested_start).unwrap_or(usize::MAX);
            input = input.get(overlap.min(input.len())..).unwrap_or_default();
            None
        };
        self.samples.extend(input);
        gap
    }

    fn sample_at(
        &self,
        position: u64,
    ) -> i16 {
        let Some(start) = self.start_sample else {
            return 0;
        };
        let Ok(index) = usize::try_from(position.saturating_sub(start)) else {
            return 0;
        };
        if position < start {
            0
        } else {
            self.samples.get(index).copied().unwrap_or(0)
        }
    }

    fn consume_before(
        &mut self,
        position: u64,
    ) {
        let Some(start) = self.start_sample else {
            return;
        };
        let count = usize::try_from(position.saturating_sub(start))
            .unwrap_or(usize::MAX)
            .min(self.samples.len());
        self.samples.drain(..count);
        if self.samples.is_empty() {
            self.start_sample = None;
        } else {
            self.start_sample = Some(start.saturating_add(count as u64));
        }
    }
}

#[derive(Default)]
struct TimelineMapper {
    capture_anchor_ns: Option<u64>,
    session_anchor_ns: u64,
}

impl TimelineMapper {
    fn map(
        &mut self,
        capture_ns: u64,
        session_now_ns: u64,
        discontinuity: bool,
    ) -> u64 {
        if self.capture_anchor_ns.is_none() || discontinuity {
            self.capture_anchor_ns = Some(capture_ns);
            self.session_anchor_ns = session_now_ns;
        }
        self.session_anchor_ns
            .saturating_add(capture_ns.saturating_sub(self.capture_anchor_ns.unwrap_or(capture_ns)))
    }
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum OutputFormat {
    #[default]
    Human,
    Json,
    Jsonl,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SourceArgument {
    Mic,
    System,
}

impl From<SourceArgument> for SourceKind {
    fn from(value: SourceArgument) -> Self {
        match value {
            SourceArgument::Mic => Self::Microphone,
            SourceArgument::System => Self::System,
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "koe", version, about = "Offline recording and transcription")]
struct Cli {
    #[arg(long, value_enum, default_value_t)]
    output_format: OutputFormat,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show machine-detected audio capabilities.
    Capabilities,
    /// Inspect audio devices.
    Devices {
        #[command(subcommand)]
        command: DeviceCommand,
    },
    /// Show microphone and system-audio permission/capability state.
    Permissions {
        #[command(subcommand)]
        command: PermissionCommand,
    },
    /// Run local configuration checks without network access.
    Doctor,
    /// Inspect and manage locally installed ASR models.
    Models {
        /// App-owned data root shared with `record --output`.
        #[arg(long)]
        data_root: PathBuf,
        #[command(subcommand)]
        command: ModelsCommand,
    },
    /// Record microphone PCM until Ctrl-C.
    Record {
        /// Opaque stable microphone ID from `devices list`.
        #[arg(long)]
        mic: String,
        /// Optional system-audio device ID from `devices list --source system`.
        #[arg(long)]
        system: Option<String>,
        /// Reserved model selector; Milestone 2 records audio without ASR.
        #[arg(long)]
        model: String,
        /// App-owned data root below which `sessions/<uuid>` is created.
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 48_000)]
        sample_rate: u32,
        #[arg(long, default_value_t = 1)]
        channels: u16,
        /// Confirm this one recording after reviewing sources and destination.
        #[arg(long)]
        consent: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DeviceCommand {
    /// List devices for an explicit source kind.
    List {
        #[arg(long, value_enum, default_value = "mic")]
        source: SourceArgument,
    },
}

#[derive(Debug, Subcommand)]
enum ModelsCommand {
    /// List catalog/installed/loaded models. Catalog and install require
    /// explicit `--network` consent.
    List {
        #[arg(long, conflicts_with = "loaded")]
        installed: bool,
        #[arg(long, conflicts_with = "installed")]
        loaded: bool,
        #[arg(long)]
        network: bool,
    },
    /// Install a model after explicit network consent.
    Install {
        selector: String,
        #[arg(long)]
        network: bool,
        #[arg(long)]
        force: bool,
    },
    /// Show an installed model manifest, digest inventory and license.
    Show { installed_id: String },
    /// Remove an installed model. Refused while loaded or in use.
    Remove { installed_id: String },
    /// Record a chunk-size latency/WER/RTF baseline for an installed model.
    Benchmark {
        installed_id: String,
        #[arg(long, default_value_t = 160)]
        chunk_ms: u64,
    },
}

#[derive(Debug, Subcommand)]
enum PermissionCommand {
    Status,
}

#[derive(Debug, Serialize)]
struct DoctorReport<'a> {
    schema_version: u32,
    platform: &'static str,
    network_accessed: bool,
    audio_backend: &'a str,
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    code: &'static str,
    message: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match execute(&cli, &CpalBackend::default(), &mut io::stdout()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let envelope = ErrorEnvelope {
                code: error.code(),
                message: error.to_string(),
            };
            match serde_json::to_string(&envelope) {
                Ok(line) => eprintln!("{line}"),
                Err(_) => eprintln!("{{\"code\":\"KOE-INTERNAL\",\"message\":\"error\"}}"),
            }
            ExitCode::FAILURE
        },
    }
}

fn execute(
    cli: &Cli,
    backend: &impl AudioBackend,
    output: &mut impl io::Write,
) -> Result<(), CliError> {
    match &cli.command {
        Command::Capabilities => {
            let capabilities = backend.capabilities()?;
            render_capabilities(&capabilities, cli.output_format, output)?;
        },
        Command::Permissions {
            command: PermissionCommand::Status,
        } => {
            let capabilities = backend.permissions()?;
            render_capabilities(&capabilities, cli.output_format, output)?;
        },
        Command::Devices {
            command: DeviceCommand::List { source },
        } => {
            let devices = backend.enumerate((*source).into())?;
            render_devices(&devices, cli.output_format, output)?;
        },
        Command::Doctor => {
            let capabilities = backend.capabilities()?;
            let microphone = capabilities
                .iter()
                .find(|capability| capability.source == SourceKind::Microphone);
            let status = if microphone
                .is_some_and(|capability| capability.state == CapabilityState::Supported)
            {
                "ok"
            } else {
                "degraded"
            };
            let audio_backend =
                microphone.map_or("unknown", |capability| capability.backend.as_str());
            let report = DoctorReport {
                schema_version: 1,
                platform: std::env::consts::OS,
                network_accessed: false,
                audio_backend,
                status,
            };
            render(&report, cli.output_format, output, || {
                format!("status: {status}\naudio backend: {audio_backend}\nnetwork accessed: no")
            })?;
        },
        Command::Models { data_root, command } => {
            run_models_command(data_root, command, cli.output_format, output)?;
        },
        Command::Record {
            mic,
            system,
            model,
            output: data_root,
            sample_rate,
            channels,
            consent,
        } => {
            record(
                backend,
                mic,
                system.as_deref(),
                model,
                data_root,
                *sample_rate,
                *channels,
                *consent,
                cli.output_format,
                output,
            )?;
        },
    }
    Ok(())
}

/// Builds a model manager for the explicit data root. Network operations
/// require `--network` consent; everything else is strictly offline.
fn model_manager(
    data_root: &PathBuf,
    network: bool,
) -> Result<KoeModelManager, CliError> {
    let policy = if network {
        NetworkPolicy::ModelInstallOnly
    } else {
        NetworkPolicy::Denied
    };
    KoeModelManager::new(
        data_root,
        DigestAllowlist::empty(),
        Box::new(FoundryLocalAdapter::new()),
        policy,
    )
    .map_err(CliError::Model)
}

/// Runs one model future on a short-lived current-thread runtime.
fn run_blocking<F, T>(future: F) -> Result<T, CliError>
where
    F: std::future::Future<Output = Result<T, CliError>>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| CliError::Model(koe_model::ModelError::Internal))?;
    runtime.block_on(future)
}

/// Executes one `koe models` subcommand.
#[allow(clippy::too_many_lines)]
fn run_models_command(
    data_root: &PathBuf,
    command: &ModelsCommand,
    format: OutputFormat,
    output: &mut impl io::Write,
) -> Result<(), CliError> {
    match command {
        ModelsCommand::List {
            installed,
            loaded,
            network,
        } => {
            let manager = model_manager(data_root, *network)?;
            let scope = if *installed {
                koe_model::ModelScope::Installed
            } else if *loaded {
                koe_model::ModelScope::Loaded
            } else {
                koe_model::ModelScope::Catalog
            };
            if matches!(scope, koe_model::ModelScope::Catalog) && !network {
                eprintln!(
                    "catalog listing requires --network consent; use --installed for offline use"
                );
            }
            let descriptors =
                run_blocking(async { manager.list(scope).await.map_err(CliError::Model) })?;
            render_model_descriptors(&descriptors, format, output)
        },
        ModelsCommand::Install {
            selector,
            network,
            force,
        } => {
            if !network {
                return Err(CliError::Model(koe_model::ModelError::NetworkDenied));
            }
            let manager = model_manager(data_root, *network)?;
            let (progress, mut progress_rx) = tokio::sync::mpsc::channel(8);
            let selector = selector.parse::<koe_model::ModelSelector>()?;
            let installed = run_blocking(async {
                let options = InstallOptions {
                    policy: NetworkPolicy::ModelInstallOnly,
                    cancel: tokio_util::sync::CancellationToken::new(),
                    progress: Some(progress),
                    force_redownload: *force,
                };
                while let Ok(phase) = progress_rx.try_recv() {
                    match phase {
                        koe_model::ModelProgress::Resolving => {
                            eprintln!("resolving model");
                        },
                        koe_model::ModelProgress::Downloading => {
                            eprintln!("downloading model");
                        },
                        koe_model::ModelProgress::Verifying => {
                            eprintln!("verifying digest inventory");
                        },
                        koe_model::ModelProgress::Installing => {
                            eprintln!("installing manifest");
                        },
                        koe_model::ModelProgress::Done => {},
                    }
                }
                manager
                    .install(&selector, &options)
                    .await
                    .map_err(CliError::Model)
            })?;
            render(&installed, format, output, || {
                format!(
                    "installed: {} ({}) verification={:?}\n{}",
                    installed.manifest.alias.0,
                    installed.manifest.version.0,
                    installed.manifest.verification,
                    license_line(&installed.manifest),
                )
            })
        },
        ModelsCommand::Show { installed_id } => {
            let manager = model_manager(data_root, false)?;
            let id = koe_model::InstalledModelId::parse(installed_id)?;
            let installed = manager.installed_model(&id)?;
            render(&installed, format, output, || {
                let mut lines = vec![
                    format!("model: {}", installed.manifest.alias.0),
                    format!("id: {}", installed.manifest.model_id.0),
                    format!("version: {}", installed.manifest.version.0),
                    format!("variant: {}", installed.manifest.variant),
                    format!("provider: {}", installed.manifest.provider),
                    license_line(&installed.manifest),
                    format!("verification: {:?}", installed.manifest.verification),
                    format!("files: {}", installed.manifest.files.len()),
                ];
                for file in &installed.manifest.files {
                    lines.push(format!(
                        "  {} ({} bytes, sha256 {})",
                        file.path, file.size, file.sha256
                    ));
                }
                if let Ok(report) = manager.benchmarks(&id) {
                    for baseline in &report.baselines {
                        lines.push(format!(
                            "benchmark chunk={}ms latency={}ms wer={:.1}% rtf={:.2}",
                            baseline.chunk_ms,
                            baseline.final_latency_ms,
                            baseline.wer_pct,
                            baseline.rtf,
                        ));
                    }
                }
                lines.join("\n")
            })
        },
        ModelsCommand::Remove { installed_id } => {
            let manager = model_manager(data_root, false)?;
            let id = koe_model::InstalledModelId::parse(installed_id)?;
            run_blocking(async { manager.remove(&id).await.map_err(CliError::Model) })?;
            render(&id.to_string(), format, output, || {
                format!("removed model {id}")
            })
        },
        ModelsCommand::Benchmark {
            installed_id,
            chunk_ms,
        } => {
            let manager = model_manager(data_root, false)?;
            let id = koe_model::InstalledModelId::parse(installed_id)?;
            let baseline = run_blocking(async {
                let settings = AsrSessionSettings {
                    chunk_ms: *chunk_ms,
                    ..AsrSessionSettings::default()
                };
                manager
                    .run_benchmark(&id, &settings, BENCHMARK_AUDIO, "")
                    .await
                    .map_err(CliError::Model)
            })?;
            render(&baseline, format, output, || {
                format!(
                    "baseline chunk={}ms first={}ms final={}ms wer={:.1}% rtf={:.2}",
                    baseline.chunk_ms,
                    baseline.first_result_latency_ms,
                    baseline.final_latency_ms,
                    baseline.wer_pct,
                    baseline.rtf,
                )
            })
        },
    }
}

fn license_line(manifest: &koe_model::ModelManifest) -> String {
    format!(
        "license: {} ({}); see the model card before acceptance",
        manifest.license_id, manifest.license_description
    )
}

fn render_model_descriptors(
    descriptors: &[ModelDescriptor],
    format: OutputFormat,
    output: &mut impl io::Write,
) -> Result<(), CliError> {
    render_collection(descriptors, format, output, || {
        if descriptors.is_empty() {
            "No models available.".to_owned()
        } else {
            descriptors
                .iter()
                .map(|descriptor| {
                    format!(
                        "{}\t{}\t{}\t{} ({})",
                        descriptor.alias.0,
                        descriptor.id.0,
                        descriptor.version.0,
                        descriptor.variant,
                        descriptor.provider,
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    })
}

/// Deterministic 2-second audio used by `koe models benchmark`.
const BENCHMARK_AUDIO: &[i16] = &benchmark_audio();

#[allow(
    clippy::large_stack_arrays,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_possible_wrap
)]
const fn benchmark_audio() -> [i16; 32_000] {
    let mut samples = [0_i16; 32_000];
    let mut index = 0;
    while index < samples.len() {
        samples[index] = ((index % 1999) as i32 - 999) as i16;
        index += 1;
    }
    samples
}

/// Prepares an offline ASR session before any audio stream is opened.
///
/// `model == "none"` disables transcription. Otherwise the model must be
/// already installed; a missing artifact yields
/// [`koe_model::ModelError::OfflineArtifactMissing`] without touching the
/// adapter. The session is created here and fed later through the
/// lock-free feed bridge in the capture loop.
#[allow(clippy::type_complexity)]
fn prepare_asr(
    data_root: &PathBuf,
    model: &str,
) -> Result<Option<(Box<dyn koe_model::StreamingAsrSession>, TranscriptModel)>, CliError> {
    if model == "none" {
        return Ok(None);
    }
    let selector = model.parse::<koe_model::ModelSelector>()?;
    let manager = model_manager(data_root, false)?;
    let installed_id = manager
        .installed_id_for(&selector)?
        .ok_or(koe_model::ModelError::OfflineArtifactMissing)?;
    let settings = AsrSessionSettings::default();
    let loaded = run_blocking(async {
        let manager = &manager;
        manager.load(&installed_id).await.map_err(CliError::Model)
    })?;
    let session = run_blocking(async {
        let manager = &manager;
        manager
            .create_asr_session(&installed_id, &settings)
            .await
            .map_err(CliError::Model)
    })?;
    Ok(Some((
        session,
        TranscriptModel {
            id: loaded.descriptor.id.0,
            version: loaded.descriptor.version.0,
            variant: loaded.descriptor.variant,
        },
    )))
}

fn render_capabilities(
    values: &[AudioCapability],
    format: OutputFormat,
    output: &mut impl io::Write,
) -> Result<(), CliError> {
    render_collection(values, format, output, || {
        values
            .iter()
            .map(|value| {
                format!(
                    "{:?}: {:?}, availability={:?}, permission={:?}, probe={:?} ({})",
                    value.source,
                    value.state,
                    value.availability,
                    value.permission,
                    value.probe_effect,
                    terminal_safe(&value.backend)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn render_devices(
    values: &[AudioDevice],
    format: OutputFormat,
    output: &mut impl io::Write,
) -> Result<(), CliError> {
    render_collection(values, format, output, || {
        if values.is_empty() {
            "No devices available.".to_owned()
        } else {
            values
                .iter()
                .map(|value| {
                    let persistence = if value.persistent {
                        "persistent"
                    } else {
                        "reselect-after-restart"
                    };
                    format!(
                        "{}\t{}\t{}\t{persistence}",
                        terminal_safe(&value.id),
                        terminal_safe(&value.display_name),
                        terminal_safe(&value.backend)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    })
}

fn render_collection<T: Serialize>(
    values: &[T],
    format: OutputFormat,
    output: &mut impl io::Write,
    human: impl FnOnce() -> String,
) -> Result<(), CliError> {
    if matches!(format, OutputFormat::Jsonl) {
        for value in values {
            serde_json::to_writer(&mut *output, value)?;
            writeln!(output)?;
        }
        return Ok(());
    }
    render(values, format, output, human)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn record<B: AudioBackend>(
    backend: &B,
    microphone_id: &str,
    system_id: Option<&str>,
    model: &str,
    data_root: &PathBuf,
    sample_rate: u32,
    channels: u16,
    consent: bool,
    format: OutputFormat,
    output: &mut impl io::Write,
) -> Result<(), CliError> {
    if !consent {
        return Err(CliError::ConsentRequired);
    }
    let asr_enabled = model != "none";
    // Model load and session creation happen strictly before capture and
    // never touch the network (`Denied` policy); a missing artifact is an
    // explicit error instead of an implicit download.
    let prepared_asr = prepare_asr(data_root, model)?;
    report_recovered_sessions(data_root)?;
    let mut stream = backend.open(&OpenSource {
        device_id: microphone_id.to_owned(),
        kind: SourceKind::Microphone,
        sample_rate,
        channels,
    })?;
    let mut system_stream = system_id
        .map(|device_id| {
            backend.open(&OpenSource {
                device_id: device_id.to_owned(),
                kind: SourceKind::System,
                sample_rate,
                channels,
            })
        })
        .transpose()?;
    let microphone_sample_rate = stream.sample_rate();
    let microphone_channels = stream.channels();
    let microphone_backend = backend
        .enumerate(SourceKind::Microphone)?
        .into_iter()
        .find(|device| device.id == microphone_id)
        .map_or_else(|| "unknown".to_owned(), |device| device.backend);
    let system_backend = if let Some(system_id) = system_id {
        backend
            .enumerate(SourceKind::System)?
            .into_iter()
            .find(|device| device.id == system_id)
            .map_or_else(|| "unknown".to_owned(), |device| device.backend)
    } else {
        String::new()
    };
    let mut config =
        RecordingConfig::microphone(data_root, microphone_sample_rate, microphone_channels);
    "pending-stream-start".clone_into(&mut config.permission_result);
    config.backend = microphone_backend;
    microphone_id.clone_into(&mut config.source_device_id);
    stream
        .native_sample_format()
        .manifest_label()
        .clone_into(&mut config.native_sample_format);
    if system_id.is_some() {
        let system_sample_rate = system_stream
            .as_ref()
            .map_or(sample_rate, AudioStream::sample_rate);
        let system_channels = system_stream
            .as_ref()
            .map_or(channels, AudioStream::channels);
        let native_samples_per_segment =
            u64::from(system_sample_rate) * u64::from(system_channels) * 15 * 60;
        config.additional_tracks = vec![
            TrackConfig {
                kind: TrackKind::System,
                sample_rate: system_sample_rate,
                channels: system_channels,
                samples_per_segment: native_samples_per_segment,
                backend: system_backend,
                source_device_id: system_id.unwrap_or("unavailable").to_owned(),
                permission_result: "pending-stream-start".to_owned(),
                native_sample_format: system_stream
                    .as_ref()
                    .map_or("signed-16-bit-pcm", |stream| {
                        stream.native_sample_format().manifest_label()
                    })
                    .to_owned(),
            },
            TrackConfig {
                kind: TrackKind::Mix,
                sample_rate: 16_000,
                channels: 1,
                samples_per_segment: 16_000 * 15 * 60,
                backend: "koe-timeline-mixer".to_owned(),
                source_device_id: "application-generated".to_owned(),
                permission_result: "not-applicable".to_owned(),
                native_sample_format: "signed-16-bit-pcm".to_owned(),
            },
        ];
    } else if asr_enabled {
        // ASR without system audio still needs the canonical 16 kHz mono
        // mix track so the timeline mixer can feed the model runtime.
        config.additional_tracks = vec![TrackConfig {
            kind: TrackKind::Mix,
            sample_rate: 16_000,
            channels: 1,
            samples_per_segment: 16_000 * 15 * 60,
            backend: "koe-timeline-mixer".to_owned(),
            source_device_id: "application-generated".to_owned(),
            permission_result: "not-applicable".to_owned(),
            native_sample_format: "signed-16-bit-pcm".to_owned(),
        }];
    }
    let queue_capacity = config.queue_capacity;
    let (producer, mut consumer) = frame_ring(queue_capacity, 16_384)?;
    let (system_producer, mut system_consumer) = if system_stream.is_some() {
        let (producer, consumer) = frame_ring(queue_capacity, 16_384)?;
        (Some(producer), Some(consumer))
    } else {
        (None, None)
    };
    let safe_microphone_id = terminal_safe(microphone_id);
    let safe_model = terminal_safe(model);
    let asr_note = if asr_enabled {
        "offline ASR; transcript saved to the session transcript dir"
    } else {
        "no model inference (audio-only)"
    };
    if system_id.is_some() {
        eprintln!(
            "confirmed recording: microphone={}, system={}, scope=system-wide, destination={}, retention=until explicitly deleted, model={} ({asr_note}), sharing=none",
            safe_microphone_id,
            terminal_safe(system_id.unwrap_or("none")),
            terminal_safe(&data_root.display().to_string()),
            safe_model,
        );
    } else {
        eprintln!(
            "confirmed recording: microphone={}, system=none, destination={}, retention=until explicitly deleted, model={} ({asr_note}), sharing=none",
            safe_microphone_id,
            terminal_safe(&data_root.display().to_string()),
            safe_model,
        );
    }
    let (coordinator, task) = RecorderCoordinator::spawn(config);
    let recording = match coordinator.start(RecordingConsent {
        microphone: true,
        system_audio: system_id.is_some(),
        storage: true,
    }) {
        Ok(recording) => recording,
        Err(error) => {
            stream.stop()?;
            if let Some(system) = &mut system_stream {
                system.stop()?;
            }
            task.shutdown(&coordinator)?;
            return Err(error.into());
        },
    };
    // The durable manifest, recovery marker, and visible session state exist
    // before either OS stream can deliver a callback.
    let session_clock = Instant::now();
    let session_process_origin_ns = process_timeline_now_ns();
    let mut asr = if let Some((session, model)) = prepared_asr {
        let session_id = recording
            .session_id
            .ok_or(CliError::Model(koe_model::ModelError::Internal))?;
        let transcript_dir = data_root
            .join("sessions")
            .join(session_id.to_string())
            .join("transcript");
        Some(AsrBridge::spawn(session, model, transcript_dir))
    } else {
        None
    };
    if let Err(error) = stream.start(Box::new(producer)) {
        let _failed = coordinator.fail(error.code())?;
        task.shutdown(&coordinator)?;
        return Err(error.into());
    }
    coordinator.record_permission_result(TrackKind::Microphone, "granted")?;
    if let (Some(system), Some(producer)) = (&mut system_stream, system_producer)
        && let Err(error) = system.start(Box::new(producer))
    {
        stream.stop()?;
        let _failed = coordinator.fail(error.code())?;
        task.shutdown(&coordinator)?;
        return Err(error.into());
    }
    if system_stream.is_some() {
        coordinator.record_permission_result(TrackKind::System, "granted")?;
    }
    coordinator.mark_recording()?;
    let interrupts = Arc::new(AtomicUsize::new(0));
    let interrupt_handler = Arc::clone(&interrupts);
    ctrlc::set_handler(move || {
        interrupt_handler.fetch_add(1, Ordering::Relaxed);
    })
    .map_err(|_| CliError::Signal)?;
    eprintln!("recording; press Ctrl-C to stop (press twice to cancel)");

    let mut samples = vec![0_i16; 16_384];
    let mut system_samples = vec![0_i16; 16_384];
    let mut microphone_normalizer =
        CanonicalNormalizer::new(microphone_sample_rate, microphone_channels)?;
    let system_sample_rate = system_stream
        .as_ref()
        .map_or(sample_rate, AudioStream::sample_rate);
    let system_channels = system_stream
        .as_ref()
        .map_or(channels, AudioStream::channels);
    let system_native_format = system_stream.as_ref().map_or(
        koe_audio::NativeSampleFormat::I16,
        AudioStream::native_sample_format,
    );
    let mut system_normalizer = CanonicalNormalizer::new(system_sample_rate, system_channels)?;
    let mut microphone_drift = DriftEstimator::new(microphone_sample_rate)?;
    let mut system_drift = DriftEstimator::new(system_sample_rate)?;
    let mut microphone_mix = TimelineTrack::default();
    let mut system_mix = TimelineTrack::default();
    let mut microphone_timeline = TimelineMapper::default();
    let mut system_timeline = TimelineMapper::default();
    let mut microphone_epoch_id = 0_u64;
    let mut system_epoch_id = 0_u64;
    let mut microphone_active = true;
    let mut system_active = system_stream.is_some();
    let mut microphone_loss_started = None;
    let mut system_loss_started = None;
    while interrupts.load(Ordering::Relaxed) == 0 {
        process_async_control(&consumer, &coordinator, TrackKind::Microphone)?;
        let dropped = consumer.take_dropped_frames();
        if dropped != 0 {
            coordinator.record_overflow(TrackKind::Microphone, dropped)?;
        }
        if consumer.take_device_lost() {
            stream.stop()?;
            coordinator.mark_degraded()?;
            let loss_start = elapsed_ns(session_clock);
            microphone_loss_started = Some(loss_start);
            if let Some((reopened, replacement)) = reopen_source(
                backend,
                &OpenSource {
                    device_id: microphone_id.to_owned(),
                    kind: SourceKind::Microphone,
                    sample_rate: microphone_sample_rate,
                    channels: microphone_channels,
                },
                stream.native_sample_format(),
                queue_capacity,
            ) {
                stream = reopened;
                consumer = replacement;
                microphone_active = true;
                reset_source_pipeline(
                    microphone_sample_rate,
                    microphone_channels,
                    &mut microphone_timeline,
                    &mut microphone_drift,
                    &mut microphone_normalizer,
                    &mut microphone_mix,
                )?;
                microphone_epoch_id = microphone_epoch_id.saturating_add(1);
                record_device_loss_gap(
                    &coordinator,
                    TrackKind::Microphone,
                    microphone_loss_started.take().unwrap_or(loss_start),
                    elapsed_ns(session_clock),
                )?;
                if all_requested_sources_active(
                    system_id.is_some(),
                    microphone_active,
                    system_active,
                ) {
                    coordinator.mark_recording()?;
                }
            } else {
                microphone_active = false;
            }
            if no_capture_source_active(microphone_active, system_active) {
                let _cancelled = coordinator.cancel()?;
                task.shutdown(&coordinator)?;
                return Err(CliError::Audio(AudioError::DeviceLost));
            }
        }
        let lost_discontinuities = consumer.take_discontinuities();
        for _ in 0..lost_discontinuities {
            coordinator.record_discontinuity(
                consumer
                    .discontinuity_timestamp_ns()
                    .saturating_sub(session_process_origin_ns),
            )?;
        }
        while let Some(metadata) = consumer.try_pop(&mut samples)? {
            if metadata.overflow {
                coordinator
                    .record_overflow(TrackKind::Microphone, metadata.dropped_frames.max(1))?;
                continue;
            }
            if let Some(failure) = metadata.runtime_failure {
                persist_runtime_failure(&coordinator, failure)?;
                return Err(failure.audio_error().into());
            }
            if metadata.device_lost {
                stream.stop()?;
                let _cancelled = coordinator.cancel()?;
                task.shutdown(&coordinator)?;
                return Err(CliError::Audio(AudioError::DeviceLost));
            }
            let timeline_ns = microphone_timeline.map(
                metadata.capture_timestamp_ns,
                metadata
                    .callback_arrival_timestamp_ns
                    .saturating_sub(session_process_origin_ns),
                metadata.discontinuity,
            );
            if metadata.discontinuity {
                microphone_epoch_id = microphone_epoch_id.saturating_add(1);
                coordinator.record_discontinuity(timeline_ns)?;
                coordinator.record_gap(AudioGap {
                    source: TrackKind::Microphone,
                    start_us: timeline_ns / 1_000,
                    duration_us: 0,
                    reason: "clock-discontinuity".to_owned(),
                })?;
            }
            let count = usize::try_from(metadata.sample_count)
                .map_err(|_| CliError::Audio(AudioError::UnsupportedFormat))?;
            if count != 0 {
                coordinator.append_block(
                    samples[..count].to_vec(),
                    stored_timeline_block(metadata, timeline_ns, microphone_epoch_id)?,
                )?;
                if system_stream.is_some() || asr.is_some() {
                    normalize_for_mix(
                        &samples[..count],
                        metadata,
                        &mut microphone_normalizer,
                        &mut microphone_drift,
                        timeline_ns,
                        &mut microphone_mix,
                        &coordinator,
                        TrackKind::Microphone,
                    )?;
                }
            }
        }
        if system_consumer
            .as_ref()
            .is_some_and(FrameConsumer::take_device_lost)
        {
            if let Some(system) = &mut system_stream {
                system.stop()?;
            }
            coordinator.mark_degraded()?;
            let loss_start = elapsed_ns(session_clock);
            system_loss_started = Some(loss_start);
            if let Some(system_id) = system_id
                && let Some((reopened, replacement)) = reopen_source(
                    backend,
                    &OpenSource {
                        device_id: system_id.to_owned(),
                        kind: SourceKind::System,
                        sample_rate: system_sample_rate,
                        channels: system_channels,
                    },
                    system_native_format,
                    queue_capacity,
                )
            {
                system_stream = Some(reopened);
                system_consumer = Some(replacement);
                system_active = true;
                reset_source_pipeline(
                    system_sample_rate,
                    system_channels,
                    &mut system_timeline,
                    &mut system_drift,
                    &mut system_normalizer,
                    &mut system_mix,
                )?;
                system_epoch_id = system_epoch_id.saturating_add(1);
                record_device_loss_gap(
                    &coordinator,
                    TrackKind::System,
                    system_loss_started.take().unwrap_or(loss_start),
                    elapsed_ns(session_clock),
                )?;
                if all_requested_sources_active(true, microphone_active, system_active) {
                    coordinator.mark_recording()?;
                }
            } else {
                system_active = false;
            }
            if no_capture_source_active(microphone_active, system_active) {
                let _cancelled = coordinator.cancel()?;
                task.shutdown(&coordinator)?;
                return Err(CliError::Audio(AudioError::DeviceLost));
            }
        }
        if let Some(system_consumer) = &system_consumer {
            process_async_control(system_consumer, &coordinator, TrackKind::System)?;
            let dropped = system_consumer.take_dropped_frames();
            if dropped != 0 {
                coordinator.record_overflow(TrackKind::System, dropped)?;
            }
            while let Some(metadata) = system_consumer.try_pop(&mut system_samples)? {
                if metadata.overflow {
                    coordinator
                        .record_overflow(TrackKind::System, metadata.dropped_frames.max(1))?;
                    continue;
                }
                let count = usize::try_from(metadata.sample_count)
                    .map_err(|_| CliError::Audio(AudioError::UnsupportedFormat))?;
                if let Some(failure) = metadata.runtime_failure {
                    if failure == koe_audio::RuntimeFailure::BufferOverflow {
                        coordinator.record_overflow(TrackKind::System, 1)?;
                        continue;
                    }
                    persist_runtime_failure(&coordinator, failure)?;
                    return Err(failure.audio_error().into());
                }
                let timeline_ns = system_timeline.map(
                    metadata.capture_timestamp_ns,
                    metadata
                        .callback_arrival_timestamp_ns
                        .saturating_sub(session_process_origin_ns),
                    metadata.discontinuity,
                );
                if metadata.discontinuity {
                    system_epoch_id = system_epoch_id.saturating_add(1);
                    coordinator.record_gap(AudioGap {
                        source: TrackKind::System,
                        start_us: timeline_ns / 1_000,
                        duration_us: 0,
                        reason: "clock-discontinuity".to_owned(),
                    })?;
                }
                if count != 0 {
                    coordinator.append_track_block(
                        TrackKind::System,
                        system_samples[..count].to_vec(),
                        stored_timeline_block(metadata, timeline_ns, system_epoch_id)?,
                    )?;
                    normalize_for_mix(
                        &system_samples[..count],
                        metadata,
                        &mut system_normalizer,
                        &mut system_drift,
                        timeline_ns,
                        &mut system_mix,
                        &coordinator,
                        TrackKind::System,
                    )?;
                }
            }
            write_available_mix(
                &mut microphone_mix,
                &mut system_mix,
                &coordinator,
                !microphone_active,
                !system_active,
                asr.as_ref(),
            )?;
        }
        thread::sleep(Duration::from_millis(5));
    }
    if microphone_active {
        stream.stop()?;
    }
    if system_active && let Some(system) = &mut system_stream {
        system.stop()?;
    }
    process_async_control(&consumer, &coordinator, TrackKind::Microphone)?;
    while let Some(metadata) = consumer.try_pop(&mut samples)? {
        if metadata.overflow {
            coordinator.record_overflow(TrackKind::Microphone, metadata.dropped_frames.max(1))?;
            continue;
        }
        if let Some(failure) = metadata.runtime_failure {
            if failure == koe_audio::RuntimeFailure::BufferOverflow {
                coordinator.record_overflow(TrackKind::Microphone, 1)?;
                continue;
            }
            persist_runtime_failure(&coordinator, failure)?;
            return Err(failure.audio_error().into());
        }
        let timeline_ns = microphone_timeline.map(
            metadata.capture_timestamp_ns,
            metadata
                .callback_arrival_timestamp_ns
                .saturating_sub(session_process_origin_ns),
            metadata.discontinuity,
        );
        if metadata.discontinuity {
            microphone_epoch_id = microphone_epoch_id.saturating_add(1);
            coordinator.record_discontinuity(timeline_ns)?;
        }
        let count = usize::try_from(metadata.sample_count)
            .map_err(|_| CliError::Audio(AudioError::UnsupportedFormat))?;
        if count != 0 {
            coordinator.append_block(
                samples[..count].to_vec(),
                stored_timeline_block(metadata, timeline_ns, microphone_epoch_id)?,
            )?;
            if system_stream.is_some() || asr.is_some() {
                normalize_for_mix(
                    &samples[..count],
                    metadata,
                    &mut microphone_normalizer,
                    &mut microphone_drift,
                    timeline_ns,
                    &mut microphone_mix,
                    &coordinator,
                    TrackKind::Microphone,
                )?;
            }
        }
    }
    if let Some(system_consumer) = &system_consumer {
        process_async_control(system_consumer, &coordinator, TrackKind::System)?;
        while let Some(metadata) = system_consumer.try_pop(&mut system_samples)? {
            if metadata.overflow {
                coordinator.record_overflow(TrackKind::System, metadata.dropped_frames.max(1))?;
                continue;
            }
            if let Some(failure) = metadata.runtime_failure {
                if failure == koe_audio::RuntimeFailure::BufferOverflow {
                    coordinator.record_overflow(TrackKind::System, 1)?;
                    continue;
                }
                persist_runtime_failure(&coordinator, failure)?;
                return Err(failure.audio_error().into());
            }
            let timeline_ns = system_timeline.map(
                metadata.capture_timestamp_ns,
                metadata
                    .callback_arrival_timestamp_ns
                    .saturating_sub(session_process_origin_ns),
                metadata.discontinuity,
            );
            if metadata.discontinuity {
                system_epoch_id = system_epoch_id.saturating_add(1);
            }
            let count = usize::try_from(metadata.sample_count)
                .map_err(|_| CliError::Audio(AudioError::UnsupportedFormat))?;
            if count != 0 {
                coordinator.append_track_block(
                    TrackKind::System,
                    system_samples[..count].to_vec(),
                    stored_timeline_block(metadata, timeline_ns, system_epoch_id)?,
                )?;
                normalize_for_mix(
                    &system_samples[..count],
                    metadata,
                    &mut system_normalizer,
                    &mut system_drift,
                    timeline_ns,
                    &mut system_mix,
                    &coordinator,
                    TrackKind::System,
                )?;
            }
        }
        write_available_mix(
            &mut microphone_mix,
            &mut system_mix,
            &coordinator,
            true,
            true,
            asr.as_ref(),
        )?;
        let dropped = system_consumer.take_dropped_frames();
        if dropped != 0 {
            coordinator.record_overflow(TrackKind::System, dropped)?;
        }
    }
    let dropped = consumer.take_dropped_frames();
    if dropped != 0 {
        coordinator.record_overflow(TrackKind::Microphone, dropped)?;
    }
    let ended_ns = elapsed_ns(session_clock);
    if let Some(start_ns) = microphone_loss_started {
        record_device_loss_gap(&coordinator, TrackKind::Microphone, start_ns, ended_ns)?;
    }
    if let Some(start_ns) = system_loss_started {
        record_device_loss_gap(&coordinator, TrackKind::System, start_ns, ended_ns)?;
    }
    let terminal = if interrupts.load(Ordering::Relaxed) >= 2 {
        coordinator.cancel()?
    } else {
        coordinator.stop()?
    };
    task.shutdown(&coordinator)?;
    if let Some(asr) = asr.take() {
        // Drains remaining chunks, runs the model finalization and
        // materializes `events.jsonl` -> `final.json`/`final.txt`.
        asr.finish()?;
    }
    render(&terminal, format, output, || {
        format!(
            "session: {}\nstate: {:?}",
            recording
                .session_id
                .map_or_else(|| "unknown".to_owned(), |id| id.to_string()),
            terminal.state
        )
    })
}

fn reopen_source<B: AudioBackend>(
    backend: &B,
    request: &OpenSource,
    expected_format: koe_audio::NativeSampleFormat,
    queue_capacity: usize,
) -> Option<(B::Stream, FrameConsumer)> {
    for attempt in 0..3_u64 {
        if attempt != 0 {
            thread::sleep(Duration::from_millis(attempt.saturating_mul(25)));
        }
        let Ok(mut candidate) = backend.open(request) else {
            continue;
        };
        if candidate.sample_rate() != request.sample_rate
            || candidate.channels() != request.channels
            || candidate.native_sample_format() != expected_format
        {
            continue;
        }
        let Ok((producer, consumer)) = frame_ring(queue_capacity, 16_384) else {
            return None;
        };
        if candidate.start(Box::new(producer)).is_ok() {
            return Some((candidate, consumer));
        }
    }
    None
}

const fn all_requested_sources_active(
    system_requested: bool,
    microphone_active: bool,
    system_active: bool,
) -> bool {
    microphone_active && (!system_requested || system_active)
}

const fn no_capture_source_active(
    microphone_active: bool,
    system_active: bool,
) -> bool {
    !microphone_active && !system_active
}

fn reset_source_pipeline(
    sample_rate: u32,
    channels: u16,
    timeline: &mut TimelineMapper,
    drift: &mut DriftEstimator,
    normalizer: &mut CanonicalNormalizer,
    mix: &mut TimelineTrack,
) -> Result<(), AudioError> {
    *timeline = TimelineMapper::default();
    *drift = DriftEstimator::new(sample_rate)?;
    *normalizer = CanonicalNormalizer::new(sample_rate, channels)?;
    *mix = TimelineTrack::default();
    Ok(())
}

fn elapsed_ns(clock: Instant) -> u64 {
    clock.elapsed().as_nanos().try_into().unwrap_or(u64::MAX)
}

fn record_device_loss_gap(
    coordinator: &RecorderCoordinator,
    source: TrackKind,
    start_ns: u64,
    end_ns: u64,
) -> Result<(), CliError> {
    coordinator.record_gap(AudioGap {
        source,
        start_us: start_ns / 1_000,
        duration_us: end_ns.saturating_sub(start_ns) / 1_000,
        reason: "device-lost".to_owned(),
    })?;
    Ok(())
}

fn persist_runtime_failure(
    coordinator: &RecorderCoordinator,
    failure: koe_audio::RuntimeFailure,
) -> Result<(), CliError> {
    let _failed = coordinator.fail(failure.audio_error().code())?;
    Ok(())
}

fn process_async_control(
    consumer: &FrameConsumer,
    coordinator: &RecorderCoordinator,
    source: TrackKind,
) -> Result<(), CliError> {
    let Some(failure) = consumer.take_runtime_failure() else {
        return Ok(());
    };
    match failure {
        koe_audio::RuntimeFailure::BufferOverflow => {
            coordinator.record_overflow(source, 1)?;
            Ok(())
        },
        koe_audio::RuntimeFailure::DeviceLost => Ok(()),
        _ => {
            persist_runtime_failure(coordinator, failure)?;
            Err(failure.audio_error().into())
        },
    }
}

fn stored_timeline_block(
    metadata: koe_audio::FrameMetadata,
    session_timeline_ns: u64,
    capture_epoch_id: u64,
) -> Result<TimelineBlock, CliError> {
    let channels = u64::from(metadata.channels);
    if channels == 0 || !u64::from(metadata.sample_count).is_multiple_of(channels) {
        return Err(AudioError::UnsupportedFormat.into());
    }
    Ok(TimelineBlock {
        session_start_us: session_timeline_ns / 1_000,
        capture_epoch_id,
        source_capture_start_ns: metadata.capture_timestamp_ns,
        callback_arrival_ns: metadata.callback_arrival_timestamp_ns,
        sequence: metadata.sequence,
        frame_count: u64::from(metadata.sample_count) / channels,
        discontinuity_before: metadata.discontinuity,
    })
}

#[allow(clippy::cast_possible_truncation, clippy::too_many_arguments)]
fn normalize_for_mix(
    samples: &[i16],
    metadata: koe_audio::FrameMetadata,
    normalizer: &mut CanonicalNormalizer,
    drift: &mut DriftEstimator,
    timeline_ns: u64,
    queue: &mut TimelineTrack,
    coordinator: &RecorderCoordinator,
    source: TrackKind,
) -> Result<(), CliError> {
    let channels = usize::from(metadata.channels);
    if channels == 0 {
        return Err(AudioError::UnsupportedFormat.into());
    }
    let frames = samples.len() / channels;
    let ppm = drift.observe(
        metadata.capture_timestamp_ns,
        u64::try_from(frames).map_err(|_| AudioError::UnsupportedFormat)?,
        metadata.discontinuity,
    );
    normalizer.set_drift_ppm(ppm);
    if metadata.sequence.is_multiple_of(100) {
        coordinator.record_drift_correction(DriftCorrection {
            source,
            timeline_us: timeline_ns / 1_000,
            ppm: ppm.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32,
        })?;
    }
    // -2000 ppm produces the greatest number of output samples.
    let maximum = frames
        .saturating_mul(16_000)
        .saturating_mul(1_000_000)
        .checked_div((metadata.sample_rate as usize).saturating_mul(998_000))
        .unwrap_or(usize::MAX)
        .saturating_add(3);
    let mut canonical = vec![0_i16; maximum];
    let count = normalizer.process(samples, &mut canonical)?;
    if let Some((start_ns, duration_ns)) = queue.push(timeline_ns, &canonical[..count]) {
        coordinator.record_gap(AudioGap {
            source,
            start_us: start_ns / 1_000,
            duration_us: duration_ns / 1_000,
            reason: "capture-gap".to_owned(),
        })?;
    }
    if queue.samples.len() > MIX_JITTER_CAPACITY {
        let dropped = queue.samples.len() - MIX_JITTER_CAPACITY;
        let start_ns = queue
            .start_sample
            .unwrap_or_default()
            .saturating_mul(1_000_000_000)
            / CANONICAL_SAMPLE_RATE;
        queue.consume_before(
            queue
                .start_sample
                .unwrap_or_default()
                .saturating_add(dropped as u64),
        );
        let duration_ns = u64::try_from(dropped)
            .map_err(|_| AudioError::UnsupportedFormat)?
            .saturating_mul(1_000_000_000)
            / 16_000;
        coordinator.record_gap(AudioGap {
            source,
            start_us: start_ns / 1_000,
            duration_us: duration_ns / 1_000,
            reason: "jitter-buffer-overflow".to_owned(),
        })?;
    }
    Ok(())
}

/// Bounded feed bridge from the sync capture loop into the async ASR session.
///
/// The worker thread owns a current-thread tokio runtime and the transcript
/// store, so the capture loop never blocks on the model runtime.
enum AsrCommand {
    Chunk { samples: Vec<i16>, start_us: u64 },
    Stop,
}

struct AsrBridge {
    sender: std::sync::mpsc::SyncSender<AsrCommand>,
    worker: Option<std::thread::JoinHandle<Result<(), CliError>>>,
}

impl AsrBridge {
    fn spawn(
        session: Box<dyn koe_model::StreamingAsrSession>,
        model: TranscriptModel,
        directory: PathBuf,
    ) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(64);
        let worker = std::thread::spawn(move || asr_worker(session, &model, &directory, receiver));
        Self {
            sender,
            worker: Some(worker),
        }
    }

    fn feed(
        &self,
        samples: Vec<i16>,
        start_us: u64,
    ) -> Result<(), CliError> {
        self.sender
            .send(AsrCommand::Chunk { samples, start_us })
            .map_err(|_| CliError::Model(koe_model::ModelError::Internal))
    }

    /// Stops the feed and waits for the materialized transcript.
    fn finish(mut self) -> Result<(), CliError> {
        self.sender
            .send(AsrCommand::Stop)
            .map_err(|_| CliError::Model(koe_model::ModelError::Internal))?;
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| CliError::Model(koe_model::ModelError::Internal))?
        } else {
            Ok(())
        }
    }
}

fn asr_worker(
    session: Box<dyn koe_model::StreamingAsrSession>,
    model: &TranscriptModel,
    directory: &Path,
    receiver: std::sync::mpsc::Receiver<AsrCommand>,
) -> Result<(), CliError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| CliError::Model(koe_model::ModelError::Internal))?;
    let mut session = session;
    let mut store = TranscriptStore::open(directory)?;
    let mut feed = |event: &koe_model::AsrEvent| -> Result<(), CliError> {
        append_asr_event(&mut store, model, event)
    };
    for command in receiver {
        match command {
            AsrCommand::Chunk { samples, start_us } => {
                runtime.block_on(session.append(koe_model::Pcm16Mono16k {
                    samples,
                    session_start_us: start_us,
                }))?;
                while let Some(event) = runtime.block_on(session.poll_results())? {
                    feed(&event)?;
                }
            },
            AsrCommand::Stop => break,
        }
    }
    while let Some(event) = runtime.block_on(session.poll_results())? {
        feed(&event)?;
    }
    let transcript = runtime.block_on(session.finish())?;
    for event in transcript.events {
        feed(&event)?;
    }
    let report = store.finalize()?;
    eprintln!(
        "transcript materialized: {} segment(s) -> {}",
        report.segment_count,
        report.json_path.display(),
    );
    Ok(())
}

fn append_asr_event(
    store: &mut TranscriptStore,
    model: &TranscriptModel,
    event: &koe_model::AsrEvent,
) -> Result<(), CliError> {
    if event.text.is_empty() {
        return Ok(());
    }
    store.append(TranscriptSegment {
        schema_version: 1,
        segment_id: SegmentId::new(),
        source: "mixed".to_owned(),
        start_ms: event.start_us / 1_000,
        end_ms: event.end_us / 1_000,
        text: event.text.clone(),
        is_final: event.is_final,
        model: Some(model.clone()),
        audio_discontinuities: Vec::new(),
    })?;
    store.checkpoint().map_err(CliError::from)
}

fn write_available_mix(
    microphone: &mut TimelineTrack,
    system: &mut TimelineTrack,
    coordinator: &RecorderCoordinator,
    microphone_ended: bool,
    system_ended: bool,
    asr: Option<&AsrBridge>,
) -> Result<(), CliError> {
    while let Some((mixed, start_us)) =
        take_available_mix(microphone, system, microphone_ended, system_ended)
    {
        coordinator.append_track(TrackKind::Mix, mixed.clone())?;
        if let Some(asr) = asr {
            asr.feed(mixed, start_us)?;
        }
    }
    Ok(())
}

fn take_available_mix(
    microphone: &mut TimelineTrack,
    system: &mut TimelineTrack,
    microphone_ended: bool,
    system_ended: bool,
) -> Option<(Vec<i16>, u64)> {
    let mut cursor = match (microphone.start_sample, system.start_sample) {
        (Some(microphone), Some(system)) => microphone.min(system),
        (Some(microphone), None) if system_ended => microphone,
        (None, Some(system)) if microphone_ended => system,
        _ => return None,
    };
    let horizon = match (microphone.end_sample(), system.end_sample()) {
        (Some(microphone), Some(system)) if microphone_ended && system_ended => {
            microphone.max(system)
        },
        (Some(microphone), Some(system)) => microphone.min(system),
        (Some(microphone), None) if system_ended => microphone,
        (None, Some(system)) if microphone_ended => system,
        _ => return None,
    };
    if horizon <= cursor {
        return None;
    }
    let count = usize::try_from(horizon - cursor)
        .unwrap_or(usize::MAX)
        .min(16_384);
    let microphone_chunk = (0..count)
        .map(|offset| microphone.sample_at(cursor.saturating_add(offset as u64)))
        .collect::<Vec<_>>();
    let system_chunk = (0..count)
        .map(|offset| system.sample_at(cursor.saturating_add(offset as u64)))
        .collect::<Vec<_>>();
    let mut mixed = vec![0_i16; count];
    let produced = mix_canonical(&microphone_chunk, &system_chunk, &mut mixed);
    let start_us = cursor.saturating_mul(1_000_000) / CANONICAL_SAMPLE_RATE;
    cursor = cursor.saturating_add(count as u64);
    microphone.consume_before(cursor);
    system.consume_before(cursor);
    Some((mixed[..produced].to_vec(), start_us))
}

fn report_recovered_sessions(data_root: &Path) -> Result<(), CliError> {
    let recovered = recover_sessions(data_root)?;
    for manifest in recovered {
        eprintln!(
            "recovered partial session {} ({})",
            manifest.session_id,
            manifest.failure_code.as_deref().unwrap_or("recovered")
        );
    }
    Ok(())
}

fn render<T: Serialize + ?Sized>(
    value: &T,
    format: OutputFormat,
    output: &mut impl io::Write,
    human: impl FnOnce() -> String,
) -> Result<(), CliError> {
    match format {
        OutputFormat::Human => writeln!(output, "{}", human())?,
        OutputFormat::Json => serde_json::to_writer_pretty(&mut *output, value)?,
        OutputFormat::Jsonl => serde_json::to_writer(&mut *output, value)?,
    }
    if !matches!(format, OutputFormat::Human) {
        writeln!(output)?;
    }
    Ok(())
}

fn terminal_safe(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() || is_default_ignorable(character) {
            use std::fmt::Write as _;
            if character.is_ascii() {
                let _ignored = write!(escaped, "\\x{:02X}", character as u32);
            } else {
                let _ignored = write!(escaped, "\\u{{{:X}}}", character as u32);
            }
        } else {
            escaped.push(character);
        }
    }
    escaped
}

const fn is_default_ignorable(character: char) -> bool {
    matches!(
        character as u32,
        0x00AD
            | 0x034F
            | 0x061C
            | 0x115F..=0x1160
            | 0x17B4..=0x17B5
            | 0x180B..=0x180F
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x206F
            | 0x3164
            | 0xFE00..=0xFE0F
            | 0xFEFF
            | 0xFFA0
            | 0xFFF0..=0xFFF8
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0000..=0xE0FFF
    )
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("{0}")]
    Audio(#[from] AudioError),
    #[error("output failed")]
    Io(#[from] io::Error),
    #[error("output serialization failed")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    App(#[from] AppError),
    #[error("{0}")]
    Recording(#[from] RecordingError),
    #[error("{0}")]
    Model(#[from] koe_model::ModelError),
    #[error("{0}")]
    Transcript(#[from] koe_transcript::TranscriptError),
    #[error("{0}")]
    Asr(#[from] koe_model::AsrError),
    #[error("failed to install Ctrl-C handler")]
    Signal,
    #[error(
        "fresh recording consent is required; review the sources and destination, then pass --consent"
    )]
    ConsentRequired,
}

impl CliError {
    const fn code(&self) -> &'static str {
        match self {
            Self::Audio(error) => error.code(),
            Self::App(error) => error.code(),
            Self::Recording(error) => error.code(),
            Self::Model(error) => error.code(),
            Self::Transcript(error) => error.code(),
            Self::Asr(error) => error.code(),
            Self::Io(_) | Self::Json(_) => "KOE-OUTPUT-FAILED",
            Self::Signal => "KOE-SIGNAL-HANDLER-FAILED",
            Self::ConsentRequired => "KOE-POLICY-CONSENT-REQUIRED",
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use koe_audio::{CanonicalNormalizer, DriftEstimator, UnsupportedBackend};
    use koe_core::SessionState;
    use koe_recording::{RecordingConfig, SessionRecorder};
    use tempfile::TempDir;

    use super::{
        Cli, OutputFormat, TimelineMapper, TimelineTrack, all_requested_sources_active, execute,
        no_capture_source_active, render_collection, report_recovered_sessions,
        reset_source_pipeline, take_available_mix, terminal_safe,
    };

    #[test]
    fn json_capabilities_are_machine_readable() {
        let cli = Cli::try_parse_from(["koe", "--output-format", "json", "capabilities"])
            .unwrap_or_else(|error| panic!("{error}"));
        let mut output = Vec::new();
        execute(&cli, &UnsupportedBackend, &mut output).unwrap_or_else(|error| panic!("{error}"));
        let value: serde_json::Value =
            serde_json::from_slice(&output).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(value.as_array().map(Vec::len), Some(2));
        assert_eq!(value[0]["state"], "unsupported");
    }

    #[test]
    fn doctor_does_not_claim_network_access() {
        let cli = Cli::try_parse_from(["koe", "--output-format", "json", "doctor"])
            .unwrap_or_else(|error| panic!("{error}"));
        let mut output = Vec::new();
        execute(&cli, &UnsupportedBackend, &mut output).unwrap_or_else(|error| panic!("{error}"));
        let value: serde_json::Value =
            serde_json::from_slice(&output).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(value["network_accessed"], false);
    }

    #[test]
    fn permission_status_is_machine_readable_per_source() {
        let cli = Cli::try_parse_from(["koe", "--output-format", "json", "permissions", "status"])
            .unwrap_or_else(|error| panic!("{error}"));
        let mut output = Vec::new();
        execute(&cli, &UnsupportedBackend, &mut output).unwrap_or_else(|error| panic!("{error}"));
        let value: serde_json::Value =
            serde_json::from_slice(&output).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(value.as_array().map(Vec::len), Some(2));
        assert_eq!(value[1]["source"], "system");
    }

    #[test]
    fn record_requires_fresh_explicit_consent_before_opening_audio() {
        let root = TempDir::new().expect("temp");
        let cli = Cli::try_parse_from([
            "koe",
            "record",
            "--mic",
            "fixture",
            "--model",
            "none",
            "--output",
            root.path().to_str().expect("UTF-8 path"),
        ])
        .expect("CLI");
        let error = execute(&cli, &UnsupportedBackend, &mut Vec::new()).expect_err("consent");
        assert_eq!(error.code(), "KOE-POLICY-CONSENT-REQUIRED");
    }

    #[test]
    fn timeline_is_anchored_at_callback_arrival_not_queue_drain() {
        let mut mapper = TimelineMapper::default();
        assert_eq!(mapper.map(1_000, 25_000, false), 25_000);
        assert_eq!(mapper.map(2_000, 9_000_000, false), 26_000);
        assert_eq!(mapper.map(50, 40_000, true), 40_000);
    }

    #[test]
    fn microphone_only_reopen_restores_recording_instead_of_cancelling() {
        assert!(all_requested_sources_active(false, true, false));
        assert!(!no_capture_source_active(true, false));
    }

    #[test]
    fn reopen_reanchors_timeline_and_clears_stale_mix_state() {
        let mut mapper = TimelineMapper::default();
        let mut drift = DriftEstimator::new(48_000).expect("drift");
        let mut normalizer = CanonicalNormalizer::new(48_000, 2).expect("normalizer");
        let mut mix = TimelineTrack::default();
        assert_eq!(mapper.map(1_000, 10_000, false), 10_000);
        mix.push(0, &[1, 2, 3]);

        reset_source_pipeline(
            48_000,
            2,
            &mut mapper,
            &mut drift,
            &mut normalizer,
            &mut mix,
        )
        .expect("reset");

        assert_eq!(mapper.map(500, 50_000, false), 50_000);
        assert!(mix.samples.is_empty());
        assert!(mix.start_sample.is_none());
    }

    #[test]
    fn human_terminal_fields_escape_controls() {
        assert_eq!(terminal_safe("device\u{1b}[31m\n"), "device\\x1B[31m\\x0A");
        assert_eq!(
            terminal_safe("safe\u{202E}txt\u{200B}\u{FEFF}"),
            "safe\\u{202E}txt\\u{200B}\\u{FEFF}"
        );
    }

    #[test]
    fn jsonl_emits_one_item_per_line() {
        for (values, expected) in [
            (Vec::<u32>::new(), ""),
            (vec![1], "1\n"),
            (vec![1, 2, 3], "1\n2\n3\n"),
        ] {
            let mut output = Vec::new();
            render_collection(&values, OutputFormat::Jsonl, &mut output, String::new)
                .unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(String::from_utf8_lossy(&output), expected);
        }
    }

    #[test]
    fn timeline_mix_preserves_initial_offset_and_unmatched_tail() {
        let mut microphone = TimelineTrack::default();
        let mut system = TimelineTrack::default();
        microphone.push(0, &[10, 10, 10]);
        system.push(125_000, &[1, 1]);
        let (first, start_us) =
            take_available_mix(&mut microphone, &mut system, true, true).expect("first chunk");
        assert_eq!(start_us, 0);
        assert_eq!(first, vec![10, 10, 11, 1]);
        assert!(take_available_mix(&mut microphone, &mut system, true, true).is_none());
    }

    #[test]
    fn timeline_track_handles_jitter_overlap_and_gap() {
        let mut track = TimelineTrack::default();
        track.push(0, &[1, 2, 3]);
        track.push(125_000, &[9, 4]);
        let gap = track.push(375_000, &[5]).expect("gap");
        assert_eq!(
            track.samples.iter().copied().collect::<Vec<_>>(),
            [1, 2, 3, 4, 0, 0, 5]
        );
        assert_eq!(gap.1, 125_000);
    }

    #[test]
    fn startup_recovery_marks_an_abandoned_checkpoint() {
        let root = TempDir::new().unwrap_or_else(|error| panic!("{error}"));
        let mut config = RecordingConfig::microphone(root.path(), 16_000, 1);
        config.samples_per_segment = 16_000;
        let session_id = {
            let mut recorder =
                SessionRecorder::start(config).unwrap_or_else(|error| panic!("{error}"));
            recorder
                .write_samples(&[1, 2, 3])
                .unwrap_or_else(|error| panic!("{error}"));
            recorder
                .checkpoint()
                .unwrap_or_else(|error| panic!("{error}"));
            recorder.session_id()
        };

        report_recovered_sessions(root.path()).unwrap_or_else(|error| panic!("{error}"));
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                root.path()
                    .join("sessions")
                    .join(session_id.to_string())
                    .join("session.json"),
            )
            .unwrap_or_else(|error| panic!("{error}")),
        )
        .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            manifest["state"],
            serde_json::json!(SessionState::RecoveredPartial)
        );
    }
    #[test]
    fn models_list_installed_is_offline_and_machine_readable() {
        let root = TempDir::new().expect("temp");
        let cli = Cli::try_parse_from([
            "koe",
            "--output-format",
            "json",
            "models",
            "--data-root",
            root.path().to_str().expect("utf8"),
            "list",
            "--installed",
        ])
        .expect("parse");
        let mut output = Vec::new();
        execute(&cli, &UnsupportedBackend, &mut output).expect("offline list");
        let value: serde_json::Value = serde_json::from_slice(&output).expect("json");
        assert_eq!(value, serde_json::json!([]));
    }

    #[test]
    fn models_catalog_without_network_is_refused() {
        let root = TempDir::new().expect("temp");
        let cli = Cli::try_parse_from([
            "koe",
            "models",
            "--data-root",
            root.path().to_str().expect("utf8"),
            "list",
        ])
        .expect("parse");
        let error = execute(&cli, &UnsupportedBackend, &mut Vec::new())
            .expect_err("catalog requires network consent");
        assert_eq!(error.code(), "KOE-MODEL-OFFLINE-MISSING");
    }

    #[test]
    fn models_install_without_network_is_refused() {
        let root = TempDir::new().expect("temp");
        let cli = Cli::try_parse_from([
            "koe",
            "models",
            "--data-root",
            root.path().to_str().expect("utf8"),
            "install",
            "nemotron-3.5-asr-streaming-0.6b",
        ])
        .expect("parse");
        let error = execute(&cli, &UnsupportedBackend, &mut Vec::new())
            .expect_err("install requires network consent");
        assert_eq!(error.code(), "KOE-MODEL-OFFLINE-MISSING");
    }
}
