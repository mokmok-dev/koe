mod config;
mod sessions;

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
    Doctor {
        /// App-owned data root to inspect. Defaults to the current directory.
        #[arg(long)]
        data_root: Option<PathBuf>,
    },
    /// Inspect and manage locally installed ASR models.
    Models {
        /// App-owned data root shared with `record --output`.
        #[arg(long)]
        data_root: PathBuf,
        #[command(subcommand)]
        command: ModelsCommand,
    },
    /// Manage recorded sessions.
    Sessions {
        /// App-owned data root that contains `sessions/`.
        #[arg(long)]
        data_root: PathBuf,
        #[command(subcommand)]
        command: SessionsCommand,
    },
    /// Manage configuration and retention.
    Config {
        /// App-owned data root that contains `config.json`.
        #[arg(long)]
        data_root: PathBuf,
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Record microphone PCM until Ctrl-C.
    Record {
        /// Opaque stable microphone ID from `devices list`.
        #[arg(long)]
        mic: String,
        /// Optional system-audio device ID from `devices list --source system`.
        #[arg(long)]
        system: Option<String>,
        /// Installed model selector, or `none` for audio-only recording.
        #[arg(long)]
        model: String,
        /// Consent to install `--model` when it is not available locally.
        /// This never enables updates or other network fallback.
        #[arg(long)]
        install_model: bool,
        /// Permit network access only for the consented missing-model install.
        #[arg(long)]
        network: bool,
        /// Accept the exact license ID displayed for the resolved model.
        /// Required only when the selected model must be installed.
        #[arg(long, value_name = "LICENSE_ID")]
        accept_model_license: Option<String>,
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

#[derive(Debug, Subcommand)]
enum SessionsCommand {
    /// List recorded sessions.
    List,
    /// Show one session manifest and transcript status.
    Show { session_id: String },
    /// Export a session to a directory.
    Export {
        session_id: String,
        /// Destination directory. Export is created as `<id>-export` below it.
        #[arg(long)]
        destination: PathBuf,
    },
    /// Delete a session. Active sessions are refused.
    Delete { session_id: String },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Show current configuration.
    Show,
    /// Set retention policy. Omit `--days` to keep forever.
    SetRetention {
        #[arg(long)]
        days: Option<u32>,
    },
    /// Apply retention policy now and return deleted session IDs.
    ApplyRetention,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    schema_version: u32,
    platform: &'static str,
    network_accessed: bool,
    audio_backend: String,
    microphone_state: String,
    system_audio_state: String,
    data_root_writable: bool,
    config_valid: bool,
    session_count: usize,
    active_session_count: usize,
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
    execute_with_model_manager(cli, backend, output, None)
}

fn execute_with_model_manager(
    cli: &Cli,
    backend: &impl AudioBackend,
    output: &mut impl io::Write,
    record_model_manager: Option<&KoeModelManager>,
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
        Command::Doctor { data_root } => {
            run_doctor_command(backend, data_root.as_deref(), cli.output_format, output)?;
        },
        Command::Models { data_root, command } => {
            run_models_command(data_root, command, cli.output_format, output)?;
        },
        Command::Sessions { data_root, command } => {
            run_sessions_command(data_root, command, cli.output_format, output)?;
        },
        Command::Config { data_root, command } => {
            run_config_command(data_root, command, cli.output_format, output)?;
        },
        Command::Record {
            mic,
            system,
            model,
            install_model,
            network,
            accept_model_license,
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
                *install_model,
                *network,
                accept_model_license.as_deref(),
                data_root,
                *sample_rate,
                *channels,
                *consent,
                cli.output_format,
                output,
                record_model_manager,
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
            let selector = selector.parse::<koe_model::ModelSelector>()?;
            let installed = run_blocking(install_model_with_progress(
                &manager,
                &selector,
                *force,
                format,
                tokio_util::sync::CancellationToken::new(),
                None,
            ))?;
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

fn run_doctor_command(
    backend: &impl AudioBackend,
    data_root: Option<&Path>,
    format: OutputFormat,
    output: &mut impl io::Write,
) -> Result<(), CliError> {
    let capabilities = backend.capabilities()?;
    let microphone = capabilities
        .iter()
        .find(|capability| capability.source == SourceKind::Microphone);
    let system_audio = capabilities
        .iter()
        .find(|capability| capability.source == SourceKind::System);
    let microphone_state = microphone.map_or_else(
        || "unknown".to_owned(),
        |capability| format!("{:?}", capability.state),
    );
    let system_audio_state = system_audio.map_or_else(
        || "unknown".to_owned(),
        |capability| format!("{:?}", capability.state),
    );
    let status =
        if microphone.is_some_and(|capability| capability.state == CapabilityState::Supported) {
            "ok"
        } else {
            "degraded"
        };
    let audio_backend = microphone
        .map_or("unknown", |capability| capability.backend.as_str())
        .to_owned();

    let data_root = match data_root {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().map_err(CliError::Io)?,
    };
    let (data_root_writable, config_valid, session_count, active_session_count) =
        if data_root.exists() {
            let writable = data_root
                .metadata()
                .is_ok_and(|metadata| !metadata.permissions().readonly());
            let config_valid = config::load_or_migrate(&data_root).is_ok();
            let sessions = sessions::list_sessions(&data_root).unwrap_or_default();
            let active = sessions
                .iter()
                .filter(|summary| {
                    !matches!(
                        summary.state.as_str(),
                        "completed" | "cancelled" | "failed" | "recovered_partial"
                    )
                })
                .count();
            (writable, config_valid, sessions.len(), active)
        } else {
            (false, false, 0, 0)
        };

    let report = DoctorReport {
        schema_version: 1,
        platform: std::env::consts::OS,
        network_accessed: false,
        audio_backend,
        microphone_state,
        system_audio_state,
        data_root_writable,
        config_valid,
        session_count,
        active_session_count,
        status,
    };
    render(&report, format, output, || human_doctor_report(&report))
}

fn human_doctor_report(report: &DoctorReport) -> String {
    format!(
        "status: {}\naudio backend: {}\nmicrophone: {}\nsystem audio: {}\ndata root writable: {}\nconfig valid: {}\nsessions: {} ({} active)\nnetwork accessed: no",
        report.status,
        report.audio_backend,
        report.microphone_state,
        report.system_audio_state,
        report.data_root_writable,
        report.config_valid,
        report.session_count,
        report.active_session_count,
    )
}

fn run_sessions_command(
    data_root: &Path,
    command: &SessionsCommand,
    format: OutputFormat,
    output: &mut impl io::Write,
) -> Result<(), CliError> {
    match command {
        SessionsCommand::List => {
            let summaries = sessions::list_sessions(data_root)?;
            render_collection(&summaries, format, output, || {
                if summaries.is_empty() {
                    "No sessions.".to_owned()
                } else {
                    summaries
                        .iter()
                        .map(|summary| {
                            format!(
                                "{}\t{}\t{}\t{}ms\t{} files\ttranscript={}",
                                summary.session_id,
                                summary.state,
                                summary.started_at_ms,
                                summary.duration_ms,
                                summary.audio_files,
                                summary.has_transcript,
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            })
        },
        SessionsCommand::Show { session_id } => {
            let detail = sessions::show_session(data_root, session_id)?;
            render(&detail, format, output, || human_session_detail(&detail))
        },
        SessionsCommand::Export {
            session_id,
            destination,
        } => {
            let path = sessions::export_session(data_root, session_id, destination)?;
            render(&path, format, output, || {
                format!("exported to {}", path.display())
            })
        },
        SessionsCommand::Delete { session_id } => {
            sessions::delete_session(data_root, session_id)?;
            render(&session_id, format, output, || {
                format!("deleted session {session_id}")
            })
        },
    }
}

fn human_session_detail(detail: &sessions::SessionDetail) -> String {
    let manifest = &detail.manifest;
    let transcript = detail.transcript.as_ref().map_or_else(
        || "none".to_owned(),
        |summary| {
            format!(
                "{} segment(s), {} words, final_json={} final_txt={}",
                summary.segment_count,
                summary.final_text_word_count,
                summary.has_final_json,
                summary.has_final_txt,
            )
        },
    );
    let duration_ms = manifest.ended_unix_ms.map_or(0, |ended| {
        u64::try_from(ended.saturating_sub(manifest.started_unix_ms)).unwrap_or(u64::MAX)
    });
    format!(
        "session: {}\nstate: {}\nstarted: {}\nended: {}\nduration: {}ms\nsource: {}\naudio files: {}\ntranscript: {}\n",
        detail.session_id,
        format!("{:?}", manifest.state).to_lowercase(),
        manifest.started_unix_ms,
        manifest
            .ended_unix_ms
            .map_or_else(|| "n/a".to_owned(), |ms| ms.to_string()),
        duration_ms,
        &manifest.source_device_id,
        manifest.audio_files.len(),
        transcript,
    )
}

fn run_config_command(
    data_root: &Path,
    command: &ConfigCommand,
    format: OutputFormat,
    output: &mut impl io::Write,
) -> Result<(), CliError> {
    match command {
        ConfigCommand::Show => {
            let config = config::load_or_migrate(data_root)?;
            render(&config, format, output, || human_config(&config))
        },
        ConfigCommand::SetRetention { days } => {
            let mut config = config::load_or_migrate(data_root)?;
            config.retention = days.map_or(config::RetentionPolicy::Forever, |days| {
                config::RetentionPolicy::Days(days)
            });
            config::save(data_root, &config)?;
            render(&config, format, output, || human_config(&config))
        },
        ConfigCommand::ApplyRetention => {
            let config = config::load_or_migrate(data_root)?;
            let deleted = config::apply_retention(data_root, &config)?;
            render_collection(&deleted, format, output, || {
                if deleted.is_empty() {
                    "No sessions deleted by retention policy.".to_owned()
                } else {
                    format!(
                        "deleted {} session(s): {}",
                        deleted.len(),
                        deleted
                            .iter()
                            .map(koe_core::SessionId::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            })
        },
    }
}

fn human_config(config: &config::Config) -> String {
    let retention = match config.retention {
        config::RetentionPolicy::Forever => "forever".to_owned(),
        config::RetentionPolicy::Days(days) => format!("{days} days"),
    };
    format!(
        "retention: {}\ndefault microphone: {}\ndefault system audio: {}\ndefault model: {}\noffline policy: {:?}",
        retention,
        config.defaults.microphone_id.as_deref().unwrap_or("none"),
        config.defaults.system_audio_id.as_deref().unwrap_or("none"),
        config.defaults.model_selector.as_deref().unwrap_or("none"),
        config.offline_policy,
    )
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

/// Installs a model under a narrowly scoped policy while reporting progress.
/// The manager itself can remain frozen to [`NetworkPolicy::Denied`], so all
/// operations after this explicit install are offline.
async fn install_model_with_progress(
    manager: &KoeModelManager,
    selector: &koe_model::ModelSelector,
    force_redownload: bool,
    format: OutputFormat,
    cancel: tokio_util::sync::CancellationToken,
    accepted_descriptor: Option<ModelDescriptor>,
) -> Result<koe_model::InstalledModel, CliError> {
    let (progress, mut progress_rx) = tokio::sync::mpsc::channel(8);
    let options = InstallOptions {
        policy: NetworkPolicy::ModelInstallOnly,
        cancel: cancel.clone(),
        progress: Some(progress),
        accepted_descriptor,
        force_redownload,
    };
    let install = manager.install(selector, &options);
    tokio::pin!(install);
    let mut progress_open = true;
    loop {
        tokio::select! {
            result = &mut install => {
                while let Ok(phase) = progress_rx.try_recv() {
                    report_model_progress(format, selector, &phase);
                }
                return result.map_err(CliError::Model);
            }
            phase = progress_rx.recv(), if progress_open => {
                if let Some(phase) = phase {
                    report_model_progress(format, selector, &phase);
                } else {
                    progress_open = false;
                }
            }
        }
    }
}

#[derive(Serialize)]
struct ModelProgressEnvelope<'a> {
    event: &'static str,
    model: &'a str,
    phase: &'static str,
}

fn model_progress_line(
    format: OutputFormat,
    selector: &koe_model::ModelSelector,
    phase: &koe_model::ModelProgress,
) -> String {
    let phase = match phase {
        koe_model::ModelProgress::Resolving => "resolving",
        koe_model::ModelProgress::Downloading => "downloading",
        koe_model::ModelProgress::Verifying => "verifying",
        koe_model::ModelProgress::Installing => "installing",
        koe_model::ModelProgress::Done => "done",
    };
    let model = selector.key();
    if matches!(format, OutputFormat::Human) {
        format!("model {}: {phase}", terminal_safe(&model))
    } else {
        serde_json::to_string(&ModelProgressEnvelope {
            event: "model_install_progress",
            model: &model,
            phase,
        })
        .unwrap_or_else(|_| {
            "{\"event\":\"model_install_progress\",\"phase\":\"serialization_failed\"}".to_owned()
        })
    }
}

fn report_model_progress(
    format: OutputFormat,
    selector: &koe_model::ModelSelector,
    phase: &koe_model::ModelProgress,
) {
    emit_stderr(&model_progress_line(format, selector, phase));
}

#[derive(Serialize)]
struct ModelCandidateEnvelope<'a> {
    event: &'static str,
    model_id: &'a str,
    alias: &'a str,
    version: &'a str,
    variant: &'a str,
    provider: &'a str,
    license_id: &'a str,
    license_description: &'a str,
    source: &'a str,
    size_mb: u64,
    verification: &'static str,
}

fn report_model_descriptor(
    format: OutputFormat,
    event: &'static str,
    descriptor: &ModelDescriptor,
) {
    if matches!(format, OutputFormat::Human) {
        emit_stderr(&format!(
            "model: {} ({}) version={} variant={} provider={} size={} MiB license={} ({}) source={} verification=runtime-only; accept with --accept-model-license {}",
            terminal_safe(&descriptor.alias.0),
            terminal_safe(&descriptor.id.0),
            terminal_safe(&descriptor.version.0),
            terminal_safe(&descriptor.variant),
            terminal_safe(&descriptor.provider),
            descriptor.size_mb,
            terminal_safe(&descriptor.license_id),
            terminal_safe(&descriptor.license_description),
            terminal_safe(&descriptor.source),
            terminal_safe(&descriptor.license_id),
        ));
    } else {
        let envelope = ModelCandidateEnvelope {
            event,
            model_id: &descriptor.id.0,
            alias: &descriptor.alias.0,
            version: &descriptor.version.0,
            variant: &descriptor.variant,
            provider: &descriptor.provider,
            license_id: &descriptor.license_id,
            license_description: &descriptor.license_description,
            source: &descriptor.source,
            size_mb: descriptor.size_mb,
            verification: "runtime-only",
        };
        emit_stderr(
            &serde_json::to_string(&envelope).unwrap_or_else(|_| {
                "{\"event\":\"model_metadata_serialization_failed\"}".to_owned()
            }),
        );
    }
}

#[derive(Serialize)]
struct InstalledModelEnvelope<'a> {
    event: &'static str,
    model_id: &'a str,
    alias: &'a str,
    version: &'a str,
    variant: &'a str,
    provider: &'a str,
    license_id: &'a str,
    license_description: &'a str,
    source: &'a str,
    size_bytes: u64,
    verification: &'static str,
}

fn report_installed_model(
    format: OutputFormat,
    installed: &koe_model::InstalledModel,
) {
    let descriptor = &installed.descriptor;
    let size_bytes = installed
        .manifest
        .files
        .iter()
        .fold(0_u64, |total, file| total.saturating_add(file.size));
    let verification = match installed.manifest.verification {
        koe_model::Verification::Verified => "verified",
        koe_model::Verification::RuntimeOnly => "runtime-only",
        koe_model::Verification::Quarantined => "quarantined",
    };
    if matches!(format, OutputFormat::Human) {
        emit_stderr(&format!(
            "model: {} ({}) version={} variant={} provider={} size={} bytes license={} ({}) source={} verification={}",
            terminal_safe(&descriptor.alias.0),
            terminal_safe(&descriptor.id.0),
            terminal_safe(&descriptor.version.0),
            terminal_safe(&descriptor.variant),
            terminal_safe(&descriptor.provider),
            size_bytes,
            terminal_safe(&descriptor.license_id),
            terminal_safe(&descriptor.license_description),
            terminal_safe(&descriptor.source),
            verification,
        ));
    } else {
        let envelope = InstalledModelEnvelope {
            event: "model_selected",
            model_id: &descriptor.id.0,
            alias: &descriptor.alias.0,
            version: &descriptor.version.0,
            variant: &descriptor.variant,
            provider: &descriptor.provider,
            license_id: &descriptor.license_id,
            license_description: &descriptor.license_description,
            source: &descriptor.source,
            size_bytes,
            verification,
        };
        emit_stderr(
            &serde_json::to_string(&envelope).unwrap_or_else(|_| {
                "{\"event\":\"model_metadata_serialization_failed\"}".to_owned()
            }),
        );
    }
}

#[derive(Serialize)]
struct DiagnosticEnvelope<'a> {
    event: &'static str,
    message: &'a str,
}

fn diagnostic_line(
    format: OutputFormat,
    event: &'static str,
    message: &str,
) -> String {
    if matches!(format, OutputFormat::Human) {
        message.to_owned()
    } else {
        serde_json::to_string(&DiagnosticEnvelope { event, message })
            .unwrap_or_else(|_| "{\"event\":\"diagnostic_serialization_failed\"}".to_owned())
    }
}

fn report_diagnostic(
    format: OutputFormat,
    event: &'static str,
    message: &str,
) {
    emit_stderr(&diagnostic_line(format, event, message));
}

#[cfg(not(test))]
fn emit_stderr(line: &str) {
    eprintln!("{line}");
}

#[cfg(test)]
thread_local! {
    static STDERR_CAPTURE: std::cell::RefCell<Option<Vec<String>>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
fn emit_stderr(line: &str) {
    let captured = STDERR_CAPTURE.with(|capture| {
        let mut capture = capture.borrow_mut();
        capture.as_mut().is_some_and(|lines| {
            lines.push(line.to_owned());
            true
        })
    });
    if !captured {
        eprintln!("{line}");
    }
}

#[cfg(test)]
fn capture_stderr<T>(operation: impl FnOnce() -> T) -> (T, Vec<String>) {
    STDERR_CAPTURE.with(|capture| {
        assert!(capture.borrow().is_none(), "nested stderr capture");
        *capture.borrow_mut() = Some(Vec::new());
    });
    let result = operation();
    let lines = STDERR_CAPTURE.with(|capture| capture.borrow_mut().take().unwrap_or_default());
    (result, lines)
}

/// Prepares ASR before any audio stream is opened. A missing model is only
/// installed when both install consent and the narrow network permission are
/// present. The manager remains offline for load and inference.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn prepare_asr(
    data_root: &PathBuf,
    model: &str,
    install_missing: bool,
    network: bool,
    accepted_license: Option<&str>,
    format: OutputFormat,
    cancel: &tokio_util::sync::CancellationToken,
    manager_override: Option<&KoeModelManager>,
) -> Result<Option<(Box<dyn koe_model::StreamingAsrSession>, TranscriptModel)>, CliError> {
    if model == "none" {
        return Ok(None);
    }
    let owned_manager;
    let manager = if let Some(manager) = manager_override {
        manager
    } else {
        owned_manager = model_manager(data_root, false)?;
        &owned_manager
    };
    prepare_asr_with_manager(
        manager,
        model,
        install_missing,
        network,
        accepted_license,
        format,
        cancel,
    )
    .map(Some)
}

#[allow(clippy::type_complexity)]
fn prepare_asr_with_manager(
    manager: &KoeModelManager,
    model: &str,
    install_missing: bool,
    network: bool,
    accepted_license: Option<&str>,
    format: OutputFormat,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(Box<dyn koe_model::StreamingAsrSession>, TranscriptModel), CliError> {
    let selector = model.parse::<koe_model::ModelSelector>()?;
    let installed_id = match manager.installed_id_for(&selector)? {
        Some(installed_id) => {
            let installed = manager.installed_model(&installed_id)?;
            report_installed_model(format, &installed);
            installed_id
        },
        None if !install_missing => {
            return Err(koe_model::ModelError::OfflineArtifactMissing.into());
        },
        None if !network => return Err(koe_model::ModelError::NetworkDenied.into()),
        None => {
            let descriptor = run_blocking(async {
                manager
                    .resolve_for_install(&selector, NetworkPolicy::ModelInstallOnly, cancel)
                    .await
                    .map_err(CliError::Model)
            })?;
            report_model_descriptor(format, "model_install_candidate", &descriptor);
            if accepted_license != Some(descriptor.license_id.as_str()) {
                return Err(koe_model::ModelError::LicenseNotAccepted.into());
            }
            let installed = run_blocking(install_model_with_progress(
                manager,
                &selector,
                false,
                format,
                cancel.clone(),
                Some(descriptor),
            ))?;
            report_installed_model(format, &installed);
            installed.id
        },
    };
    let settings = AsrSessionSettings::default();
    let loaded =
        run_blocking(async { manager.load(&installed_id).await.map_err(CliError::Model) })?;
    let session = run_blocking(async {
        manager
            .create_asr_session(&installed_id, &settings)
            .await
            .map_err(CliError::Model)
    })?;
    Ok((
        session,
        TranscriptModel {
            id: loaded.descriptor.id.0,
            version: loaded.descriptor.version.0,
            variant: loaded.descriptor.variant,
        },
    ))
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
    install_missing_model: bool,
    network: bool,
    accepted_model_license: Option<&str>,
    data_root: &PathBuf,
    sample_rate: u32,
    channels: u16,
    consent: bool,
    format: OutputFormat,
    output: &mut impl io::Write,
    model_manager_override: Option<&KoeModelManager>,
) -> Result<(), CliError> {
    if !consent {
        return Err(CliError::ConsentRequired);
    }
    let asr_enabled = model != "none";
    let interrupts = Arc::new(AtomicUsize::new(0));
    let install_cancel = tokio_util::sync::CancellationToken::new();
    let interrupt_handler = Arc::clone(&interrupts);
    let handler_cancel = install_cancel.clone();
    if model_manager_override.is_none() {
        ctrlc::set_handler(move || {
            interrupt_handler.fetch_add(1, Ordering::Relaxed);
            handler_cancel.cancel();
        })
        .map_err(|_| CliError::Signal)?;
    }
    // A consented install, model load and session creation all finish before
    // capture. Network permission is scoped to the missing-model install;
    // inference remains under the manager's frozen `Denied` policy.
    let prepared_asr = prepare_asr(
        data_root,
        model,
        install_missing_model,
        network,
        accepted_model_license,
        format,
        &install_cancel,
        model_manager_override,
    )?;
    report_recovered_sessions(data_root, format)?;
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
    let confirmation = if system_id.is_some() {
        format!(
            "confirmed recording: microphone={}, system={}, scope=system-wide, destination={}, retention=until explicitly deleted, model={} ({asr_note}), sharing=none",
            safe_microphone_id,
            terminal_safe(system_id.unwrap_or("none")),
            terminal_safe(&data_root.display().to_string()),
            safe_model,
        )
    } else {
        format!(
            "confirmed recording: microphone={}, system=none, destination={}, retention=until explicitly deleted, model={} ({asr_note}), sharing=none",
            safe_microphone_id,
            terminal_safe(&data_root.display().to_string()),
            safe_model,
        )
    };
    report_diagnostic(format, "recording_confirmed", &confirmation);
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
        Some(AsrBridge::spawn(session, model, transcript_dir, format))
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
    report_diagnostic(
        format,
        "recording_started",
        "recording; press Ctrl-C to stop (press twice to cancel)",
    );

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
        format: OutputFormat,
    ) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(64);
        let worker =
            std::thread::spawn(move || asr_worker(session, &model, &directory, receiver, format));
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
    format: OutputFormat,
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
    report_diagnostic(
        format,
        "transcript_materialized",
        &format!(
            "transcript materialized: {} segment(s)",
            report.segment_count
        ),
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

fn report_recovered_sessions(
    data_root: &Path,
    format: OutputFormat,
) -> Result<(), CliError> {
    let recovered = recover_sessions(data_root)?;
    for manifest in recovered {
        report_diagnostic(
            format,
            "session_recovered",
            &format!(
                "recovered partial session {} ({})",
                manifest.session_id,
                manifest.failure_code.as_deref().unwrap_or("recovered")
            ),
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
    #[error("{0}")]
    Config(#[from] config::ConfigError),
    #[error("{0}")]
    Session(#[from] sessions::SessionError),
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
            Self::Config(error) => error.code(),
            Self::Session(error) => error.code(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use clap::Parser;
    use koe_audio::{
        AudioBackend, AudioCapability, AudioDevice, AudioError, CanonicalNormalizer,
        DriftEstimator, OpenSource, UnsupportedBackend, UnsupportedStream,
    };
    use koe_core::{NetworkPolicy, SessionState, SourceKind};
    use koe_model::{DigestAllowlist, FixtureFoundryAdapter, KoeModelManager};
    use koe_recording::{RecordingConfig, SessionRecorder};
    use tempfile::TempDir;

    use super::{
        Cli, OutputFormat, TimelineMapper, TimelineTrack, all_requested_sources_active,
        capture_stderr, execute, execute_with_model_manager, model_progress_line,
        no_capture_source_active, prepare_asr, prepare_asr_with_manager, render_collection,
        report_recovered_sessions, reset_source_pipeline, take_available_mix, terminal_safe,
    };

    #[derive(Default)]
    struct CountingUnsupportedBackend {
        opens: AtomicUsize,
    }

    impl AudioBackend for CountingUnsupportedBackend {
        type Stream = UnsupportedStream;

        fn capabilities(&self) -> Result<Vec<AudioCapability>, AudioError> {
            UnsupportedBackend.capabilities()
        }

        fn permissions(&self) -> Result<Vec<AudioCapability>, AudioError> {
            UnsupportedBackend.permissions()
        }

        fn enumerate(
            &self,
            kind: SourceKind,
        ) -> Result<Vec<AudioDevice>, AudioError> {
            UnsupportedBackend.enumerate(kind)
        }

        fn open(
            &self,
            _request: &OpenSource,
        ) -> Result<Self::Stream, AudioError> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            Err(AudioError::Unsupported)
        }
    }

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
        let root = TempDir::new().expect("temp");
        let cli = Cli::try_parse_from([
            "koe",
            "--output-format",
            "json",
            "doctor",
            "--data-root",
            root.path().to_str().expect("utf8"),
        ])
        .unwrap_or_else(|error| panic!("{error}"));
        let mut output = Vec::new();
        execute(&cli, &UnsupportedBackend, &mut output).unwrap_or_else(|error| panic!("{error}"));
        let value: serde_json::Value =
            serde_json::from_slice(&output).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(value["network_accessed"], false);
        assert_eq!(value["config_valid"], true);
        assert_eq!(value["session_count"], 0);
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
    fn missing_model_is_rejected_before_the_audio_backend_is_opened() {
        let root = TempDir::new().expect("temp");
        let cli = Cli::try_parse_from([
            "koe",
            "record",
            "--mic",
            "fixture",
            "--model",
            "missing-fixture-model",
            "--output",
            root.path().to_str().expect("UTF-8 path"),
            "--consent",
        ])
        .expect("CLI");
        let error = execute(&cli, &UnsupportedBackend, &mut Vec::new()).expect_err("missing model");
        assert!(matches!(
            error,
            super::CliError::Model(koe_model::ModelError::OfflineArtifactMissing)
        ));
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

        report_recovered_sessions(root.path(), OutputFormat::Human)
            .unwrap_or_else(|error| panic!("{error}"));
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
    fn record_model_none_ignores_install_and_network_flags() {
        let root = TempDir::new().expect("temp");
        let cancel = tokio_util::sync::CancellationToken::new();
        assert!(
            prepare_asr(
                &root.path().to_path_buf(),
                "none",
                true,
                true,
                Some("unrelated-license"),
                OutputFormat::Json,
                &cancel,
                None,
            )
            .expect("audio-only")
            .is_none()
        );
        assert!(!root.path().join("models").exists());
    }

    #[test]
    fn record_missing_model_requires_both_install_consent_and_network_permission() {
        let root = TempDir::new().expect("temp");
        let cache = TempDir::new().expect("cache");
        let manager = KoeModelManager::new(
            root.path(),
            DigestAllowlist::empty(),
            Box::new(FixtureFoundryAdapter::new(cache.path())),
            NetworkPolicy::Denied,
        )
        .expect("manager");

        for (install, network, expected) in [
            (false, false, koe_model::ModelError::OfflineArtifactMissing),
            (false, true, koe_model::ModelError::OfflineArtifactMissing),
            (true, false, koe_model::ModelError::NetworkDenied),
        ] {
            let cancel = tokio_util::sync::CancellationToken::new();
            let Err(error) = prepare_asr_with_manager(
                &manager,
                "fixture-nemotron-asr-0.6b",
                install,
                network,
                None,
                OutputFormat::Jsonl,
                &cancel,
            ) else {
                panic!("missing consent or network permission must fail");
            };
            assert!(matches!(&error, super::CliError::Model(actual) if actual == &expected));
            assert_eq!(error.code(), "KOE-MODEL-OFFLINE-MISSING");
            assert!(manager.installed_models().expect("installed").is_empty());
        }
    }

    #[test]
    fn record_missing_model_requires_exact_license_acceptance() {
        let root = TempDir::new().expect("temp");
        let cache = TempDir::new().expect("cache");
        let manager = KoeModelManager::new(
            root.path(),
            DigestAllowlist::empty(),
            Box::new(FixtureFoundryAdapter::new(cache.path())),
            NetworkPolicy::Denied,
        )
        .expect("manager");
        for accepted_license in [None, Some(""), Some("wrong-license")] {
            let cancel = tokio_util::sync::CancellationToken::new();
            let Err(error) = prepare_asr_with_manager(
                &manager,
                "fixture-nemotron-asr-0.6b",
                true,
                true,
                accepted_license,
                OutputFormat::Json,
                &cancel,
            ) else {
                panic!("model-specific license token must be required");
            };
            assert!(matches!(
                error,
                super::CliError::Model(koe_model::ModelError::LicenseNotAccepted)
            ));
            assert_eq!(error.code(), "KOE-MODEL-LICENSE-NOT-ACCEPTED");
            assert!(manager.installed_models().expect("installed").is_empty());
        }
    }

    #[test]
    fn record_can_install_missing_model_then_prepare_offline_asr() {
        let root = TempDir::new().expect("temp");
        let cache = TempDir::new().expect("cache");
        let manager = KoeModelManager::new(
            root.path(),
            DigestAllowlist::empty(),
            Box::new(FixtureFoundryAdapter::new(cache.path())),
            NetworkPolicy::Denied,
        )
        .expect("manager");

        let cancel = tokio_util::sync::CancellationToken::new();
        let (_session, transcript_model) = prepare_asr_with_manager(
            &manager,
            "fixture-nemotron-asr-0.6b",
            true,
            true,
            Some("fixture-license-apache-2.0"),
            OutputFormat::Jsonl,
            &cancel,
        )
        .expect("consented install and ASR preparation");

        assert_eq!(manager.policy(), NetworkPolicy::Denied);
        assert_eq!(transcript_model.id, "FixtureLocal/NemotronASRStreaming0.6B");
        assert_eq!(
            manager.installed_models().expect("installed models").len(),
            1
        );

        // Once installed, both permissions and license acceptance can be
        // absent: the second preparation is entirely local.
        let (_session, repeated_model) = prepare_asr_with_manager(
            &manager,
            "fixture-nemotron-asr-0.6b",
            false,
            false,
            None,
            OutputFormat::Json,
            &cancel,
        )
        .expect("installed model stays offline");
        assert_eq!(repeated_model.id, transcript_model.id);
        assert_eq!(
            manager.installed_models().expect("installed models").len(),
            1
        );
    }

    #[test]
    fn record_flags_parse_through_the_command_boundary() {
        let cli = Cli::try_parse_from([
            "koe",
            "record",
            "--mic",
            "fixture",
            "--model",
            "fixture-nemotron-asr-0.6b",
            "--install-model",
            "--network",
            "--accept-model-license",
            "fixture-license-apache-2.0",
            "--output",
            "/tmp/koe-fixture",
            "--consent",
        ])
        .expect("record CLI");
        let super::Command::Record {
            install_model,
            network,
            accept_model_license,
            ..
        } = cli.command
        else {
            panic!("record command");
        };
        assert!(install_model && network);
        assert_eq!(
            accept_model_license.as_deref(),
            Some("fixture-license-apache-2.0")
        );
    }

    #[test]
    fn record_command_installs_and_prepares_asr_before_opening_audio() {
        let root = TempDir::new().expect("temp");
        let cache = TempDir::new().expect("cache");
        let manager = KoeModelManager::new(
            root.path(),
            DigestAllowlist::empty(),
            Box::new(FixtureFoundryAdapter::new(cache.path())),
            NetworkPolicy::Denied,
        )
        .expect("manager");
        let cli = Cli::try_parse_from([
            "koe",
            "--output-format",
            "jsonl",
            "record",
            "--mic",
            "fixture",
            "--model",
            "fixture-nemotron-asr-0.6b",
            "--install-model",
            "--network",
            "--accept-model-license",
            "fixture-license-apache-2.0",
            "--output",
            root.path().to_str().expect("UTF-8 root"),
            "--consent",
        ])
        .expect("record CLI");
        let backend = CountingUnsupportedBackend::default();
        let error = execute_with_model_manager(&cli, &backend, &mut Vec::new(), Some(&manager))
            .expect_err("fake backend stops after preparation");
        assert!(
            matches!(error, super::CliError::Audio(AudioError::Unsupported)),
            "unexpected command result: {error:?}"
        );
        assert_eq!(backend.opens.load(Ordering::SeqCst), 1);
        assert_eq!(manager.installed_models().expect("installed").len(), 1);
    }

    #[test]
    fn model_install_progress_is_machine_readable() {
        for format in [OutputFormat::Json, OutputFormat::Jsonl] {
            let root = TempDir::new().expect("temp");
            let cache = TempDir::new().expect("cache");
            let manager = KoeModelManager::new(
                root.path(),
                DigestAllowlist::empty(),
                Box::new(FixtureFoundryAdapter::new(cache.path())),
                NetworkPolicy::Denied,
            )
            .expect("manager");
            let cancel = tokio_util::sync::CancellationToken::new();
            let (result, lines) = capture_stderr(|| {
                prepare_asr_with_manager(
                    &manager,
                    "fixture-nemotron-asr-0.6b",
                    true,
                    true,
                    Some("fixture-license-apache-2.0"),
                    format,
                    &cancel,
                )
            });
            let (_session, _model) = result.expect("prepare ASR");
            let events = lines
                .iter()
                .map(|line| {
                    serde_json::from_str::<serde_json::Value>(line)
                        .unwrap_or_else(|error| panic!("non-JSON stderr line {line:?}: {error}"))
                })
                .collect::<Vec<_>>();
            assert_eq!(
                events
                    .iter()
                    .map(|event| event["event"].as_str().expect("event"))
                    .collect::<Vec<_>>(),
                [
                    "model_install_candidate",
                    "model_install_progress",
                    "model_install_progress",
                    "model_install_progress",
                    "model_install_progress",
                    "model_install_progress",
                    "model_selected",
                ]
            );
            assert_eq!(events[0]["license_id"], "fixture-license-apache-2.0");
            assert_eq!(events[6]["verification"], "runtime-only");
            assert_eq!(
                events[1..6]
                    .iter()
                    .map(|event| event["phase"].as_str().expect("phase"))
                    .collect::<Vec<_>>(),
                [
                    "resolving",
                    "downloading",
                    "verifying",
                    "installing",
                    "done"
                ]
            );
        }

        let unsafe_selector = "model\u{202e}name"
            .parse::<koe_model::ModelSelector>()
            .expect("selector");
        assert_eq!(
            model_progress_line(
                OutputFormat::Human,
                &unsafe_selector,
                &koe_model::ModelProgress::Resolving,
            ),
            "model model\\u{202E}name: resolving"
        );
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

    #[test]
    fn sessions_list_empty_is_machine_readable() {
        let root = TempDir::new().expect("temp");
        fs::create_dir_all(root.path().join("sessions")).expect("sessions");
        let cli = Cli::try_parse_from([
            "koe",
            "--output-format",
            "json",
            "sessions",
            "--data-root",
            root.path().to_str().expect("utf8"),
            "list",
        ])
        .expect("parse");
        let mut output = Vec::new();
        execute(&cli, &UnsupportedBackend, &mut output).expect("execute");
        let value: serde_json::Value = serde_json::from_slice(&output).expect("json");
        assert_eq!(value, serde_json::json!([]));
    }

    #[test]
    fn sessions_show_and_delete_completed_session() {
        let root = TempDir::new().expect("temp");
        let id = koe_core::SessionId::new();
        let session_dir = root.path().join("sessions").join(id.to_string());
        fs::create_dir_all(&session_dir).expect("dir");
        fs::create_dir_all(session_dir.join("audio")).expect("audio");
        fs::create_dir_all(session_dir.join("transcript")).expect("transcript");
        fs::create_dir_all(session_dir.join("recovery")).expect("recovery");
        let manifest = koe_recording::SessionManifest {
            schema_version: 2,
            session_id: id,
            state: koe_core::SessionState::Completed,
            started_unix_ms: 1_000,
            ended_unix_ms: Some(2_000),
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
        };
        fs::write(
            session_dir.join("session.json"),
            serde_json::to_vec(&manifest).expect("json"),
        )
        .expect("manifest");

        let show_cli = Cli::try_parse_from([
            "koe",
            "--output-format",
            "json",
            "sessions",
            "--data-root",
            root.path().to_str().expect("utf8"),
            "show",
            &id.to_string(),
        ])
        .expect("parse");
        let mut output = Vec::new();
        execute(&show_cli, &UnsupportedBackend, &mut output).expect("show");
        let value: serde_json::Value = serde_json::from_slice(&output).expect("json");
        assert_eq!(value["session_id"], id.to_string());

        let delete_cli = Cli::try_parse_from([
            "koe",
            "sessions",
            "--data-root",
            root.path().to_str().expect("utf8"),
            "delete",
            &id.to_string(),
        ])
        .expect("parse");
        execute(&delete_cli, &UnsupportedBackend, &mut Vec::new()).expect("delete");
        assert!(!session_dir.exists());
    }

    #[test]
    fn config_show_and_retention_round_trip() {
        let root = TempDir::new().expect("temp");
        let cli = Cli::try_parse_from([
            "koe",
            "--output-format",
            "json",
            "config",
            "--data-root",
            root.path().to_str().expect("utf8"),
            "set-retention",
            "--days",
            "14",
        ])
        .expect("parse");
        let mut output = Vec::new();
        execute(&cli, &UnsupportedBackend, &mut output).expect("set");
        let value: serde_json::Value = serde_json::from_slice(&output).expect("json");
        assert!(matches!(value["retention"], serde_json::Value::Object(_)));
    }

    #[test]
    fn stdout_contract_does_not_emit_audio_or_transcript_text() {
        let root = TempDir::new().expect("temp");
        let id = koe_core::SessionId::new();
        let session_dir = root.path().join("sessions").join(id.to_string());
        fs::create_dir_all(session_dir.join("transcript")).expect("transcript");
        let manifest = koe_recording::SessionManifest {
            schema_version: 2,
            session_id: id,
            state: koe_core::SessionState::Completed,
            started_unix_ms: 1,
            ended_unix_ms: Some(2),
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
        };
        fs::write(
            session_dir.join("session.json"),
            serde_json::to_vec(&manifest).expect("json"),
        )
        .expect("manifest");
        fs::write(
            session_dir.join("transcript").join("final.txt"),
            "secret transcript text",
        )
        .expect("final");

        let cli = Cli::try_parse_from([
            "koe",
            "--output-format",
            "human",
            "sessions",
            "--data-root",
            root.path().to_str().expect("utf8"),
            "list",
        ])
        .expect("parse");
        let mut output = Vec::new();
        execute(&cli, &UnsupportedBackend, &mut output).expect("execute");
        let text = String::from_utf8_lossy(&output);
        assert!(!text.contains("secret"));
        assert!(!text.contains("transcript text"));
    }
}
