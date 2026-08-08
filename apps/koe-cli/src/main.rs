mod config;
mod sessions;

use std::{
    collections::VecDeque,
    fs,
    io::{self, IsTerminal as _, Write as _},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use clap::{Parser, Subcommand, ValueEnum};
use koe_app::{AppError, RecorderCoordinator, RecordingConsent};
use koe_audio::{
    AudioBackend, AudioCapability, AudioDevice, AudioError, AudioStream, CanonicalNormalizer,
    CpalBackend, DriftEstimator, FrameConsumer, OpenSource, frame_ring, process_timeline_now_ns,
};
use koe_core::{CapabilityState, NetworkPolicy, SessionId, SourceKind};
use koe_model::{
    AsrSessionSettings, DigestAllowlist, FoundryLocalAdapter, InstallOptions, KoeModelManager,
    ModelDescriptor, ModelManager, Verification,
};
use koe_recording::{
    AudioGap, DriftCorrection, RecordingConfig, RecordingError, TimelineBlock, TrackConfig,
    TrackKind, recover_sessions,
};
use koe_transcript::{SegmentId, TranscriptModel, TranscriptSegment, TranscriptStore};
use serde::Serialize;
use unicode_width::UnicodeWidthStr as _;

const MIX_JITTER_CAPACITY: usize = 32_000;
const CANONICAL_SAMPLE_RATE: u64 = 16_000;
const INTERRUPT_EXIT_CODE: i32 = 130;
const INTERRUPT_GRACE_PERIOD: Duration = Duration::from_secs(10);
#[cfg(not(test))]
const SETUP_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
const SETUP_PROBE_TIMEOUT: Duration = Duration::from_millis(100);
const PHASE_PREPARING: u8 = 0;
const PHASE_RECORDING: u8 = 1;

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
#[command(
    name = "koe",
    version,
    about = "Offline, private audio recording with on-device transcription.",
    long_about = "koe records microphone (and optionally system) audio and transcribes it \
                  entirely on your machine. Nothing is uploaded: the network is only ever \
                  used for an explicitly consented model install.\n\n\
                  New here? Try:\n  \
                  koe doctor                 check your setup\n  \
                  koe devices list           see your microphones\n  \
                  koe record --output ./data start a guided recording",
    after_help = "Run `koe <command> --help` for details on any command."
)]
struct Cli {
    #[arg(long, value_enum, default_value_t)]
    output_format: OutputFormat,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Prepare a data root, validate audio access, recover interrupted work,
    /// and save verified defaults. Safe to run again.
    Setup {
        /// App-owned data root to initialize.
        #[arg(long)]
        data_root: PathBuf,
        /// Microphone device ID to save as the default.
        #[arg(long)]
        mic: Option<String>,
        /// Model alias/ID to verify, or `none` for audio-only use.
        #[arg(long, default_value = "none")]
        model: String,
        /// Install the selected model when it is not already available.
        #[arg(long)]
        install_model: bool,
        /// Permit network access only for the requested model install.
        #[arg(long, requires = "install_model")]
        network: bool,
    },
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
    /// Recover interrupted recordings without loading an ASR model.
    Recover {
        /// App-owned data root containing sessions.
        #[arg(long)]
        data_root: PathBuf,
    },
    /// Print the materialized final transcript for a completed session.
    Transcript {
        session_id: String,
        /// App-owned data root containing sessions.
        #[arg(long)]
        data_root: PathBuf,
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
    /// Record audio and transcribe it live, until you press Ctrl-C.
    ///
    /// On an interactive terminal you can omit --mic and --model to pick them
    /// from a menu, and omit --consent to confirm interactively. In
    /// non-interactive or JSON/JSONL mode every selector must be explicit.
    Record {
        /// Microphone device ID from `devices list`. Omit on a terminal to
        /// choose from a menu.
        #[arg(long)]
        mic: Option<String>,
        /// Optional system-audio device ID from `devices list --source system`.
        #[arg(long)]
        system: Option<String>,
        /// Installed model selector, or `none` for audio-only recording. Omit
        /// on a terminal to choose from a menu.
        #[arg(long)]
        model: Option<String>,
        /// Consent to install `--model` when it is not available locally.
        /// This never enables updates or other network fallback.
        #[arg(long)]
        install_model: bool,
        /// Permit network access only for the consented missing-model install.
        #[arg(long)]
        network: bool,
        /// Require the resolved model's displayed license ID to match this value.
        /// The installation remains bound to the exact reported descriptor.
        #[arg(
            long,
            value_name = "LICENSE_ID",
            conflicts_with = "accept_model_license"
        )]
        expect_model_license: Option<String>,
        /// Deprecated compatibility spelling for --expect-model-license.
        #[arg(
            long = "accept-model-license",
            value_name = "LICENSE_ID",
            hide = true,
            conflicts_with = "expect_model_license"
        )]
        accept_model_license: Option<String>,
        /// App-owned data root below which `sessions/<uuid>` is created.
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 48_000)]
        sample_rate: u32,
        #[arg(long, default_value_t = 1)]
        channels: u16,
        /// BCP-47 language hint for the ASR model (e.g. "en", "ja", "auto").
        /// The multilingual model requires this; English-only models ignore it.
        #[arg(long, default_value = "auto")]
        language: String,
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
        /// Force replacement of a cached model when the runtime supports it.
        /// Foundry Local SDK 1.2.3 does not; it returns an explicit error.
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
    /// Preview retention candidates; pass --confirm to delete them.
    ApplyRetention {
        /// Confirm deletion after reviewing the preview.
        #[arg(long)]
        confirm: bool,
    },
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
struct SetupReport {
    schema_version: u32,
    data_root_ready: bool,
    microphone: String,
    permission: String,
    model: String,
    model_verified: bool,
    offline_smoke_test: bool,
    recovered_sessions: usize,
    /// Structured argv is safe for automation and does not rely on shell
    /// quoting conventions.
    next_command_argv: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    code: &'static str,
    message: String,
    remedy: &'static str,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation_id: Option<String>,
    diagnostic_id: String,
}

fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    match execute(&cli, &CpalBackend::default(), &mut io::stdout()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let envelope = ErrorEnvelope {
                code: error.code(),
                message: error.to_string(),
                remedy: error.remedy(),
                retryable: error.retryable(),
                operation_id: None,
                diagnostic_id: uuid::Uuid::new_v4().to_string(),
            };
            match serde_json::to_string(&envelope) {
                Ok(line) => eprintln!("{line}"),
                Err(_) => eprintln!("{{\"code\":\"KOE-INTERNAL\",\"message\":\"error\"}}"),
            }
            ExitCode::FAILURE
        },
    }
}

/// Installs a human-readable tracing subscriber on stderr. stdout stays
/// reserved for rendered command output. The level defaults to `info` and can
/// be overridden with `RUST_LOG`.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .with_ansi(io::stderr().is_terminal())
        .compact()
        .init();
}

const fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Setup { .. } => "setup",
        Command::Capabilities => "capabilities",
        Command::Devices { .. } => "devices",
        Command::Permissions { .. } => "permissions",
        Command::Doctor { .. } => "doctor",
        Command::Recover { .. } => "recover",
        Command::Transcript { .. } => "transcript",
        Command::Models { .. } => "models",
        Command::Sessions { .. } => "sessions",
        Command::Config { .. } => "config",
        Command::Record { .. } => "record",
    }
}

fn execute<B: AudioBackend>(
    cli: &Cli,
    backend: &B,
    output: &mut impl io::Write,
) -> Result<(), CliError>
where
    B::Stream: Send + 'static,
{
    execute_with_model_manager(cli, backend, output, None)
}

fn execute_with_model_manager<B: AudioBackend>(
    cli: &Cli,
    backend: &B,
    output: &mut impl io::Write,
    record_model_manager: Option<&KoeModelManager>,
) -> Result<(), CliError>
where
    B::Stream: Send + 'static,
{
    execute_with_runtime(cli, backend, output, record_model_manager, None)
}

#[cfg(test)]
fn execute_with_model_manager_and_interrupts<B: AudioBackend>(
    cli: &Cli,
    backend: &B,
    output: &mut impl io::Write,
    record_model_manager: &KoeModelManager,
    interrupts: Arc<AtomicUsize>,
) -> Result<(), CliError>
where
    B::Stream: Send + 'static,
{
    execute_with_runtime(
        cli,
        backend,
        output,
        Some(record_model_manager),
        Some(interrupts),
    )
}

#[allow(clippy::too_many_lines)]
fn execute_with_runtime<B: AudioBackend>(
    cli: &Cli,
    backend: &B,
    output: &mut impl io::Write,
    record_model_manager: Option<&KoeModelManager>,
    record_interrupts: Option<Arc<AtomicUsize>>,
) -> Result<(), CliError>
where
    B::Stream: Send + 'static,
{
    tracing::debug!(command = command_name(&cli.command), "dispatching command");
    match &cli.command {
        Command::Setup {
            data_root,
            mic,
            model,
            install_model,
            network,
        } => run_setup_command(
            backend,
            data_root,
            mic.as_deref(),
            model,
            *install_model,
            *network,
            cli.output_format,
            output,
        )?,
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
        Command::Recover { data_root } => {
            let recovered = recover_sessions_and_transcripts(data_root)?;
            render_collection(&recovered, cli.output_format, output, || {
                if recovered.is_empty() {
                    "No interrupted sessions needed recovery.".to_owned()
                } else {
                    format!("recovered {} partial session(s)", recovered.len())
                }
            })?;
        },
        Command::Transcript {
            session_id,
            data_root,
        } => {
            let transcript = sessions::transcript(data_root, session_id)?;
            render_collection(&transcript, cli.output_format, output, || {
                transcript
                    .iter()
                    .map(|segment| terminal_safe(segment.text()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })?;
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
            expect_model_license,
            accept_model_license,
            language,
            output: data_root,
            sample_rate,
            channels,
            consent,
        } => {
            if accept_model_license.is_some() {
                report_diagnostic(
                    cli.output_format,
                    "deprecated_option",
                    "--accept-model-license is deprecated; use --expect-model-license (this checks a license ID and does not record acceptance)",
                );
            }
            let selection = resolve_record_inputs(
                backend,
                data_root,
                mic.as_deref(),
                system.as_deref(),
                model.as_deref(),
                *consent,
                cli.output_format,
            )?;
            record(
                backend,
                &selection.microphone_id,
                selection.system_id.as_deref(),
                &selection.model,
                *install_model,
                *network,
                expect_model_license
                    .as_deref()
                    .or(accept_model_license.as_deref()),
                language,
                data_root,
                *sample_rate,
                *channels,
                selection.consent,
                cli.output_format,
                output,
                record_model_manager,
                record_interrupts,
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

/// Runs a future that returns a Foundry live session with a blocking push loop.
fn run_blocking_with_live_tasks<F, T>(future: F) -> Result<T, CliError>
where
    F: std::future::Future<Output = Result<T, CliError>>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| CliError::Model(koe_model::ModelError::Internal))?;
    let result = runtime.block_on(future);
    // A normal Runtime drop waits for spawn_blocking tasks, but Foundry's push
    // loop cannot finish until recording later stops the returned session.
    // Keep that loop alive; the ASR worker subsequently stops and joins it.
    runtime.shutdown_background();
    result
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
            if *force
                && !run_blocking(async { Ok(manager.supports_cached_force_redownload().await) })?
            {
                return Err(CliError::Model(
                    koe_model::ModelError::ForceRedownloadUnsupported,
                ));
            }
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

/// Idempotently prepares the local-only recording environment. Model network
/// access is possible only when both install and network flags are explicit.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_setup_command<B: AudioBackend>(
    backend: &B,
    data_root: &PathBuf,
    microphone: Option<&str>,
    model: &str,
    install_model: bool,
    network: bool,
    format: OutputFormat,
    output: &mut impl io::Write,
) -> Result<(), CliError>
where
    B::Stream: Send + 'static,
{
    fs::create_dir_all(data_root)?;
    let permissions = backend.permissions()?;
    let microphone_permission = permissions
        .iter()
        .find(|capability| capability.source == SourceKind::Microphone)
        .map_or_else(
            || "unknown".to_owned(),
            |capability| format!("{:?}", capability.permission),
        );
    if permissions.iter().any(|capability| {
        capability.source == SourceKind::Microphone
            && matches!(
                capability.permission,
                koe_core::PermissionState::Denied
                    | koe_core::PermissionState::Restricted
                    | koe_core::PermissionState::Revoked
            )
    }) {
        return Err(CliError::Audio(AudioError::PermissionDenied));
    }
    let devices = backend.enumerate(SourceKind::Microphone)?;
    let microphone = match microphone {
        Some(id) if devices.iter().any(|device| device.id == id) => id.to_owned(),
        Some(id) => {
            return Err(CliError::SelectionRequired(format!(
                "microphone `{}` is not available; run `koe devices list` and retry setup",
                terminal_safe(id)
            )));
        },
        None if devices.len() == 1 => devices[0].id.clone(),
        None => {
            return Err(CliError::SelectionRequired(
                "setup needs --mic when zero or multiple microphones are available".to_owned(),
            ));
        },
    };
    probe_microphone(backend, &microphone)?;

    // Recovery deliberately runs before touching model state, so a broken or
    // absent runtime can never prevent durable audio salvage.
    let recovered = recover_sessions_and_transcripts(data_root)?;
    if model != "none" {
        let manager = model_manager(data_root, network)?;
        let selector = model.parse::<koe_model::ModelSelector>()?;
        let mut installed_id = manager.installed_id_for(&selector)?;
        if installed_id.is_none() && install_model {
            if !network {
                return Err(CliError::Model(koe_model::ModelError::NetworkDenied));
            }
            let installed = run_blocking(install_model_with_progress(
                &manager,
                &selector,
                false,
                format,
                tokio_util::sync::CancellationToken::new(),
                None,
            ))?;
            installed_id = Some(installed.id);
        }
        let installed_id = installed_id.ok_or(CliError::Model(koe_model::ModelError::NotFound))?;
        let installed = manager.installed_model(&installed_id)?;
        let model_verified = !matches!(installed.manifest.verification, Verification::Quarantined);
        if !model_verified {
            return Err(CliError::Model(koe_model::ModelError::VerifyFailed));
        }
        let _baseline = run_blocking(async {
            manager
                .run_benchmark(
                    &installed_id,
                    &AsrSessionSettings::default(),
                    BENCHMARK_AUDIO,
                    "",
                )
                .await
                .map_err(CliError::Model)
        })?;
    }

    let mut config = config::load_or_migrate(data_root)?;
    config.defaults.microphone_id = Some(microphone.clone());
    config.defaults.model_selector = Some(model.to_owned());
    config::save(data_root, &config)?;
    let next_command_argv = vec![
        "koe".to_owned(),
        "record".to_owned(),
        "--mic".to_owned(),
        microphone.clone(),
        "--model".to_owned(),
        model.to_owned(),
        "--output".to_owned(),
        data_root.display().to_string(),
        "--consent".to_owned(),
    ];
    let report = SetupReport {
        schema_version: 1,
        data_root_ready: true,
        microphone,
        permission: microphone_permission,
        model: model.to_owned(),
        model_verified: true,
        offline_smoke_test: model != "none",
        recovered_sessions: recovered.len(),
        next_command_argv,
    };
    render(&report, format, output, || {
        format!(
            "setup ready\nmicrophone: {} ({})\nmodel: {} (verified={}, smoke-test={})\nrecovered: {}\nnext: {}",
            terminal_safe(&report.microphone),
            report.permission,
            terminal_safe(&report.model),
            report.model_verified,
            report.offline_smoke_test,
            report.recovered_sessions,
            render_argv(&report.next_command_argv)
        )
    })
}

/// Opens and starts a short capture to distinguish an enumerable device from
/// one that is actually usable by the current process. The probe is bounded so
/// a silent input cannot hang setup.
fn probe_microphone<B: AudioBackend>(
    backend: &B,
    microphone: &str,
) -> Result<(), CliError>
where
    B::Stream: Send + 'static,
{
    let stream = backend.open(&OpenSource {
        device_id: microphone.to_owned(),
        kind: SourceKind::Microphone,
        preferred_sample_rate: 48_000,
        preferred_channels: 1,
        negotiation: koe_audio::FormatNegotiation::Nearest,
    })?;
    let (completion, completed) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut stream = stream;
        let result = (|| {
            let (producer, consumer) = frame_ring(4, 16_384)?;
            stream.start(Box::new(producer))?;
            // Starting the stream is the permission/device negotiation probe.
            // A muted but valid device need not deliver non-empty audio.
            thread::sleep(Duration::from_millis(25));
            let capture = consumer.take_runtime_failure().map_or_else(
                || {
                    if consumer.take_device_lost() {
                        Err(AudioError::DeviceLost)
                    } else {
                        Ok(())
                    }
                },
                |failure| Err(failure.audio_error()),
            );
            let stop = stream.stop();
            capture?;
            stop
        })();
        let _ignored = completion.send(result);
    });
    completed
        .recv_timeout(SETUP_PROBE_TIMEOUT)
        .map_err(|_| CliError::SetupProbeTimedOut)??;
    Ok(())
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
                    "No sessions yet. Start one with `koe record`.".to_owned()
                } else {
                    let rows = summaries
                        .iter()
                        .map(|summary| {
                            vec![
                                terminal_safe(&summary.session_id),
                                format_timestamp_ms(summary.started_at_ms),
                                format_duration_ms(summary.duration_ms),
                                terminal_safe(&summary.state),
                                summary.audio_files.to_string(),
                                if summary.has_transcript { "yes" } else { "no" }.to_owned(),
                            ]
                        })
                        .collect::<Vec<_>>();
                    render_table(
                        &[
                            "SESSION",
                            "STARTED",
                            "DURATION",
                            "STATE",
                            "AUDIO",
                            "TRANSCRIPT",
                        ],
                        &rows,
                    )
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
        "session:      {}\nstate:        {}\nstarted:      {}\nended:        {}\nduration:     {}\nsource:       {}\naudio files:  {}\ntranscript:   {}",
        terminal_safe(&detail.session_id),
        format!("{:?}", manifest.state).to_lowercase(),
        format_timestamp_ms(manifest.started_unix_ms),
        manifest
            .ended_unix_ms
            .map_or_else(|| "n/a".to_owned(), format_timestamp_ms),
        format_duration_ms(duration_ms),
        terminal_safe(&manifest.source_device_id),
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
        ConfigCommand::ApplyRetention { confirm } => {
            let config = config::load_or_migrate(data_root)?;
            let candidates = if *confirm {
                config::apply_retention(data_root, &config)?
            } else {
                config::retention_candidates(data_root, &config)?
            };
            render_collection(&candidates, format, output, || {
                if candidates.is_empty() {
                    "No sessions eligible under the retention policy.".to_owned()
                } else if *confirm {
                    format!("deleted {} session(s)", candidates.len())
                } else {
                    format!(
                        "{} session(s) eligible; review IDs and rerun with --confirm: {}",
                        candidates.len(),
                        candidates
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
            let rows = descriptors
                .iter()
                .map(|descriptor| {
                    vec![
                        terminal_safe(&descriptor.alias.0),
                        terminal_safe(&descriptor.version.0),
                        terminal_safe(&descriptor.variant),
                        terminal_safe(&descriptor.provider),
                        terminal_safe(&descriptor.id.0),
                    ]
                })
                .collect::<Vec<_>>();
            render_table(&["MODEL", "VERSION", "VARIANT", "PROVIDER", "ID"], &rows)
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
    expected_descriptor: Option<ModelDescriptor>,
) -> Result<koe_model::InstalledModel, CliError> {
    let (progress, mut progress_rx) = tokio::sync::mpsc::channel(8);
    let options = InstallOptions {
        policy: NetworkPolicy::ModelInstallOnly,
        cancel: cancel.clone(),
        progress: Some(progress),
        expected_descriptor,
        force_redownload,
    };
    // Print before polling the SDK future. Catalog/native calls may take a
    // while to produce their first yield, especially on a cold macOS cache.
    report_model_progress(format, selector, &koe_model::ModelProgress::Resolving);
    let install = manager.install(selector, &options);
    tokio::pin!(install);
    let mut progress_open = true;
    let mut skip_initial_resolving = true;
    loop {
        tokio::select! {
            result = &mut install => {
                while let Ok(phase) = progress_rx.try_recv() {
                    if skip_initial_resolving
                        && matches!(phase, koe_model::ModelProgress::Resolving)
                    {
                        skip_initial_resolving = false;
                    } else {
                        report_model_progress(format, selector, &phase);
                    }
                }
                return result.map_err(CliError::Model);
            }
            phase = progress_rx.recv(), if progress_open => {
                if let Some(phase) = phase {
                    if skip_initial_resolving
                        && matches!(phase, koe_model::ModelProgress::Resolving)
                    {
                        skip_initial_resolving = false;
                    } else {
                        report_model_progress(format, selector, &phase);
                    }
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
    authorization: &'static str,
    expected_license_id_supplied: bool,
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
    expected_license_id_supplied: bool,
) {
    if matches!(format, OutputFormat::Human) {
        let license_expectation = if expected_license_id_supplied {
            "pinned"
        } else {
            "not-pinned"
        };
        emit_stderr(&format!(
            "model: {} ({}) version={} variant={} provider={} size={} MiB license={} ({}) source={} verification=runtime-only authorization=explicit-recording-install-consent license-expectation={license_expectation}",
            terminal_safe(&descriptor.alias.0),
            terminal_safe(&descriptor.id.0),
            terminal_safe(&descriptor.version.0),
            terminal_safe(&descriptor.variant),
            terminal_safe(&descriptor.provider),
            descriptor.size_mb,
            terminal_safe(&descriptor.license_id),
            terminal_safe(&descriptor.license_description),
            terminal_safe(&descriptor.source),
        ));
    } else {
        let envelope = ModelCandidateEnvelope {
            event,
            authorization: "explicit-recording-install-consent",
            expected_license_id_supplied,
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
    expected_license: Option<&str>,
    language: &str,
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
        expected_license,
        language,
        format,
        cancel,
    )
    .map(Some)
}

#[allow(clippy::type_complexity)]
fn install_model_fresh(
    manager: &KoeModelManager,
    selector: &koe_model::ModelSelector,
    expected_license: Option<&str>,
    format: OutputFormat,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<koe_model::InstalledModelId, CliError> {
    let descriptor = run_blocking(async {
        manager
            .resolve_for_install(selector, NetworkPolicy::ModelInstallOnly, cancel)
            .await
            .map_err(CliError::Model)
    })?;
    report_model_descriptor(
        format,
        "model_install_candidate",
        &descriptor,
        expected_license.is_some(),
    );
    if expected_license.is_some_and(|license| license != descriptor.license_id.as_str()) {
        return Err(koe_model::ModelError::LicenseMismatch.into());
    }
    let installed = run_blocking(install_model_with_progress(
        manager,
        selector,
        false,
        format,
        cancel.clone(),
        Some(descriptor),
    ))?;
    report_installed_model(format, &installed);
    Ok(installed.id)
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn prepare_asr_with_manager(
    manager: &KoeModelManager,
    model: &str,
    install_missing: bool,
    network: bool,
    expected_license: Option<&str>,
    language: &str,
    format: OutputFormat,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(Box<dyn koe_model::StreamingAsrSession>, TranscriptModel), CliError> {
    let selector = model.parse::<koe_model::ModelSelector>()?;
    let installed_id = match manager.installed_id_for(&selector)? {
        Some(installed_id) => {
            let installed = manager.installed_model(&installed_id)?;
            tracing::debug!(model = %selector.key(), "using installed model");
            report_installed_model(format, &installed);
            // Verify the model is actually cached in the SDK before attempting
            // load. A stale manifest from a previous install where the SDK
            // cache was cleared would fail with NotFound at load time.
            let is_cached = run_blocking(async {
                manager
                    .is_model_cached(&installed_id)
                    .await
                    .map_err(CliError::Model)
            })
            .unwrap_or(false);
            if is_cached {
                installed_id
            } else if !install_missing || !network {
                return Err(koe_model::ModelError::NotFound.into());
            } else {
                report_diagnostic(
                    format,
                    "model_reinstall_required",
                    "SDK model cache was cleared; reinstalling…",
                );
                let _ = run_blocking(async {
                    manager.remove(&installed_id).await.map_err(CliError::Model)
                });
                install_model_fresh(manager, &selector, expected_license, format, cancel)?
            }
        },
        None if !install_missing => {
            return Err(koe_model::ModelError::OfflineArtifactMissing.into());
        },
        None if !network => return Err(koe_model::ModelError::NetworkDenied.into()),
        None => install_model_fresh(manager, &selector, expected_license, format, cancel)?,
    };
    let settings = AsrSessionSettings {
        language: Some(language.to_owned()),
        ..AsrSessionSettings::default()
    };
    report_diagnostic(
        format,
        "model_verification_started",
        "preparing ASR: verifying installed model files",
    );
    let loaded =
        run_blocking(async { manager.load(&installed_id).await.map_err(CliError::Model) })?;
    report_diagnostic(
        format,
        "model_load_completed",
        "preparing ASR: model loaded; starting streaming session",
    );
    let session = run_blocking_with_live_tasks(async {
        manager
            .create_asr_session(&installed_id, &settings)
            .await
            .map_err(CliError::Model)
    })?;
    tracing::info!(model = %selector.key(), "ASR session ready");
    Ok((
        session,
        TranscriptModel::new(
            loaded.descriptor.id.0,
            loaded.descriptor.version.0,
            loaded.descriptor.variant,
        )
        .map_err(koe_transcript::TranscriptError::from)?,
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
            let rows = values
                .iter()
                .map(|value| {
                    let persistence = if value.persistent {
                        "persistent"
                    } else {
                        "re-select after restart"
                    };
                    vec![
                        terminal_safe(&value.display_name),
                        terminal_safe(&value.id),
                        terminal_safe(&value.backend),
                        persistence.to_owned(),
                    ]
                })
                .collect::<Vec<_>>();
            render_table(&["NAME", "ID", "BACKEND", "PERSISTENCE"], &rows)
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

/// Renders left-aligned, header-labeled columns for human output. Callers pass
/// already-escaped cells; this only pads and joins them.
fn render_table(
    headers: &[&str],
    rows: &[Vec<String>],
) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|header| header.width()).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(cell.as_str().width());
            }
        }
    }
    let format_row = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                let width = widths.get(index).copied().unwrap_or(0);
                let padding = width.saturating_sub(cell.as_str().width());
                if index + 1 == cells.len() {
                    cell.clone()
                } else {
                    format!("{cell}{}", " ".repeat(padding + 2))
                }
            })
            .collect::<String>()
    };
    let header_cells: Vec<String> = headers.iter().map(|header| (*header).to_owned()).collect();
    let underline_cells: Vec<String> = widths.iter().map(|width| "-".repeat(*width)).collect();
    let mut lines = vec![format_row(&header_cells), format_row(&underline_cells)];
    for row in rows {
        lines.push(format_row(row));
    }
    lines.join("\n")
}

/// Formats an epoch-millisecond timestamp as a human-readable UTC datetime.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn format_timestamp_ms(unix_ms: u128) -> String {
    let total_seconds = (unix_ms / 1_000) as i64;
    let days = total_seconds.div_euclid(86_400);
    let seconds_of_day = total_seconds.rem_euclid(86_400);
    let (hour, minute, second) = (
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60,
    );
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// Converts days since the Unix epoch into a proleptic Gregorian calendar date
/// using Howard Hinnant's algorithm.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
const fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u32;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Formats a millisecond duration as a compact, human-friendly string.
fn format_duration_ms(duration_ms: u64) -> String {
    if duration_ms < 1_000 {
        return format!("{duration_ms}ms");
    }
    let total_seconds = duration_ms / 1_000;
    let (hours, minutes, seconds) = (
        total_seconds / 3_600,
        (total_seconds % 3_600) / 60,
        total_seconds % 60,
    );
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// True only when interactive prompts are safe: human output on a real TTY for
/// both stdin and stdout. JSON/JSONL and any redirected stream stay silent so a
/// machine invocation is never blocked waiting on input.
fn stdio_is_interactive(format: OutputFormat) -> bool {
    matches!(format, OutputFormat::Human) && io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Presents a numbered menu on stderr and returns the chosen zero-based index.
fn prompt_choice(
    prompt: &str,
    options: &[String],
    default_index: Option<usize>,
) -> Result<usize, CliError> {
    loop {
        eprintln!("{prompt}:");
        for (index, option) in options.iter().enumerate() {
            let marker = if Some(index) == default_index {
                "  (current default)"
            } else {
                ""
            };
            eprintln!("  {}) {option}{marker}", index + 1);
        }
        match default_index {
            Some(index) => eprint!(
                "Enter a number [1-{}], or press Enter for {}: ",
                options.len(),
                index + 1
            ),
            None => eprint!("Enter a number [1-{}]: ", options.len()),
        }
        let _ignored = io::stderr().flush();
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            if let Some(index) = default_index {
                return Ok(index);
            }
            return Err(CliError::SelectionRequired(
                "no selection was provided".to_owned(),
            ));
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if let Some(index) = default_index {
                return Ok(index);
            }
            continue;
        }
        if let Ok(number) = trimmed.parse::<usize>()
            && (1..=options.len()).contains(&number)
        {
            return Ok(number - 1);
        }
        eprintln!("Please enter a number between 1 and {}.", options.len());
    }
}

/// Asks a yes/no question on stderr, honoring a default for empty input.
fn prompt_yes_no(
    prompt: &str,
    default: bool,
) -> Result<bool, CliError> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        eprint!("{prompt} {hint}: ");
        let _ignored = io::stderr().flush();
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            return Ok(default);
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => eprintln!("Please answer 'y' or 'n'."),
        }
    }
}

/// Fully-resolved record inputs after applying flags, stored defaults, and any
/// interactive selection.
struct RecordSelection {
    microphone_id: String,
    system_id: Option<String>,
    model: String,
    consent: bool,
}

/// Resolves the microphone, optional system device, model, and consent for a
/// `record` invocation. Interactive menus and confirmation only run on a TTY in
/// human mode; otherwise the same explicit flags and clear errors are required.
#[allow(clippy::too_many_lines)]
fn resolve_record_inputs(
    backend: &impl AudioBackend,
    data_root: &Path,
    mic: Option<&str>,
    system: Option<&str>,
    model: Option<&str>,
    consent: bool,
    format: OutputFormat,
) -> Result<RecordSelection, CliError> {
    let interactive = stdio_is_interactive(format);
    let config = config::load_or_migrate(data_root)?;
    let defaults = config.defaults.clone();

    let mut selection_changed = false;

    // Microphone (required).
    let microphone_id = if let Some(mic) = mic {
        mic.to_owned()
    } else if interactive {
        let devices = backend.enumerate(SourceKind::Microphone)?;
        if devices.is_empty() {
            return Err(CliError::SelectionRequired(
                "no microphones were found; connect one and try again".to_owned(),
            ));
        }
        let default_index = defaults
            .microphone_id
            .as_deref()
            .and_then(|id| devices.iter().position(|device| device.id == id));
        let labels: Vec<String> = devices
            .iter()
            .map(|device| {
                format!(
                    "{}  [{}]",
                    terminal_safe(&device.display_name),
                    terminal_safe(&device.backend)
                )
            })
            .collect();
        let choice = prompt_choice("Select a microphone", &labels, default_index)?;
        let chosen = devices[choice].id.clone();
        selection_changed |= defaults.microphone_id.as_deref() != Some(chosen.as_str());
        chosen
    } else {
        return Err(CliError::SelectionRequired(
            "--mic is required; run `koe devices list` to find an ID, or run in a terminal to pick from a menu".to_owned(),
        ));
    };

    // System audio (optional).
    let system_id = if let Some(system) = system {
        Some(system.to_owned())
    } else if interactive {
        let devices = backend.enumerate(SourceKind::System)?;
        if devices.is_empty() {
            None
        } else {
            let mut labels: Vec<String> = vec!["No system audio (microphone only)".to_owned()];
            labels.extend(devices.iter().map(|device| {
                format!(
                    "{}  [{}]",
                    terminal_safe(&device.display_name),
                    terminal_safe(&device.backend)
                )
            }));
            let default_index = defaults
                .system_audio_id
                .as_deref()
                .and_then(|id| devices.iter().position(|device| device.id == id))
                .map_or(0, |index| index + 1);
            let choice = prompt_choice("Also capture system audio?", &labels, Some(default_index))?;
            let chosen = if choice == 0 {
                None
            } else {
                Some(devices[choice - 1].id.clone())
            };
            selection_changed |= defaults.system_audio_id.as_deref() != chosen.as_deref();
            chosen
        }
    } else {
        None
    };

    // Model (required; `none` means audio-only).
    let model = if let Some(model) = model {
        model.to_owned()
    } else if interactive {
        let manager = model_manager(&data_root.to_path_buf(), false)?;
        let installed = run_blocking(async {
            manager
                .list(koe_model::ModelScope::Installed)
                .await
                .map_err(CliError::Model)
        })
        .unwrap_or_default();
        let mut labels: Vec<String> = installed
            .iter()
            .map(|descriptor| {
                format!(
                    "{} ({})",
                    terminal_safe(&descriptor.alias.0),
                    terminal_safe(&descriptor.version.0)
                )
            })
            .collect();
        labels.push("No model — record audio only".to_owned());
        let default_index = defaults
            .model_selector
            .as_deref()
            .and_then(|selector| {
                installed
                    .iter()
                    .position(|descriptor| descriptor.alias.0 == selector)
            })
            .unwrap_or(installed.len());
        if installed.is_empty() {
            report_diagnostic(
                format,
                "no_models_installed",
                "no models are installed; choose audio-only, or install one with `koe models install`",
            );
        }
        let choice = prompt_choice("Select a transcription model", &labels, Some(default_index))?;
        let chosen = if choice == installed.len() {
            "none".to_owned()
        } else {
            installed[choice].alias.0.clone()
        };
        selection_changed |= defaults.model_selector.as_deref() != Some(chosen.as_str());
        chosen
    } else {
        return Err(CliError::SelectionRequired(
            "--model is required (use `none` for audio-only); run `koe models list --installed` to see options, or run in a terminal to pick from a menu".to_owned(),
        ));
    };

    // Offer to remember interactive picks for next time.
    if interactive && selection_changed && prompt_yes_no("Save these as your defaults?", false)? {
        let mut updated = config;
        updated.defaults.microphone_id = Some(microphone_id.clone());
        updated.defaults.system_audio_id.clone_from(&system_id);
        updated.defaults.model_selector = Some(model.clone());
        match config::save(data_root, &updated) {
            Ok(()) => report_diagnostic(
                format,
                "defaults_saved",
                "saved your selections as defaults",
            ),
            Err(error) => report_diagnostic(
                format,
                "defaults_save_failed",
                &format!("could not save defaults: {error}"),
            ),
        }
    }

    // Consent.
    let consent = if consent {
        true
    } else if interactive {
        confirm_recording_interactively(&microphone_id, system_id.as_deref(), &model, data_root)?
    } else {
        return Err(CliError::ConsentRequired);
    };
    if !consent {
        return Err(CliError::ConsentRequired);
    }

    Ok(RecordSelection {
        microphone_id,
        system_id,
        model,
        consent,
    })
}

/// Shows the resolved recording plan and asks the user to confirm.
fn confirm_recording_interactively(
    microphone_id: &str,
    system_id: Option<&str>,
    model: &str,
    data_root: &Path,
) -> Result<bool, CliError> {
    let transcription = if model == "none" {
        "audio only — no transcription"
    } else {
        "on-device transcription"
    };
    eprintln!();
    eprintln!("Ready to record:");
    eprintln!("  microphone:   {}", terminal_safe(microphone_id));
    eprintln!(
        "  system audio: {}",
        system_id.map_or_else(|| "none".to_owned(), terminal_safe)
    );
    eprintln!("  model:        {} ({transcription})", terminal_safe(model));
    eprintln!(
        "  saved to:     {}",
        terminal_safe(&data_root.display().to_string())
    );
    let retention = config::load_or_migrate(data_root).map_or_else(
        |_| "until explicitly deleted".to_owned(),
        |config| retention_label(config.retention),
    );
    eprintln!("  privacy:      stays on this machine; retained {retention}");
    eprintln!();
    prompt_yes_no("Start recording now?", true)
}

fn retention_label(policy: config::RetentionPolicy) -> String {
    match policy {
        config::RetentionPolicy::Forever => "until explicitly deleted".to_owned(),
        config::RetentionPolicy::Days(days) => format!("for {days} day(s) after completion"),
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn record<B: AudioBackend>(
    backend: &B,
    microphone_id: &str,
    system_id: Option<&str>,
    model: &str,
    install_missing_model: bool,
    network: bool,
    expected_model_license: Option<&str>,
    language: &str,
    data_root: &PathBuf,
    sample_rate: u32,
    channels: u16,
    consent: bool,
    format: OutputFormat,
    output: &mut impl io::Write,
    model_manager_override: Option<&KoeModelManager>,
    interrupts_override: Option<Arc<AtomicUsize>>,
) -> Result<(), CliError> {
    if !consent {
        return Err(CliError::ConsentRequired);
    }
    let asr_enabled = model != "none";
    let interrupts = interrupts_override.unwrap_or_else(|| Arc::new(AtomicUsize::new(0)));
    let interrupt_phase = Arc::new(AtomicU8::new(PHASE_PREPARING));
    let install_cancel = tokio_util::sync::CancellationToken::new();
    let interrupt_handler = Arc::clone(&interrupts);
    let handler_phase = Arc::clone(&interrupt_phase);
    let handler_cancel = install_cancel.clone();
    if model_manager_override.is_none() {
        ctrlc::set_handler(move || {
            let previous = interrupt_handler.fetch_add(1, Ordering::Relaxed);
            handler_cancel.cancel();
            // Before a recorder exists there is nothing to finalize. Native
            // catalog/load calls are not all cooperatively cancellable, so
            // restore the expected CLI behavior instead of swallowing SIGINT.
            if handler_phase.load(Ordering::Acquire) == PHASE_PREPARING {
                std::process::exit(INTERRUPT_EXIT_CODE);
            }
            // Recorder writes are synchronous and may be inside a slow fsync.
            // Keep normal Ctrl-C finalization, but do not let one blocked
            // request swallow SIGINT forever.
            if previous == 0 {
                let watchdog_phase = Arc::clone(&handler_phase);
                thread::spawn(move || {
                    thread::sleep(INTERRUPT_GRACE_PERIOD);
                    if watchdog_phase.load(Ordering::Acquire) == PHASE_RECORDING {
                        std::process::exit(INTERRUPT_EXIT_CODE);
                    }
                });
            }
        })
        .map_err(|_| CliError::Signal)?;
    }
    // Recovery is independent of model availability and must happen before a
    // consented install/load can fail.
    report_recovered_sessions(data_root, format)?;
    // A consented install, model load and session creation all finish before
    // capture. Network permission is scoped to the missing-model install;
    // inference remains under the manager's frozen `Denied` policy.
    let prepared_asr = prepare_asr(
        data_root,
        model,
        install_missing_model,
        network,
        expected_model_license,
        language,
        format,
        &install_cancel,
        model_manager_override,
    )?;
    let mut stream = backend.open(&OpenSource {
        device_id: microphone_id.to_owned(),
        kind: SourceKind::Microphone,
        preferred_sample_rate: sample_rate,
        preferred_channels: channels,
        negotiation: koe_audio::FormatNegotiation::Exact,
    })?;
    let mut system_stream = system_id
        .map(|device_id| {
            backend.open(&OpenSource {
                device_id: device_id.to_owned(),
                kind: SourceKind::System,
                preferred_sample_rate: sample_rate,
                preferred_channels: channels,
                negotiation: koe_audio::FormatNegotiation::Exact,
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
    let retention = config::load_or_migrate(data_root).map_or_else(
        |_| "until explicitly deleted".to_owned(),
        |config| retention_label(config.retention),
    );
    let asr_note = if asr_enabled {
        "offline ASR; transcript saved to the session transcript dir"
    } else {
        "no model inference (audio-only)"
    };
    let confirmation = if system_id.is_some() {
        format!(
            "confirmed recording: microphone={}, system={}, scope=system-wide, destination={}, retention={}, model={} ({asr_note}), sharing=none",
            safe_microphone_id,
            terminal_safe(system_id.unwrap_or("none")),
            terminal_safe(&data_root.display().to_string()),
            retention,
            safe_model,
        )
    } else {
        format!(
            "confirmed recording: microphone={}, system=none, destination={}, retention={}, model={} ({asr_note}), sharing=none",
            safe_microphone_id,
            terminal_safe(&data_root.display().to_string()),
            retention,
            safe_model,
        )
    };
    report_diagnostic(format, "recording_confirmed", &confirmation);
    // From this point an interrupt must finalize the durable session rather
    // than terminating immediately.
    interrupt_phase.store(PHASE_RECORDING, Ordering::Release);
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
            task.shutdown()?;
            return Err(error.into());
        },
    };
    tracing::info!(
        session_id = %recording.session_id.map_or_else(|| "unknown".to_owned(), |id| id.to_string()),
        microphone = %terminal_safe(microphone_id),
        system = %terminal_safe(system_id.unwrap_or("none")),
        sample_rate = microphone_sample_rate,
        channels = microphone_channels,
        "recording session created"
    );
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
        task.shutdown()?;
        return Err(error.into());
    }
    coordinator.record_permission_result(TrackKind::Microphone, "granted")?;
    if let (Some(system), Some(producer)) = (&mut system_stream, system_producer)
        && let Err(error) = system.start(Box::new(producer))
    {
        stream.stop()?;
        let _failed = coordinator.fail(error.code())?;
        task.shutdown()?;
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
            tracing::warn!(source = "microphone", "audio device lost");
            stream.stop()?;
            coordinator.mark_degraded()?;
            let loss_start = elapsed_ns(session_clock);
            microphone_loss_started = Some(loss_start);
            if let Some((reopened, replacement)) = reopen_source(
                backend,
                &OpenSource {
                    device_id: microphone_id.to_owned(),
                    kind: SourceKind::Microphone,
                    preferred_sample_rate: microphone_sample_rate,
                    preferred_channels: microphone_channels,
                    negotiation: koe_audio::FormatNegotiation::Exact,
                },
                stream.native_sample_format(),
                queue_capacity,
            ) {
                tracing::info!(source = "microphone", "audio device reopened");
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
                tracing::warn!(source = "microphone", "audio device could not be reopened");
                microphone_active = false;
            }
            if no_capture_source_active(microphone_active, system_active) {
                let _cancelled = coordinator.cancel()?;
                task.shutdown()?;
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
                task.shutdown()?;
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
            tracing::warn!(source = "system", "audio device lost");
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
                        preferred_sample_rate: system_sample_rate,
                        preferred_channels: system_channels,
                        negotiation: koe_audio::FormatNegotiation::Exact,
                    },
                    system_native_format,
                    queue_capacity,
                )
            {
                tracing::info!(source = "system", "audio device reopened");
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
                tracing::warn!(source = "system", "audio device could not be reopened");
                system_active = false;
            }
            if no_capture_source_active(microphone_active, system_active) {
                let _cancelled = coordinator.cancel()?;
                task.shutdown()?;
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
        } else if asr.is_some() {
            write_available_mix(
                &mut microphone_mix,
                &mut system_mix,
                &coordinator,
                !microphone_active,
                true,
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
    } else if asr.is_some() {
        write_available_mix(
            &mut microphone_mix,
            &mut system_mix,
            &coordinator,
            true,
            true,
            asr.as_ref(),
        )?;
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
    let cancelled = interrupts.load(Ordering::Relaxed) >= 2;
    if let Some(asr) = asr.take() {
        // Drains remaining chunks, runs the model finalization and
        // materializes `events.jsonl` -> `final.json`/`final.txt`.
        let dropped_chunks = match asr.finish() {
            Ok(dropped_chunks) => dropped_chunks,
            Err(error) => {
                // The durable WAV remains available, but a session must not be
                // advertised as completed when its requested transcript could
                // not be materialized. Recovery/retranscription can retry it.
                let _failed = coordinator.fail(error.code())?;
                task.shutdown()?;
                return Err(error);
            },
        };
        if dropped_chunks != 0 {
            report_diagnostic(
                format,
                "asr_overrun",
                &format!(
                    "live transcription skipped {dropped_chunks} chunk(s) to protect durable recording; retranscribe from the saved WAV"
                ),
            );
        }
    }
    let terminal = if cancelled {
        tracing::info!("recording cancelled by interrupt");
        coordinator.cancel()?
    } else {
        coordinator.stop()?
    };
    task.shutdown()?;
    tracing::info!(state = ?terminal.state, "recording finished");
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
        if candidate.sample_rate() != request.preferred_sample_rate
            || candidate.channels() != request.preferred_channels
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
    dropped_chunks: Arc<AtomicUsize>,
    cancellation: Arc<AtomicBool>,
}

impl AsrBridge {
    fn spawn(
        session: Box<dyn koe_model::StreamingAsrSession>,
        model: TranscriptModel,
        directory: PathBuf,
        format: OutputFormat,
    ) -> Self {
        Self::spawn_with_capacity(session, model, directory, format, 64)
    }

    fn spawn_with_capacity(
        session: Box<dyn koe_model::StreamingAsrSession>,
        model: TranscriptModel,
        directory: PathBuf,
        format: OutputFormat,
        capacity: usize,
    ) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(capacity);
        let cancellation = Arc::new(AtomicBool::new(false));
        let worker_cancellation = Arc::clone(&cancellation);
        let worker = std::thread::spawn(move || {
            asr_worker(
                session,
                &model,
                &directory,
                &receiver,
                format,
                &worker_cancellation,
            )
        });
        Self {
            sender,
            worker: Some(worker),
            dropped_chunks: Arc::new(AtomicUsize::new(0)),
            cancellation,
        }
    }

    fn feed(
        &self,
        samples: Vec<i16>,
        start_us: u64,
    ) -> Result<(), CliError> {
        match self
            .sender
            .try_send(AsrCommand::Chunk { samples, start_us })
        {
            Ok(()) => Ok(()),
            // Persisted WAV is authoritative. Under inference overload we
            // drop live-ASR work instead of ever stalling the capture path;
            // the durable audio remains available for later transcription.
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.dropped_chunks.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                Err(CliError::Model(koe_model::ModelError::Internal))
            },
        }
    }

    /// Stops the feed and waits for the materialized transcript.
    fn finish(self) -> Result<usize, CliError> {
        self.finish_with_timeout(Duration::from_secs(5))
    }

    fn finish_with_timeout(
        mut self,
        timeout: Duration,
    ) -> Result<usize, CliError> {
        let dropped = self.dropped_chunks.load(Ordering::Relaxed);
        let deadline = Instant::now() + timeout;
        let mut stop = AsrCommand::Stop;
        loop {
            match self.sender.try_send(stop) {
                Ok(()) => break,
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    return Err(CliError::Model(koe_model::ModelError::Internal));
                },
                Err(std::sync::mpsc::TrySendError::Full(command)) => {
                    stop = command;
                    if Instant::now() >= deadline {
                        self.cancellation.store(true, Ordering::Release);
                        return Err(CliError::AsrShutdownTimedOut);
                    }
                    thread::sleep(Duration::from_millis(2));
                },
            }
        }
        if let Some(worker) = self.worker.take() {
            while !worker.is_finished() {
                if Instant::now() >= deadline {
                    self.cancellation.store(true, Ordering::Release);
                    return Err(CliError::AsrShutdownTimedOut);
                }
                thread::sleep(Duration::from_millis(2));
            }
            worker
                .join()
                .map_err(|_| CliError::Model(koe_model::ModelError::Internal))??;
        }
        Ok(dropped)
    }
}

fn asr_worker(
    session: Box<dyn koe_model::StreamingAsrSession>,
    model: &TranscriptModel,
    directory: &Path,
    receiver: &std::sync::mpsc::Receiver<AsrCommand>,
    format: OutputFormat,
    cancellation: &AtomicBool,
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
    let processing = (|| -> Result<(), CliError> {
        loop {
            if cancellation.load(Ordering::Acquire) {
                return Err(CliError::AsrShutdownTimedOut);
            }
            let command = match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(command) => command,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
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
        Ok(())
    })();
    // Always finalize the manager-owned guard, even after append/result/store
    // failure, so the production model is explicitly unloaded.
    let transcript = runtime.block_on(session.finish());
    processing?;
    for event in transcript?.events {
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
    let segment = TranscriptSegment::new(
        SegmentId::from(event.segment_id),
        "mixed",
        event.start_us / 1_000,
        event.end_us / 1_000,
        event.text.clone(),
        event.is_final.into(),
        Some(model.clone()),
        Vec::new(),
    )
    .map_err(koe_transcript::TranscriptError::from)?;
    store.append(segment)?;
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
            // The default ASR session advertises a 160 ms caller-side chunk
            // target. Split capture output here rather than pretending the
            // native SDK rechunks arbitrary appends.
            for (index, chunk) in mixed.chunks(2_560).enumerate() {
                let offset_us = u64::try_from(index.saturating_mul(2_560))
                    .unwrap_or(u64::MAX)
                    .saturating_mul(1_000_000)
                    / CANONICAL_SAMPLE_RATE;
                asr.feed(chunk.to_vec(), start_us.saturating_add(offset_us))?;
            }
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
    // Mix directly into one owned buffer. The previous path allocated two
    // temporary source vectors plus an output vector and then copied it again.
    let mixed = (0..count)
        .map(|offset| {
            let sample = cursor.saturating_add(offset as u64);
            microphone
                .sample_at(sample)
                .saturating_add(system.sample_at(sample))
        })
        .collect::<Vec<_>>();
    let start_us = cursor.saturating_mul(1_000_000) / CANONICAL_SAMPLE_RATE;
    cursor = cursor.saturating_add(count as u64);
    microphone.consume_before(cursor);
    system.consume_before(cursor);
    Some((mixed, start_us))
}

fn report_recovered_sessions(
    data_root: &Path,
    format: OutputFormat,
) -> Result<(), CliError> {
    let recovered = recover_sessions_and_transcripts(data_root)?;
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

/// Reconciles recording checkpoints and then lets the transcript store remove
/// only an incomplete trailing JSONL record. Complete malformed records remain
/// an error and original WAVs are preserved by recording recovery artifacts.
fn recover_sessions_and_transcripts(
    data_root: &Path
) -> Result<Vec<koe_recording::SessionManifest>, CliError> {
    let recovered = recover_sessions(data_root)?;
    let sessions_root = data_root.join("sessions");
    let entries = match fs::read_dir(&sessions_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(recovered),
        Err(error) => return Err(error.into()),
    };
    // Transcript repair is intentionally independent from WAV recovery.
    // This makes retries compose: a prior run may already have repaired the
    // recording checkpoint while leaving a torn transcript tail behind.
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir()
            || SessionId::parse(&entry.file_name().to_string_lossy()).is_err()
        {
            continue;
        }
        let transcript_dir = entry.path().join("transcript");
        if transcript_dir.is_dir() {
            drop(TranscriptStore::open(transcript_dir)?);
        }
    }
    Ok(recovered)
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

/// Produces a copy/paste-safe POSIX-shell rendering for human output. Machine
/// output uses `next_command_argv` directly and never has to parse this string.
fn render_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|argument| {
            let safe = terminal_safe(argument);
            format!("'{}'", safe.replace('\'', "'\\''"))
        })
        .collect::<Vec<_>>()
        .join(" ")
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
    #[error("ASR finalization timed out; saved audio can be retranscribed")]
    AsrShutdownTimedOut,
    #[error("microphone setup probe timed out")]
    SetupProbeTimedOut,
    #[error(
        "fresh recording consent is required; review the sources and destination, then pass --consent"
    )]
    ConsentRequired,
    #[error("{0}")]
    SelectionRequired(String),
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
            Self::AsrShutdownTimedOut => "KOE-ASR-FINALIZE-TIMEOUT",
            Self::SetupProbeTimedOut => "KOE-AUDIO-PROBE-TIMEOUT",
            Self::ConsentRequired => "KOE-POLICY-CONSENT-REQUIRED",
            Self::SelectionRequired(_) => "KOE-POLICY-SELECTION-REQUIRED",
            Self::Config(error) => error.code(),
            Self::Session(error) => error.code(),
        }
    }

    const fn remedy(&self) -> &'static str {
        match self {
            Self::ConsentRequired => "review the recording plan and pass --consent",
            Self::SelectionRequired(_) => "select an available device/model and retry",
            Self::Model(koe_model::ModelError::NetworkDenied) => {
                "retry the explicit model install with --network"
            },
            Self::Model(
                koe_model::ModelError::OfflineArtifactMissing | koe_model::ModelError::NotFound,
            ) => "install the model explicitly, then retry offline",
            Self::Audio(_) => "run `koe doctor` and `koe permissions status`, then retry",
            Self::Session(sessions::SessionError::NotFound(_)) => {
                "run `koe sessions list` and use an existing session ID"
            },
            Self::Recording(_) => "run `koe recover --data-root <path>` before retrying",
            Self::AsrShutdownTimedOut => {
                "retry transcription from the saved WAV; the recording is preserved"
            },
            Self::SetupProbeTimedOut => {
                "disconnect the stalled device, run `koe doctor`, and retry setup"
            },
            _ => "inspect the stable error code and retry after correcting the reported condition",
        }
    }

    const fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Audio(_)
                | Self::Io(_)
                | Self::App(_)
                | Self::Recording(_)
                | Self::AsrShutdownTimedOut
                | Self::SetupProbeTimedOut
                | Self::Model(koe_model::ModelError::Busy | koe_model::ModelError::Conflict)
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
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
        Cli, OutputFormat, TimelineMapper, TimelineTrack, TranscriptSegment,
        all_requested_sources_active, capture_stderr, execute, execute_with_model_manager,
        execute_with_model_manager_and_interrupts, model_progress_line, no_capture_source_active,
        prepare_asr, prepare_asr_with_manager, render_argv, render_collection, render_table,
        report_recovered_sessions, reset_source_pipeline, resolve_record_inputs, run_blocking,
        take_available_mix, terminal_safe,
    };

    fn adapter_outbound_attempts(manager: &KoeModelManager) -> usize {
        run_blocking(async {
            manager
                .adapter_outbound_attempts()
                .await
                .map_err(super::CliError::Model)
        })
        .expect("attempt count")
    }

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
    fn machine_record_requires_explicit_selectors_even_with_saved_defaults() {
        let root = TempDir::new().expect("temp");
        let mut stored = super::config::Config::default();
        stored.defaults.microphone_id = Some("saved-mic".to_owned());
        stored.defaults.system_audio_id = Some("saved-system".to_owned());
        stored.defaults.model_selector = Some("saved-model".to_owned());
        super::config::save(root.path(), &stored).expect("save defaults");
        let backend = CountingUnsupportedBackend::default();

        for arguments in [vec!["--model", "none"], vec!["--mic", "explicit-mic"]] {
            let mut command = vec![
                "koe",
                "--output-format",
                "json",
                "record",
                "--output",
                root.path().to_str().expect("UTF-8 root"),
                "--consent",
            ];
            command.extend(arguments);
            let cli = Cli::try_parse_from(command).expect("record CLI");
            let error = execute(&cli, &backend, &mut Vec::new()).expect_err("explicit selector");
            assert_eq!(error.code(), "KOE-POLICY-SELECTION-REQUIRED");
        }
        assert_eq!(backend.opens.load(Ordering::SeqCst), 0);

        let selection = resolve_record_inputs(
            &UnsupportedBackend,
            root.path(),
            Some("explicit-mic"),
            None,
            Some("none"),
            true,
            OutputFormat::Json,
        )
        .expect("explicit machine selection");
        assert_eq!(selection.microphone_id, "explicit-mic");
        assert_eq!(selection.model, "none");
        assert_eq!(selection.system_id, None);
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
        assert_eq!(
            render_argv(&["koe".to_owned(), "a b'c".to_owned()]),
            "'koe' 'a b'\\''c'"
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
    fn human_table_uses_terminal_display_width() {
        let rows = vec![
            vec!["日本語".to_owned(), "a".to_owned()],
            vec!["Cafe\u{301}".to_owned(), "bb".to_owned()],
        ];
        assert_eq!(
            render_table(&["NAME", "ID"], &rows),
            "NAME    ID\n------  --\n日本語  a\nCafe\u{301}    bb"
        );
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
    fn transcript_recovery_retries_after_wav_is_already_terminal() {
        let root = TempDir::new().expect("temp");
        let session_id = {
            let mut recorder =
                SessionRecorder::start(RecordingConfig::microphone(root.path(), 16_000, 1))
                    .expect("recorder");
            recorder.write_samples(&[1, 2, 3]).expect("audio");
            recorder.checkpoint().expect("checkpoint");
            recorder.session_id()
        };
        super::recover_sessions_and_transcripts(root.path()).expect("wav recovery");

        let transcript_dir = root
            .path()
            .join("sessions")
            .join(session_id.to_string())
            .join("transcript");
        {
            let mut store = koe_transcript::TranscriptStore::open(&transcript_dir).expect("store");
            store
                .append(
                    TranscriptSegment::final_segment(0, 1, "kept", None, Vec::new())
                        .expect("segment"),
                )
                .expect("append");
            store.checkpoint().expect("checkpoint");
        }
        {
            use std::io::Write as _;
            let mut events = std::fs::OpenOptions::new()
                .append(true)
                .open(transcript_dir.join("events.jsonl"))
                .expect("events");
            events
                .write_all(b"{\"schema_version\":")
                .expect("torn tail");
        }

        let second = super::recover_sessions_and_transcripts(root.path())
            .expect("independent transcript retry");
        assert!(second.is_empty(), "WAV was already terminal");
        let events = fs::read_to_string(transcript_dir.join("events.jsonl")).expect("events");
        assert!(!events.ends_with("{\"schema_version\":"));
    }

    #[test]
    fn asr_overload_counts_drops_and_finish_is_bounded() {
        use std::sync::{Condvar, Mutex};

        struct StalledAsr {
            gate: Arc<(Mutex<(bool, bool)>, Condvar)>,
        }

        #[async_trait::async_trait]
        impl koe_model::StreamingAsrSession for StalledAsr {
            async fn append(
                &mut self,
                _chunk: koe_model::Pcm16Mono16k,
            ) -> Result<(), koe_model::AsrError> {
                let (lock, signal) = &*self.gate;
                let mut state = lock.lock().expect("gate");
                state.0 = true;
                signal.notify_all();
                while !state.1 {
                    state = signal.wait(state).expect("wait");
                }
                drop(state);
                Ok(())
            }

            async fn poll_results(
                &mut self
            ) -> Result<Option<koe_model::AsrEvent>, koe_model::AsrError> {
                Ok(None)
            }

            async fn finish(
                self: Box<Self>
            ) -> Result<koe_model::FinalTranscript, koe_model::AsrError> {
                Ok(koe_model::FinalTranscript::default())
            }
        }

        let root = TempDir::new().expect("temp");
        let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let bridge = super::AsrBridge::spawn_with_capacity(
            Box::new(StalledAsr {
                gate: Arc::clone(&gate),
            }),
            koe_transcript::TranscriptModel::new("fixture", "1", "cpu").expect("model"),
            root.path().join("transcript"),
            OutputFormat::Json,
            1,
        );
        bridge.feed(vec![1], 0).expect("first");
        {
            let (lock, signal) = &*gate;
            let mut state = lock.lock().expect("gate");
            while !state.0 {
                state = signal.wait(state).expect("wait");
            }
            drop(state);
        }
        bridge.feed(vec![2], 1).expect("queued");
        bridge
            .feed(vec![3], 2)
            .expect("dropped without blocking capture");
        assert_eq!(bridge.dropped_chunks.load(Ordering::Relaxed), 1);
        let started = Instant::now();
        let error = bridge
            .finish_with_timeout(Duration::from_millis(20))
            .expect_err("full queue must time out");
        assert!(matches!(error, super::CliError::AsrShutdownTimedOut));
        assert_eq!(error.code(), "KOE-ASR-FINALIZE-TIMEOUT");
        assert!(error.remedy().contains("saved WAV"));
        assert!(error.retryable());
        assert!(started.elapsed() < Duration::from_millis(200));
        let (lock, signal) = &*gate;
        lock.lock().expect("gate").1 = true;
        signal.notify_all();
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
        assert_eq!(error.code(), "KOE-MODEL-NETWORK-DENIED");
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
                None,
                "auto",
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
                "auto",
                OutputFormat::Jsonl,
                &cancel,
            ) else {
                panic!("missing consent or network permission must fail");
            };
            assert!(matches!(&error, super::CliError::Model(actual) if actual == &expected));
            assert_eq!(error.code(), expected.code());
            assert!(manager.installed_models().expect("installed").is_empty());
            assert_eq!(
                adapter_outbound_attempts(&manager),
                0,
                "denied combinations must not resolve or download"
            );
        }
    }

    #[test]
    fn record_without_recording_consent_never_touches_the_model_adapter() {
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
            "record",
            "--mic",
            "fixture",
            "--model",
            "fixture-nemotron-asr-0.6b",
            "--install-model",
            "--network",
            "--output",
            root.path().to_str().expect("UTF-8 root"),
        ])
        .expect("record CLI");

        let error =
            execute_with_model_manager(&cli, &UnsupportedBackend, &mut Vec::new(), Some(&manager))
                .expect_err("recording consent is required before model access");

        assert_eq!(error.code(), "KOE-POLICY-CONSENT-REQUIRED");
        assert_eq!(adapter_outbound_attempts(&manager), 0);
    }

    #[test]
    fn record_license_expectation_uses_honest_name_and_legacy_alias() {
        for option in ["--expect-model-license", "--accept-model-license"] {
            let cli = Cli::try_parse_from([
                "koe",
                "record",
                "--mic",
                "fixture",
                "--model",
                "fixture-nemotron-asr-0.6b",
                option,
                "fixture-license-apache-2.0",
                "--output",
                "/tmp/koe-license-option-test",
            ])
            .expect("record CLI");
            let super::Command::Record {
                expect_model_license,
                accept_model_license,
                ..
            } = cli.command
            else {
                panic!("record command");
            };
            let parsed = expect_model_license.or(accept_model_license);
            assert_eq!(parsed.as_deref(), Some("fixture-license-apache-2.0"));
        }
        let help = Cli::try_parse_from(["koe", "record", "--help"])
            .expect_err("help exits")
            .to_string();
        assert!(help.contains("--expect-model-license"));
        assert!(!help.contains("--accept-model-license"));

        let legacy = Cli::try_parse_from([
            "koe",
            "record",
            "--mic",
            "fixture",
            "--model",
            "none",
            "--accept-model-license",
            "legacy",
            "--output",
            "/tmp/koe-license-option-test",
        ])
        .expect("legacy CLI");
        let (result, lines) = capture_stderr(|| {
            execute_with_model_manager(&legacy, &UnsupportedBackend, &mut Vec::new(), None)
        });
        assert!(result.is_err());
        assert!(lines[0].contains("--accept-model-license is deprecated"));
        assert!(lines[0].contains("--expect-model-license"));
    }

    #[test]
    fn record_rejects_a_mismatched_optional_license_pin() {
        for expected_license in ["", "wrong-license"] {
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
            let Err(error) = prepare_asr_with_manager(
                &manager,
                "fixture-nemotron-asr-0.6b",
                true,
                true,
                Some(expected_license),
                "auto",
                OutputFormat::Json,
                &cancel,
            ) else {
                panic!("a mismatched license pin must fail");
            };

            assert_eq!(error.code(), "KOE-MODEL-LICENSE-MISMATCH");
            assert!(manager.installed_models().expect("installed").is_empty());
            assert_eq!(adapter_outbound_attempts(&manager), 1);
            assert!(!cache.path().join("models").exists());
        }
    }

    #[test]
    fn record_binds_authorization_to_the_reported_descriptor() {
        let root = TempDir::new().expect("temp");
        let cache = TempDir::new().expect("cache");
        let manager = KoeModelManager::new(
            root.path(),
            DigestAllowlist::empty(),
            Box::new(
                FixtureFoundryAdapter::new(cache.path())
                    .changing_descriptor_after_first_resolution(),
            ),
            NetworkPolicy::Denied,
        )
        .expect("manager");
        let cancel = tokio_util::sync::CancellationToken::new();

        let Err(error) = prepare_asr_with_manager(
            &manager,
            "fixture-nemotron-asr-0.6b",
            true,
            true,
            None,
            "auto",
            OutputFormat::Json,
            &cancel,
        ) else {
            panic!("changed catalog metadata must invalidate the reported descriptor");
        };

        assert_eq!(error.code(), "KOE-MODEL-DESCRIPTOR-CHANGED");
        assert!(manager.installed_models().expect("installed").is_empty());
        assert_eq!(
            adapter_outbound_attempts(&manager),
            2,
            "only the initial and binding resolutions may occur; download must not start"
        );
        assert!(!cache.path().join("models").exists());
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
            None,
            "auto",
            OutputFormat::Jsonl,
            &cancel,
        )
        .expect("consented install and ASR preparation without a license pin");

        assert_eq!(manager.policy(), NetworkPolicy::Denied);
        assert_eq!(
            transcript_model.id(),
            "FixtureLocal/NemotronASRStreaming0.6B"
        );
        assert_eq!(
            manager.installed_models().expect("installed models").len(),
            1
        );

        // Once installed, install/network permissions and the optional pin can
        // be absent: the second preparation is entirely local.
        let (_session, repeated_model) = prepare_asr_with_manager(
            &manager,
            "fixture-nemotron-asr-0.6b",
            false,
            false,
            None,
            "auto",
            OutputFormat::Json,
            &cancel,
        )
        .expect("installed model stays offline");
        assert_eq!(repeated_model.id(), transcript_model.id());
        assert_eq!(
            manager.installed_models().expect("installed models").len(),
            1
        );
    }

    #[test]
    fn reported_record_command_installs_before_audio_and_reuses_offline() {
        let root = TempDir::new().expect("temp");
        let cache = TempDir::new().expect("cache");
        let manager = KoeModelManager::new(
            root.path(),
            DigestAllowlist::empty(),
            Box::new(
                FixtureFoundryAdapter::new(cache.path())
                    .with_selector_alias("nemotron-3.5-asr-streaming-0.6b"),
            ),
            NetworkPolicy::Denied,
        )
        .expect("manager");
        // This is the reported command shape, substituting only the fixture
        // microphone and a safe temporary output directory.
        let cli = Cli::try_parse_from([
            "koe",
            "record",
            "--mic",
            "fixture",
            "--model",
            "nemotron-3.5-asr-streaming-0.6b",
            "--output",
            root.path().to_str().expect("UTF-8 root"),
            "--consent",
            "--install-model",
            "--network",
        ])
        .expect("record CLI");
        let backend = CountingUnsupportedBackend::default();

        for expected_opens in [1, 2] {
            let error = execute_with_model_manager(&cli, &backend, &mut Vec::new(), Some(&manager))
                .expect_err("fixture audio backend stops after model preparation");
            assert!(
                matches!(error, super::CliError::Audio(AudioError::Unsupported)),
                "unexpected command result: {error:?}"
            );
            assert_eq!(backend.opens.load(Ordering::SeqCst), expected_opens);
            assert_eq!(manager.installed_models().expect("installed").len(), 1);
            if expected_opens == 1 {
                assert_eq!(adapter_outbound_attempts(&manager), 3);
            }
        }
        assert_eq!(
            adapter_outbound_attempts(&manager),
            3,
            "the second command must reuse the installed model without another outbound attempt"
        );
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
                    "auto",
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
                    "model_verification_started",
                    "model_load_completed",
                ]
            );
            assert_eq!(events[0]["license_id"], "fixture-license-apache-2.0");
            assert_eq!(
                events[0]["authorization"],
                "explicit-recording-install-consent"
            );
            assert_eq!(events[0]["expected_license_id_supplied"], true);
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

        let descriptor = FixtureFoundryAdapter::fixture_descriptor();
        let ((), lines) = capture_stderr(|| {
            super::report_model_descriptor(
                OutputFormat::Json,
                "model_install_candidate",
                &descriptor,
                false,
            );
        });
        let event: serde_json::Value = serde_json::from_str(&lines[0]).expect("candidate JSON");
        assert_eq!(event["authorization"], "explicit-recording-install-consent");
        assert_eq!(event["expected_license_id_supplied"], false);
    }

    #[test]
    fn human_install_reports_safe_license_metadata_before_download() {
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
                "auto",
                OutputFormat::Human,
                &cancel,
            )
        });
        result.expect("human install");
        assert!(lines[0].contains("license=fixture-license-apache-2.0"));
        assert!(lines[0].contains("(Fixture license for offline tests)"));
        assert!(lines[0].contains("source=fixture://catalog"));
        assert!(lines[0].contains("authorization=explicit-recording-install-consent"));
        assert!(lines[0].contains("license-expectation=pinned"));
        assert!(!lines[0].contains("accept with --accept-model-license"));
        assert_eq!(lines[1], "model fixture-nemotron-asr-0.6b: resolving");
        assert_eq!(lines[2], "model fixture-nemotron-asr-0.6b: downloading");

        let mut unsafe_descriptor = FixtureFoundryAdapter::fixture_descriptor();
        unsafe_descriptor.license_description = "license\n\u{202e}text".to_owned();
        let ((), descriptor_lines) = capture_stderr(|| {
            super::report_model_descriptor(
                OutputFormat::Human,
                "model_install_candidate",
                &unsafe_descriptor,
                false,
            );
        });
        assert!(!descriptor_lines[0].contains('\n'));
        assert!(descriptor_lines[0].contains("license\\x0A\\u{202E}text"));

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
        assert_eq!(error.code(), "KOE-MODEL-NETWORK-DENIED");
    }

    #[test]
    fn foundry_force_redownload_is_discoverably_unsupported() {
        let root = TempDir::new().expect("temp");
        let cli = Cli::try_parse_from([
            "koe",
            "models",
            "--data-root",
            root.path().to_str().expect("utf8"),
            "install",
            "nemotron-3.5-asr-streaming-0.6b",
            "--network",
            "--force",
        ])
        .expect("parse");
        let error = execute(&cli, &UnsupportedBackend, &mut Vec::new())
            .expect_err("Foundry cannot safely replace a cached model");
        assert_eq!(error.code(), "KOE-MODEL-FORCE-REDOWNLOAD-UNSUPPORTED");

        let help =
            Cli::try_parse_from(["koe", "models", "--data-root", "/tmp", "install", "--help"])
                .expect_err("help exits")
                .to_string();
        assert!(help.contains("Foundry Local SDK 1.2.3 does not"));
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

        let segment =
            TranscriptSegment::final_segment(0, 10, "offline text".to_owned(), None, Vec::new())
                .expect("segment");
        fs::write(
            session_dir.join("transcript/final.json"),
            serde_json::to_vec(&vec![segment]).expect("transcript json"),
        )
        .expect("transcript");
        let transcript_cli = Cli::try_parse_from([
            "koe",
            "--output-format",
            "json",
            "transcript",
            &id.to_string(),
            "--data-root",
            root.path().to_str().expect("utf8"),
        ])
        .expect("parse transcript");
        let mut transcript_output = Vec::new();
        execute(&transcript_cli, &UnsupportedBackend, &mut transcript_output).expect("transcript");
        let transcript_value: serde_json::Value =
            serde_json::from_slice(&transcript_output).expect("transcript output");
        assert_eq!(transcript_value[0]["text"], "offline text");

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
    fn transcript_sanitizes_only_human_rendering() {
        let root = TempDir::new().expect("temp");
        let mut recorder =
            SessionRecorder::start(RecordingConfig::microphone(root.path(), 16_000, 1))
                .expect("recorder");
        let id = recorder.session_id();
        recorder.finalize(false).expect("complete");
        let session_dir = root.path().join("sessions").join(id.to_string());
        fs::create_dir_all(session_dir.join("transcript")).expect("dirs");
        let segment = TranscriptSegment::final_segment(0, 1, "safe\u{1b}[31m", None, Vec::new())
            .expect("segment");
        fs::write(
            session_dir.join("transcript/final.json"),
            serde_json::to_vec(&vec![segment]).expect("json"),
        )
        .expect("transcript");

        let id_string = id.to_string();
        let base = [
            "koe",
            "transcript",
            &id_string,
            "--data-root",
            root.path().to_str().expect("utf8"),
        ];
        let human = Cli::try_parse_from(base).expect("human CLI");
        let mut output = Vec::new();
        execute(&human, &UnsupportedBackend, &mut output).expect("human");
        let rendered = String::from_utf8(output).expect("utf8");
        assert!(rendered.contains("\\x1B"));
        assert!(!rendered.contains('\u{1b}'));

        let machine = Cli::try_parse_from([
            "koe",
            "--output-format",
            "json",
            "transcript",
            &id_string,
            "--data-root",
            root.path().to_str().expect("utf8"),
        ])
        .expect("json CLI");
        let mut output = Vec::new();
        execute(&machine, &UnsupportedBackend, &mut output).expect("json");
        let parsed: serde_json::Value = serde_json::from_slice(&output).expect("json");
        assert_eq!(parsed[0]["text"], "safe\u{1b}[31m");

        let jsonl = Cli::try_parse_from([
            "koe",
            "--output-format",
            "jsonl",
            "transcript",
            &id_string,
            "--data-root",
            root.path().to_str().expect("utf8"),
        ])
        .expect("jsonl CLI");
        let mut output = Vec::new();
        execute(&jsonl, &UnsupportedBackend, &mut output).expect("jsonl");
        let lines: Vec<_> = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_slice(lines[0]).expect("jsonl row");
        assert_eq!(parsed["text"], "safe\u{1b}[31m");

        let final_path = session_dir.join("transcript/final.json");
        fs::remove_file(&final_path).expect("remove final");
        assert_eq!(
            execute(&machine, &UnsupportedBackend, &mut Vec::new())
                .expect_err("missing transcript")
                .code(),
            "KOE-TRANSCRIPT-NOT-FOUND"
        );
        fs::write(&final_path, b"not json").expect("malformed final");
        assert_eq!(
            execute(&machine, &UnsupportedBackend, &mut Vec::new())
                .expect_err("malformed transcript")
                .code(),
            "KOE-SESSION-JSON-FAILED"
        );
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
    fn retention_cli_previews_then_requires_confirmation() {
        let root = TempDir::new().expect("temp");
        let mut recorder =
            SessionRecorder::start(RecordingConfig::microphone(root.path(), 16_000, 1))
                .expect("recorder");
        let id = recorder.session_id();
        recorder.finalize(false).expect("complete");
        let mut config = super::config::Config::default();
        config.retention = super::config::RetentionPolicy::Days(0);
        super::config::save(root.path(), &config).expect("config");

        let preview = Cli::try_parse_from([
            "koe",
            "--output-format",
            "json",
            "config",
            "--data-root",
            root.path().to_str().expect("utf8"),
            "apply-retention",
        ])
        .expect("preview CLI");
        let mut output = Vec::new();
        execute(&preview, &UnsupportedBackend, &mut output).expect("preview");
        let value: serde_json::Value = serde_json::from_slice(&output).expect("json");
        assert_eq!(value[0], id.to_string());
        assert!(root.path().join("sessions").join(id.to_string()).exists());

        let confirm = Cli::try_parse_from([
            "koe",
            "--output-format",
            "json",
            "config",
            "--data-root",
            root.path().to_str().expect("utf8"),
            "apply-retention",
            "--confirm",
        ])
        .expect("confirm CLI");
        execute(&confirm, &UnsupportedBackend, &mut Vec::new()).expect("confirm");
        assert!(!root.path().join("sessions").join(id.to_string()).exists());
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

    /// Deterministic word vocabulary produced by [`koe_model::fixture_transcribe`].
    /// The E2E transcript must consist solely of these words, proving that the
    /// local fixture model — not some other source — produced the transcript.
    const FIXTURE_VOCABULARY: [&str; 16] = [
        "aha", "amma", "ane", "asa", "awa", "baba", "bee", "dada", "e", "ene", "fufu", "koko",
        "mama", "nana", "oh", "yaya",
    ];

    /// In-process audio backend that replays a finite, deterministic PCM stream
    /// and then requests a cooperative stop, so the real-time record loop can be
    /// driven end-to-end without audio hardware.
    struct SyntheticBackend {
        samples: Arc<Vec<i16>>,
        interrupts: Arc<AtomicUsize>,
    }

    impl AudioBackend for SyntheticBackend {
        type Stream = SyntheticStream;

        fn capabilities(&self) -> Result<Vec<AudioCapability>, AudioError> {
            Ok(vec![koe_audio::AudioCapability {
                source: SourceKind::Microphone,
                state: koe_core::CapabilityState::Supported,
                availability: koe_core::Availability::Available,
                permission: koe_core::PermissionState::Granted,
                probe_effect: koe_core::ProbeEffect::None,
                backend: "synthetic".to_owned(),
            }])
        }

        fn permissions(&self) -> Result<Vec<AudioCapability>, AudioError> {
            self.capabilities()
        }

        fn enumerate(
            &self,
            kind: SourceKind,
        ) -> Result<Vec<AudioDevice>, AudioError> {
            if kind == SourceKind::Microphone {
                Ok(vec![AudioDevice {
                    id: "synthetic-mic".to_owned(),
                    display_name: "Synthetic Microphone".to_owned(),
                    backend: "synthetic".to_owned(),
                    kind: SourceKind::Microphone,
                    persistent: true,
                }])
            } else {
                Ok(Vec::new())
            }
        }

        fn open(
            &self,
            request: &OpenSource,
        ) -> Result<Self::Stream, AudioError> {
            Ok(SyntheticStream {
                samples: Arc::clone(&self.samples),
                interrupts: Arc::clone(&self.interrupts),
                sample_rate: request.preferred_sample_rate,
                channels: request.preferred_channels,
                worker: None,
            })
        }
    }

    struct SyntheticStream {
        samples: Arc<Vec<i16>>,
        interrupts: Arc<AtomicUsize>,
        sample_rate: u32,
        channels: u16,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl koe_audio::AudioStream for SyntheticStream {
        fn native_sample_format(&self) -> koe_audio::NativeSampleFormat {
            koe_audio::NativeSampleFormat::I16
        }

        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }

        fn channels(&self) -> u16 {
            self.channels
        }

        fn start(
            &mut self,
            sink: Box<dyn koe_audio::FrameSink>,
        ) -> Result<(), AudioError> {
            let samples = Arc::clone(&self.samples);
            let interrupts = Arc::clone(&self.interrupts);
            let sample_rate = self.sample_rate;
            let channels = self.channels;
            self.worker = Some(std::thread::spawn(move || {
                const FRAME_SAMPLES: usize = 1_600; // 100 ms at 16 kHz mono.
                let mut sequence = 0_u64;
                for (index, chunk) in samples.chunks(FRAME_SAMPLES).enumerate() {
                    let capture_ns = (index as u64).saturating_mul(100_000_000);
                    let metadata = koe_audio::FrameMetadata {
                        sequence,
                        source_kind: SourceKind::Microphone,
                        sample_rate,
                        channels,
                        sample_format: koe_audio::NativeSampleFormat::I16,
                        payload_sample_format: koe_audio::NativeSampleFormat::I16,
                        capture_timestamp_ns: capture_ns,
                        callback_arrival_timestamp_ns: koe_audio::process_timeline_now_ns(),
                        ..koe_audio::FrameMetadata::default()
                    };
                    // The bounded ring is drained by the record loop every few
                    // milliseconds; pace pushes so nothing overflows. Honor a
                    // watchdog stop while retrying so `stop()` cannot block
                    // forever joining a producer after the consumer exits.
                    while sink.try_send(metadata, chunk).is_err() {
                        if interrupts.load(Ordering::SeqCst) != 0 {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    sequence = sequence.saturating_add(1);
                    std::thread::sleep(Duration::from_millis(2));
                }
                // Give the loop time to drain and feed the ASR bridge before it
                // observes the cooperative stop and finalizes the session.
                std::thread::sleep(Duration::from_millis(50));
                interrupts.store(1, Ordering::SeqCst);
            }));
            Ok(())
        }

        fn stop(&mut self) -> Result<(), AudioError> {
            if let Some(worker) = self.worker.take() {
                worker.join().map_err(|_| AudioError::StreamRuntimeFailed)?;
            }
            Ok(())
        }
    }

    #[test]
    fn setup_is_offline_and_idempotent_for_audio_only_use() {
        let root = TempDir::new().expect("temp");
        let interrupts = Arc::new(AtomicUsize::new(0));
        let backend = SyntheticBackend {
            samples: Arc::new(Vec::new()),
            interrupts,
        };
        let cli = Cli::try_parse_from([
            "koe",
            "--output-format",
            "json",
            "setup",
            "--data-root",
            root.path().to_str().expect("utf8"),
            "--mic",
            "synthetic-mic",
            "--model",
            "none",
        ])
        .expect("setup CLI");

        for _ in 0..2 {
            let mut output = Vec::new();
            execute(&cli, &backend, &mut output).expect("idempotent setup");
            let report: serde_json::Value = serde_json::from_slice(&output).expect("report");
            assert_eq!(report["data_root_ready"], true);
            assert_eq!(report["model_verified"], true);
            assert_eq!(report["offline_smoke_test"], false);
            assert_eq!(report["next_command_argv"][0], "koe");
            assert_eq!(report["next_command_argv"][1], "record");
        }
        let config = super::config::load_or_migrate(root.path()).expect("config");
        assert_eq!(
            config.defaults.microphone_id.as_deref(),
            Some("synthetic-mic")
        );
        assert_eq!(config.defaults.model_selector.as_deref(), Some("none"));
    }

    #[test]
    fn setup_recovers_audio_before_reporting_a_missing_offline_model() {
        let root = TempDir::new().expect("temp");
        let session_id = {
            let mut recorder =
                SessionRecorder::start(RecordingConfig::microphone(root.path(), 16_000, 1))
                    .expect("recorder");
            recorder.write_samples(&[1, 2]).expect("audio");
            recorder.checkpoint().expect("checkpoint");
            recorder.session_id()
        };
        let backend = SyntheticBackend {
            samples: Arc::new(Vec::new()),
            interrupts: Arc::new(AtomicUsize::new(0)),
        };
        let cli = Cli::try_parse_from([
            "koe",
            "setup",
            "--data-root",
            root.path().to_str().expect("utf8"),
            "--mic",
            "synthetic-mic",
            "--model",
            "definitely-missing",
        ])
        .expect("CLI");
        assert!(matches!(
            execute(&cli, &backend, &mut Vec::new()),
            Err(super::CliError::Model(koe_model::ModelError::NotFound))
        ));
        let manifest: koe_recording::SessionManifest = serde_json::from_slice(
            &fs::read(
                root.path()
                    .join("sessions")
                    .join(session_id.to_string())
                    .join("session.json"),
            )
            .expect("manifest"),
        )
        .expect("json");
        assert_eq!(manifest.state, SessionState::RecoveredPartial);
    }

    #[test]
    fn setup_rejects_denied_permission_before_opening_capture() {
        struct DeniedBackend(Arc<AtomicBool>);
        impl AudioBackend for DeniedBackend {
            type Stream = UnsupportedStream;
            fn capabilities(&self) -> Result<Vec<AudioCapability>, AudioError> {
                Ok(vec![AudioCapability {
                    source: SourceKind::Microphone,
                    state: koe_core::CapabilityState::PermissionRequired,
                    availability: koe_core::Availability::Available,
                    permission: koe_core::PermissionState::Denied,
                    probe_effect: koe_core::ProbeEffect::None,
                    backend: "denied".to_owned(),
                }])
            }
            fn permissions(&self) -> Result<Vec<AudioCapability>, AudioError> {
                self.capabilities()
            }
            fn enumerate(
                &self,
                _kind: SourceKind,
            ) -> Result<Vec<AudioDevice>, AudioError> {
                Ok(vec![AudioDevice {
                    id: "denied".to_owned(),
                    display_name: "Denied".to_owned(),
                    backend: "denied".to_owned(),
                    kind: SourceKind::Microphone,
                    persistent: true,
                }])
            }
            fn open(
                &self,
                _request: &OpenSource,
            ) -> Result<Self::Stream, AudioError> {
                self.0.store(true, Ordering::SeqCst);
                Err(AudioError::PermissionDenied)
            }
        }
        let root = TempDir::new().expect("temp");
        let opened = Arc::new(AtomicBool::new(false));
        let cli = Cli::try_parse_from([
            "koe",
            "setup",
            "--data-root",
            root.path().to_str().expect("utf8"),
            "--mic",
            "denied",
            "--model",
            "none",
        ])
        .expect("CLI");
        assert!(matches!(
            execute(&cli, &DeniedBackend(Arc::clone(&opened)), &mut Vec::new()),
            Err(super::CliError::Audio(AudioError::PermissionDenied))
        ));
        assert!(!opened.load(Ordering::SeqCst));
    }

    #[test]
    fn setup_probe_bounds_a_stalled_stream_start() {
        struct StalledBackend;
        struct StalledStream;
        impl koe_audio::AudioStream for StalledStream {
            fn native_sample_format(&self) -> koe_audio::NativeSampleFormat {
                koe_audio::NativeSampleFormat::I16
            }
            fn sample_rate(&self) -> u32 {
                48_000
            }
            fn channels(&self) -> u16 {
                1
            }
            fn start(
                &mut self,
                _sink: Box<dyn koe_audio::FrameSink>,
            ) -> Result<(), AudioError> {
                std::thread::sleep(Duration::from_secs(1));
                Ok(())
            }
            fn stop(&mut self) -> Result<(), AudioError> {
                Ok(())
            }
        }
        impl AudioBackend for StalledBackend {
            type Stream = StalledStream;
            fn capabilities(&self) -> Result<Vec<AudioCapability>, AudioError> {
                Ok(Vec::new())
            }
            fn permissions(&self) -> Result<Vec<AudioCapability>, AudioError> {
                Ok(Vec::new())
            }
            fn enumerate(
                &self,
                _kind: SourceKind,
            ) -> Result<Vec<AudioDevice>, AudioError> {
                Ok(Vec::new())
            }
            fn open(
                &self,
                _request: &OpenSource,
            ) -> Result<Self::Stream, AudioError> {
                Ok(StalledStream)
            }
        }

        let started = Instant::now();
        assert!(matches!(
            super::probe_microphone(&StalledBackend, "stalled"),
            Err(super::CliError::SetupProbeTimedOut)
        ));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    /// Deterministic 16 kHz mono PCM: a blend of tones so the fixture model
    /// produces a varied, reproducible word sequence.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn synthetic_pcm(sample_count: usize) -> Vec<i16> {
        (0..sample_count)
            .map(|index| {
                let t = index as f64 / 16_000.0;
                let tone = 0.5f64.mul_add(
                    (t * 2.0 * std::f64::consts::PI * 1_200.0).sin(),
                    (t * 2.0 * std::f64::consts::PI * 440.0).sin(),
                );
                (tone * 9_000.0) as i16
            })
            .collect()
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn e2e_local_model_records_and_transcribes_to_durable_artifacts() {
        let root = TempDir::new().expect("temp");
        let cache = TempDir::new().expect("cache");
        let manager = KoeModelManager::new(
            root.path(),
            DigestAllowlist::empty(),
            Box::new(FixtureFoundryAdapter::new(cache.path())),
            NetworkPolicy::Denied,
        )
        .expect("manager");

        // Two seconds of deterministic mono PCM at the canonical ASR rate.
        let pcm = Arc::new(synthetic_pcm(32_000));
        let interrupts = Arc::new(AtomicUsize::new(0));
        let timed_out = Arc::new(AtomicBool::new(false));
        let (cancel_watchdog, watchdog_cancelled) = std::sync::mpsc::sync_channel(1);
        let watchdog_interrupts = Arc::clone(&interrupts);
        let watchdog_timed_out = Arc::clone(&timed_out);
        let watchdog = std::thread::spawn(move || {
            match watchdog_cancelled.recv_timeout(Duration::from_secs(10)) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {},
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    watchdog_timed_out.store(true, Ordering::SeqCst);
                    watchdog_interrupts.store(1, Ordering::SeqCst);
                },
            }
        });
        let backend = SyntheticBackend {
            samples: Arc::clone(&pcm),
            interrupts: Arc::clone(&interrupts),
        };

        let cli = Cli::try_parse_from([
            "koe",
            "record",
            "--mic",
            "synthetic-mic",
            "--model",
            "fixture-nemotron-asr-0.6b",
            "--install-model",
            "--network",
            "--sample-rate",
            "16000",
            "--channels",
            "1",
            "--output",
            root.path().to_str().expect("UTF-8 root"),
            "--consent",
        ])
        .expect("record CLI");

        let mut output = Vec::new();
        let result = execute_with_model_manager_and_interrupts(
            &cli,
            &backend,
            &mut output,
            &manager,
            Arc::clone(&interrupts),
        );
        let _ignored = cancel_watchdog.send(());
        watchdog.join().expect("watchdog thread");
        assert!(
            !timed_out.load(Ordering::SeqCst),
            "end-to-end recording exceeded the 10-second deadline"
        );
        result.expect("end-to-end fixture recording succeeds");

        // 1. A completed session manifest exists.
        let summaries = super::sessions::list_sessions(root.path()).expect("list sessions");
        assert_eq!(summaries.len(), 1, "exactly one session recorded");
        let summary = &summaries[0];
        assert_eq!(summary.state, "completed");
        assert!(summary.has_transcript, "transcript materialized");

        let detail =
            super::sessions::show_session(root.path(), &summary.session_id).expect("show session");
        assert_eq!(detail.manifest.state, SessionState::Completed);

        // 2. Audio files were written to durable storage.
        assert!(
            !detail.manifest.audio_files.is_empty(),
            "audio segments persisted"
        );
        assert!(
            summary.audio_files > 0,
            "session summary reports audio files"
        );

        // 3. A transcript was materialized with deterministic fixture words.
        let transcript = detail.transcript.as_ref().expect("transcript summary");
        assert!(transcript.has_final_json, "final.json materialized");
        assert!(transcript.has_final_txt, "final.txt materialized");
        assert!(transcript.segment_count > 0, "transcript has segments");
        assert!(transcript.final_text_word_count > 0, "transcript has words");

        let transcript_dir = root
            .path()
            .join("sessions")
            .join(&summary.session_id)
            .join("transcript");
        let final_text =
            fs::read_to_string(transcript_dir.join("final.txt")).expect("read final.txt");
        assert!(
            !final_text.trim().is_empty(),
            "final transcript is non-empty"
        );
        for word in final_text.split_whitespace() {
            assert!(
                FIXTURE_VOCABULARY.contains(&word),
                "unexpected transcript word {word:?}; the local fixture model must \
                 produce only fixture vocabulary"
            );
        }
        assert!(
            transcript_dir.join("events.jsonl").is_file(),
            "streaming events log materialized"
        );

        // The fixture model was actually installed under the data root.
        assert_eq!(
            manager.installed_models().expect("installed models").len(),
            1
        );
    }
}
