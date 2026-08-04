use std::{
    io,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use clap::{Parser, Subcommand, ValueEnum};
use koe_app::{AppError, RecorderCoordinator, RecordingConsent};
use koe_audio::{
    AudioBackend, AudioCapability, AudioDevice, AudioError, AudioStream, CpalBackend, OpenSource,
    frame_ring,
};
use koe_core::{CapabilityState, SourceKind};
use koe_recording::{RecordingConfig, RecordingError, recover_sessions};
use serde::Serialize;

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
    /// Run local configuration checks without network access.
    Doctor,
    /// Record microphone PCM until Ctrl-C.
    Record {
        /// Opaque microphone ID from this process's `devices list`; CPAL IDs
        /// must be reselected after restart.
        #[arg(long)]
        mic: String,
        /// Reserved model selector; Milestone 1 records audio without ASR.
        #[arg(long)]
        model: String,
        /// App-owned data root below which `sessions/<uuid>` is created.
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 48_000)]
        sample_rate: u32,
        #[arg(long, default_value_t = 1)]
        channels: u16,
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
        Command::Record {
            mic,
            model: _,
            output: data_root,
            sample_rate,
            channels,
        } => {
            record(
                backend,
                mic,
                data_root,
                *sample_rate,
                *channels,
                cli.output_format,
                output,
            )?;
        },
    }
    Ok(())
}

fn render_capabilities(
    values: &[AudioCapability],
    format: OutputFormat,
    output: &mut impl io::Write,
) -> Result<(), CliError> {
    render_collection(values, format, output, || {
        values
            .iter()
            .map(|value| format!("{:?}: {:?} ({})", value.source, value.state, value.backend))
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
                        value.id, value.display_name, value.backend
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

fn record<B: AudioBackend>(
    backend: &B,
    microphone_id: &str,
    data_root: &PathBuf,
    sample_rate: u32,
    channels: u16,
    format: OutputFormat,
    output: &mut impl io::Write,
) -> Result<(), CliError> {
    report_recovered_sessions(data_root)?;
    let mut stream = backend.open(&OpenSource {
        device_id: microphone_id.to_owned(),
        kind: SourceKind::Microphone,
        sample_rate,
        channels,
    })?;
    let mut config = RecordingConfig::microphone(data_root, sample_rate, channels);
    microphone_id.clone_into(&mut config.source_device_id);
    stream
        .native_sample_format()
        .manifest_label()
        .clone_into(&mut config.native_sample_format);
    let queue_capacity = config.queue_capacity;
    let (producer, consumer) = frame_ring(queue_capacity, 16_384)?;
    let (coordinator, task) = RecorderCoordinator::spawn(config);
    let recording = coordinator.start(RecordingConsent {
        microphone: true,
        storage: true,
    })?;
    let interrupts = Arc::new(AtomicUsize::new(0));
    let interrupt_handler = Arc::clone(&interrupts);
    ctrlc::set_handler(move || {
        interrupt_handler.fetch_add(1, Ordering::Relaxed);
    })
    .map_err(|_| CliError::Signal)?;
    stream.start(Box::new(producer))?;
    eprintln!("recording; press Ctrl-C to stop (press twice to cancel)");

    let mut samples = vec![0_i16; 16_384];
    while interrupts.load(Ordering::Relaxed) == 0 {
        let dropped = consumer.take_dropped_frames();
        if dropped != 0 {
            coordinator.record_overflow(dropped)?;
        }
        if consumer.take_device_lost() {
            stream.stop()?;
            let _cancelled = coordinator.cancel()?;
            task.shutdown(&coordinator)?;
            return Err(CliError::Audio(AudioError::DeviceLost));
        }
        let lost_discontinuities = consumer.take_discontinuities();
        for _ in 0..lost_discontinuities {
            coordinator.record_discontinuity(0)?;
        }
        while let Some(metadata) = consumer.try_pop(&mut samples)? {
            if metadata.overflow {
                coordinator.record_overflow(metadata.dropped_frames)?;
                continue;
            }
            if metadata.device_lost {
                stream.stop()?;
                let _cancelled = coordinator.cancel()?;
                task.shutdown(&coordinator)?;
                return Err(CliError::Audio(AudioError::DeviceLost));
            }
            if metadata.discontinuity {
                coordinator.record_discontinuity(metadata.capture_timestamp_ns)?;
            }
            let count = usize::try_from(metadata.sample_count)
                .map_err(|_| CliError::Audio(AudioError::UnsupportedFormat))?;
            if count != 0 {
                coordinator.append(samples[..count].to_vec())?;
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    stream.stop()?;
    while let Some(metadata) = consumer.try_pop(&mut samples)? {
        if metadata.overflow {
            coordinator.record_overflow(metadata.dropped_frames)?;
            continue;
        }
        if metadata.discontinuity {
            coordinator.record_discontinuity(metadata.capture_timestamp_ns)?;
        }
        let count = usize::try_from(metadata.sample_count)
            .map_err(|_| CliError::Audio(AudioError::UnsupportedFormat))?;
        if count != 0 {
            coordinator.append(samples[..count].to_vec())?;
        }
    }
    let dropped = consumer.take_dropped_frames();
    if dropped != 0 {
        coordinator.record_overflow(dropped)?;
    }
    let terminal = if interrupts.load(Ordering::Relaxed) >= 2 {
        coordinator.cancel()?
    } else {
        coordinator.stop()?
    };
    task.shutdown(&coordinator)?;
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
    #[error("failed to install Ctrl-C handler")]
    Signal,
}

impl CliError {
    const fn code(&self) -> &'static str {
        match self {
            Self::Audio(error) => error.code(),
            Self::App(error) => error.code(),
            Self::Recording(error) => error.code(),
            Self::Io(_) | Self::Json(_) => "KOE-OUTPUT-FAILED",
            Self::Signal => "KOE-SIGNAL-HANDLER-FAILED",
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use koe_audio::UnsupportedBackend;
    use koe_core::SessionState;
    use koe_recording::{RecordingConfig, SessionRecorder};
    use tempfile::TempDir;

    use super::{Cli, OutputFormat, execute, render_collection, report_recovered_sessions};

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
}
