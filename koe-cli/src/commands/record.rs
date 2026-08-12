//! `koe record` — capture, encode, and optionally transcribe.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Parser;
use koe_core::{
    AudioSourceConfig, OutputFormat, PipelineConfig, PipelineError, RecordingError,
    RecordingPipeline, RecordingSummary, TranscriptFormat, enumerate_apps,
    native_provider_registered,
};

use super::Run;
use super::apps_table::{format_apps_table, prepare_apps};
use crate::MainError;

/// Canonical capture / encode rate (pipeline is fixed to this today).
const CANONICAL_SAMPLE_RATE_HZ: u32 = 48_000;
/// Canonical channel count (stereo interleaved).
const CANONICAL_CHANNELS: u8 = 2;
/// Peak level below this is treated as silence for `--silence-timeout`.
const SILENCE_PEAK_THRESHOLD: f32 = 0.01;

/// Start a recording session with optional live transcription.
///
/// Flag fields mirror CLI switches; they are independent toggles, not a state
/// machine, so packing them into an enum would obscure the clap surface.
#[derive(Debug, Parser)]
#[allow(clippy::struct_excessive_bools)]
pub struct RecordArgs {
    /// Audio source: `system`, `mic`, or `both` (default: system).
    #[arg(long, default_value = "system")]
    pub source: String,

    /// Capture system audio from an app bundle id.
    #[arg(long)]
    pub app_id: Option<String>,

    /// Capture system audio from a process id.
    #[arg(long)]
    pub pid: Option<i32>,

    /// Capture from a display id (not yet supported).
    #[arg(long)]
    pub display: Option<u32>,

    /// Print available capture sources and exit.
    #[arg(long)]
    pub list_sources: bool,

    /// Output sample rate in Hz (only `48000` is supported).
    #[arg(long, default_value_t = CANONICAL_SAMPLE_RATE_HZ)]
    pub sample_rate: u32,

    /// Output channel count (only `2` is supported).
    #[arg(long, default_value_t = CANONICAL_CHANNELS)]
    pub channels: u8,

    /// Disable acoustic echo cancellation for `--source both`.
    #[arg(long)]
    pub no_aec: bool,

    /// Disable comfort noise in AEC output.
    #[arg(long)]
    pub no_comfort_noise: bool,

    /// Speech recognition locale (BCP-47).
    #[arg(long, default_value = "en-US")]
    pub locale: String,

    /// Record audio only; skip transcription.
    #[arg(long)]
    pub no_transcribe: bool,

    /// Print supported speech locales and exit.
    #[arg(long)]
    pub list_locales: bool,

    /// Encoded audio output path.
    #[arg(
        short = 'o',
        long,
        required_unless_present_any = ["list_sources", "list_locales"]
    )]
    pub output: Option<PathBuf>,

    /// Audio container: `ogg`, `wav`, or `flac`.
    #[arg(long, default_value = "ogg")]
    pub format: String,

    /// Transcript format: `txt`, `srt`, `vtt`, or `json`.
    #[arg(long, default_value = "txt")]
    pub transcript_format: String,

    /// Transcript output path (default: `<output>.<transcript-format>`).
    #[arg(long)]
    pub transcript_output: Option<PathBuf>,

    /// Max recording duration (e.g. `30s`, `30m`, `1h`, `2h30m`).
    #[arg(long)]
    pub duration: Option<String>,

    /// Max encoded output size (e.g. `500M`, `2G`).
    #[arg(long)]
    pub max_size: Option<String>,

    /// Stop after this much continuous silence (same syntax as `--duration`).
    #[arg(long)]
    pub silence_timeout: Option<String>,

    /// Play captured audio through the default output device.
    #[arg(short = 'm', long)]
    pub monitor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    DurationLimit,
    MaxSize,
    SilenceTimeout,
    Interrupted,
}

impl Run for RecordArgs {
    fn run(self) -> Result<(), MainError> {
        if self.list_sources {
            return list_sources();
        }
        if self.list_locales {
            list_locales();
            return Ok(());
        }

        let prepared = prepare_session(&self)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| MainError::Internal(format!("tokio runtime: {err}")))?;

        runtime.block_on(run_recording(prepared))
    }
}

#[derive(Debug)]
struct PreparedSession {
    config: PipelineConfig,
    max_duration: Option<Duration>,
    max_bytes: Option<u64>,
    silence_timeout: Option<Duration>,
}

fn prepare_session(args: &RecordArgs) -> Result<PreparedSession, MainError> {
    if args.display.is_some() {
        return Err(MainError::InvalidArgs(
            "--display is not supported yet (no display capture source in the pipeline)".into(),
        ));
    }
    if args.sample_rate != CANONICAL_SAMPLE_RATE_HZ {
        return Err(MainError::InvalidArgs(format!(
            "--sample-rate must be {CANONICAL_SAMPLE_RATE_HZ} (canonical pipeline rate)"
        )));
    }
    if args.channels != CANONICAL_CHANNELS {
        return Err(MainError::InvalidArgs(format!(
            "--channels must be {CANONICAL_CHANNELS} (canonical stereo pipeline)"
        )));
    }

    let output = args.output.clone().ok_or_else(|| {
        MainError::InvalidArgs("--output is required unless listing sources/locales".into())
    })?;

    let source = resolve_source(args)?;
    let audio_format = parse_audio_format(&args.format)?;
    let transcript_format = parse_transcript_format(&args.transcript_format)?;
    let max_duration = args
        .duration
        .as_deref()
        .map(parse_duration)
        .transpose()
        .map_err(MainError::InvalidArgs)?;
    let max_bytes = args
        .max_size
        .as_deref()
        .map(parse_byte_size)
        .transpose()
        .map_err(MainError::InvalidArgs)?;
    let silence_timeout = args
        .silence_timeout
        .as_deref()
        .map(parse_duration)
        .transpose()
        .map_err(MainError::InvalidArgs)?;

    let transcribe = !args.no_transcribe;
    if !transcribe
        && (args.transcript_output.is_some()
            || args.transcript_format != "txt"
            || args.locale != "en-US")
    {
        eprintln!(
            "warning: --no-transcribe ignores --locale / --transcript-format / --transcript-output"
        );
    }
    let transcript_output_path = if transcribe {
        Some(
            args.transcript_output
                .clone()
                .unwrap_or_else(|| default_transcript_path(&output, transcript_format)),
        )
    } else {
        None
    };

    let estimated_duration_hours = max_duration.map(|d| d.as_secs_f64() / 3600.0);

    Ok(PreparedSession {
        config: PipelineConfig {
            source,
            output_path: output,
            transcript_output_path,
            locale: args.locale.clone(),
            audio_format,
            transcript_format,
            enable_aec: !args.no_aec,
            comfort_noise: !args.no_comfort_noise,
            monitor: args.monitor,
            transcribe,
            estimated_duration_hours,
        },
        max_duration,
        max_bytes,
        silence_timeout,
    })
}

fn resolve_source(args: &RecordArgs) -> Result<AudioSourceConfig, MainError> {
    if args.app_id.is_some() && args.pid.is_some() {
        return Err(MainError::InvalidArgs(
            "--app-id and --pid are mutually exclusive".into(),
        ));
    }

    let source = args.source.trim().to_ascii_lowercase();
    match source.as_str() {
        "mic" | "microphone" => {
            if args.app_id.is_some() || args.pid.is_some() {
                return Err(MainError::InvalidArgs(
                    "--source mic does not accept --app-id or --pid".into(),
                ));
            }
            Ok(AudioSourceConfig::Microphone)
        },
        "system" => match (&args.app_id, args.pid) {
            (Some(bundle_id), None) => Ok(AudioSourceConfig::AppAudio {
                bundle_id: bundle_id.clone(),
            }),
            (None, Some(pid)) => Ok(AudioSourceConfig::PidAudio { pid }),
            (None, None) => Err(MainError::InvalidArgs(
                "--source system requires --app-id or --pid".into(),
            )),
            (Some(_), Some(_)) => unreachable!("checked above"),
        },
        "both" => {
            let Some(bundle_id) = &args.app_id else {
                return Err(MainError::InvalidArgs(
                    "--source both requires --app-id".into(),
                ));
            };
            if args.pid.is_some() {
                return Err(MainError::InvalidArgs(
                    "--source both uses --app-id; do not pass --pid".into(),
                ));
            }
            Ok(AudioSourceConfig::Both {
                bundle_id: bundle_id.clone(),
            })
        },
        other => Err(MainError::InvalidArgs(format!(
            "unknown --source '{other}' (expected system, mic, or both)"
        ))),
    }
}

fn parse_audio_format(value: &str) -> Result<OutputFormat, MainError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "ogg" => Ok(OutputFormat::Ogg { quality: 0.5 }),
        "wav" => Ok(OutputFormat::Wav {
            bits_per_sample: 16,
        }),
        "flac" => Ok(OutputFormat::Flac {
            compression_level: 5,
        }),
        other => Err(MainError::InvalidArgs(format!(
            "unknown --format '{other}' (expected ogg, wav, or flac)"
        ))),
    }
}

fn parse_transcript_format(value: &str) -> Result<TranscriptFormat, MainError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "txt" => Ok(TranscriptFormat::Txt),
        "srt" => Ok(TranscriptFormat::Srt),
        "vtt" => Ok(TranscriptFormat::Vtt),
        "json" => Ok(TranscriptFormat::Json),
        other => Err(MainError::InvalidArgs(format!(
            "unknown --transcript-format '{other}' (expected txt, srt, vtt, or json)"
        ))),
    }
}

fn default_transcript_path(
    output: &Path,
    format: TranscriptFormat,
) -> PathBuf {
    let ext = match format {
        TranscriptFormat::Txt => "txt",
        TranscriptFormat::Srt => "srt",
        TranscriptFormat::Vtt => "vtt",
        TranscriptFormat::Json => "json",
    };
    let mut path = output.to_path_buf();
    // Prefer `<stem>.<fmt>` over appending after the audio extension.
    if let Some(stem) = output.file_stem().and_then(|s| s.to_str()) {
        path.set_file_name(format!("{stem}.{ext}"));
    } else {
        path.set_extension(ext);
    }
    path
}

/// Parses durations like `30s`, `5m`, `1h`, `2h30m`, `1h30m10s`.
///
/// Rejects empty, zero, and values that cannot safely form an [`Instant`]
/// deadline (~292 years).
fn parse_duration(input: &str) -> Result<Duration, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("duration is empty".into());
    }
    // Plain integer = seconds.
    if raw.chars().all(|c| c.is_ascii_digit()) {
        let secs: u64 = raw
            .parse()
            .map_err(|_| format!("invalid duration '{raw}'"))?;
        return checked_positive_duration(secs, raw);
    }

    let mut total_secs: u64 = 0;
    let mut number = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_digit() {
            number.push(ch);
            continue;
        }
        if number.is_empty() {
            return Err(format!("invalid duration '{raw}'"));
        }
        let value: u64 = number
            .parse()
            .map_err(|_| format!("invalid duration '{raw}'"))?;
        number.clear();
        let factor = match ch {
            'h' | 'H' => 3600_u64,
            'm' | 'M' => 60,
            's' | 'S' => 1,
            _ => return Err(format!("invalid duration unit '{ch}' in '{raw}'")),
        };
        let part = value
            .checked_mul(factor)
            .ok_or_else(|| format!("duration '{raw}' is too large"))?;
        total_secs = total_secs
            .checked_add(part)
            .ok_or_else(|| format!("duration '{raw}' is too large"))?;
    }
    if !number.is_empty() {
        return Err(format!(
            "duration '{raw}' is missing a unit on trailing digits"
        ));
    }
    checked_positive_duration(total_secs, raw)
}

/// Caps at ~100 years so `Instant::now() + duration` cannot panic.
const MAX_DURATION_SECS: u64 = 100 * 365 * 24 * 60 * 60;

fn checked_positive_duration(
    secs: u64,
    raw: &str,
) -> Result<Duration, String> {
    if secs == 0 {
        return Err(format!("duration '{raw}' must be greater than zero"));
    }
    if secs > MAX_DURATION_SECS {
        return Err(format!("duration '{raw}' is too large"));
    }
    Ok(Duration::from_secs(secs))
}

/// Parses sizes like `500M`, `2G`, `100K`, `1024` (bytes).
fn parse_byte_size(input: &str) -> Result<u64, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("size is empty".into());
    }
    let (digits, unit) = raw.split_at(
        raw.chars()
            .take_while(char::is_ascii_digit)
            .map(char::len_utf8)
            .sum(),
    );
    if digits.is_empty() {
        return Err(format!("invalid size '{raw}'"));
    }
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("invalid size '{raw}'"))?;
    let multiplier = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1u64,
        "K" | "KB" => 1024,
        "M" | "MB" => 1024 * 1024,
        "G" | "GB" => 1024 * 1024 * 1024,
        other => return Err(format!("unknown size unit '{other}' in '{raw}'")),
    };
    value
        .checked_mul(multiplier)
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("size '{raw}' is too large or zero"))
}

fn list_sources() -> Result<(), MainError> {
    if !native_provider_registered() {
        return Err(MainError::NativeBridgeUnavailable("record --list-sources"));
    }
    let apps = prepare_apps(enumerate_apps(), false);
    print!("{}", format_apps_table(&apps));
    eprintln!("Re-run with --app-id <bundle> or --pid <pid> to record.");
    Ok(())
}

fn list_locales() {
    // OS-backed locale enumeration is not exported on NativeProvider yet.
    // Print the common on-device set documented for Speech framework hosts.
    println!("Supported locales (common on-device set; OS list not bridged yet):");
    for locale in COMMON_SPEECH_LOCALES {
        println!("  {locale}");
    }
}

const COMMON_SPEECH_LOCALES: &[&str] = &[
    "en-US", "en-GB", "en-AU", "en-CA", "en-IN", "ja-JP", "zh-CN", "zh-TW", "zh-HK", "ko-KR",
    "fr-FR", "fr-CA", "de-DE", "es-ES", "es-MX", "it-IT", "pt-BR", "pt-PT", "ru-RU", "ar-SA",
];

async fn run_recording(prepared: PreparedSession) -> Result<(), MainError> {
    let output_path = prepared.config.output_path.clone();
    let transcript_path = prepared.config.transcript_output_path.clone();

    let mut pipeline = RecordingPipeline::start(prepared.config)
        .await
        .map_err(map_pipeline_error)?;

    eprintln!("Recording → {}", output_path.display());

    let stop_reason = wait_until_done(
        &pipeline,
        prepared.max_duration,
        prepared.max_bytes,
        prepared.silence_timeout,
    )
    .await?;

    let summary = pipeline.stop().await.map_err(map_pipeline_error)?;

    print_summary(&summary, &output_path, transcript_path.as_deref());

    if matches!(stop_reason, StopReason::Interrupted) {
        return Err(MainError::Interrupted);
    }
    Ok(())
}

async fn wait_until_done(
    pipeline: &RecordingPipeline,
    max_duration: Option<Duration>,
    max_bytes: Option<u64>,
    silence_timeout: Option<Duration>,
) -> Result<StopReason, MainError> {
    let mut progress = pipeline.subscribe_progress();
    let deadline = max_duration.map(|d| Instant::now() + d);
    let mut last_sound = Instant::now();
    // Poll silence even when no meter updates arrive.
    let silence_tick = Duration::from_millis(200);
    let mut sigterm = install_terminate_signal()?;

    loop {
        let until_deadline = deadline.map(|at| at.saturating_duration_since(Instant::now()));
        let until_silence = silence_timeout.map(|timeout| {
            timeout
                .saturating_sub(last_sound.elapsed())
                .max(silence_tick)
        });
        let sleep_for = match (until_deadline, until_silence) {
            (Some(d), Some(s)) => d.min(s),
            (Some(d), None) => d,
            (None, Some(s)) => s,
            (None, None) => Duration::from_mins(1),
        };
        let sleep_armed = deadline.is_some() || silence_timeout.is_some();

        tokio::select! {
            ctrl = tokio::signal::ctrl_c() => {
                ctrl.map_err(|err| MainError::Internal(format!("signal: {err}")))?;
                eprintln!("Interrupted — finishing recording…");
                return Ok(StopReason::Interrupted);
            }
            () = recv_terminate(&mut sigterm) => {
                eprintln!("Interrupted — finishing recording…");
                return Ok(StopReason::Interrupted);
            }
            () = tokio::time::sleep(sleep_for), if sleep_armed => {
                if let Some(at) = deadline
                    && Instant::now() >= at
                {
                    return Ok(StopReason::DurationLimit);
                }
                if let Some(timeout) = silence_timeout
                    && last_sound.elapsed() >= timeout
                {
                    return Ok(StopReason::SilenceTimeout);
                }
            }
            status = progress.recv() => {
                match status {
                    Ok(status) => {
                        if let Some(limit) = max_bytes
                            && status.bytes_written >= limit
                        {
                            return Ok(StopReason::MaxSize);
                        }
                        if silence_timeout.is_some() {
                            let peak = status.level_left.max(status.level_right);
                            if peak >= SILENCE_PEAK_THRESHOLD {
                                last_sound = Instant::now();
                            } else if let Some(timeout) = silence_timeout
                                && last_sound.elapsed() >= timeout
                            {
                                return Ok(StopReason::SilenceTimeout);
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Best-effort progress; keep waiting.
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Progress publisher gone; finish gracefully.
                        return Ok(StopReason::DurationLimit);
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
struct TerminateSignal(tokio::signal::unix::Signal);

#[cfg(not(unix))]
struct TerminateSignal;

fn install_terminate_signal() -> Result<TerminateSignal, MainError> {
    #[cfg(unix)]
    {
        let signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|err| MainError::Internal(format!("SIGTERM handler: {err}")))?;
        Ok(TerminateSignal(signal))
    }
    #[cfg(not(unix))]
    {
        Ok(TerminateSignal)
    }
}

async fn recv_terminate(signal: &mut TerminateSignal) {
    #[cfg(unix)]
    {
        let _ = signal.0.recv().await;
    }
    #[cfg(not(unix))]
    {
        let _ = signal;
        std::future::pending::<()>().await;
    }
}

fn print_summary(
    summary: &RecordingSummary,
    output: &Path,
    transcript: Option<&Path>,
) {
    eprintln!(
        "Done: {:.1}s, {} bytes, {} segments → {}",
        summary.duration_sec,
        summary.bytes_written,
        summary.transcript_segment_count,
        output.display()
    );
    if let Some(path) = transcript {
        eprintln!("Transcript → {}", path.display());
    }
    if summary.dropped_audio_frames > 0 {
        eprintln!(
            "warning: dropped {} audio frames during capture",
            summary.dropped_audio_frames
        );
    }
}

fn map_pipeline_error(err: PipelineError) -> MainError {
    match err {
        PipelineError::PermissionDenied(name) => MainError::PermissionDenied(name),
        PipelineError::Recording(RecordingError::InsufficientDiskSpace { needed, available }) => {
            MainError::Io(format!(
                "insufficient disk space: need {needed} bytes, have {available}"
            ))
        },
        PipelineError::Recording(RecordingError::OutputExists { path }) => {
            MainError::InvalidArgs(format!("output already exists: {path}"))
        },
        PipelineError::Recording(RecordingError::ConfigError { msg }) => {
            MainError::InvalidArgs(msg)
        },
        PipelineError::Io(err) => MainError::Io(err.to_string()),
        PipelineError::Capture(err) => MainError::Capture(err.to_string()),
        PipelineError::Monitor(err) => MainError::Capture(err.to_string()),
        PipelineError::Transcription(err) => MainError::Internal(err.to_string()),
        PipelineError::Recording(RecordingError::Transcription(err)) => {
            MainError::Internal(err.to_string())
        },
        PipelineError::Codec(err) => MainError::Internal(err.to_string()),
        PipelineError::InvalidState(msg)
        | PipelineError::Recording(RecordingError::Internal { msg }) => MainError::Internal(msg),
        PipelineError::Recording(RecordingError::Capture(err)) => {
            MainError::Capture(err.to_string())
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_list_sources_without_output() {
        let args = RecordArgs::try_parse_from(["record", "--list-sources"]).expect("parse");
        assert!(args.list_sources);
        assert!(args.output.is_none());
    }

    #[test]
    fn parses_mic_no_transcribe() {
        let args = RecordArgs::try_parse_from([
            "record",
            "--source",
            "mic",
            "--no-transcribe",
            "-o",
            "test.ogg",
        ])
        .expect("parse");
        assert!(args.no_transcribe);
        assert_eq!(args.source, "mic");
    }

    #[test]
    fn prepare_rejects_noncanonical_rate() {
        let args = RecordArgs::try_parse_from([
            "record",
            "--source",
            "mic",
            "--sample-rate",
            "44100",
            "-o",
            "out.wav",
        ])
        .expect("parse");
        let err = prepare_session(&args).expect_err("rate");
        assert!(matches!(err, MainError::InvalidArgs(_)));
    }

    #[test]
    fn resolve_system_requires_target() {
        let args = RecordArgs::try_parse_from(["record", "--source", "system", "-o", "out.ogg"])
            .expect("parse");
        let err = resolve_source(&args).expect_err("need target");
        assert!(matches!(err, MainError::InvalidArgs(_)));
    }

    #[test]
    fn resolve_both_with_app_id() {
        let args = RecordArgs::try_parse_from([
            "record",
            "--source",
            "both",
            "--app-id",
            "us.zoom.xos",
            "-o",
            "out.ogg",
        ])
        .expect("parse");
        match resolve_source(&args).expect("source") {
            AudioSourceConfig::Both { bundle_id } => assert_eq!(bundle_id, "us.zoom.xos"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parse_duration_compound() {
        assert_eq!(parse_duration("2h30m").unwrap(), Duration::from_mins(150));
        assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("999999999h").is_err());
    }

    #[test]
    fn parse_byte_size_units() {
        assert_eq!(parse_byte_size("500M").unwrap(), 500 * 1024 * 1024);
        assert_eq!(parse_byte_size("2G").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_byte_size("1024").unwrap(), 1024);
        assert!(parse_byte_size("0K").is_err());
    }

    #[test]
    fn default_transcript_replaces_stem() {
        let path = default_transcript_path(Path::new("meeting.ogg"), TranscriptFormat::Srt);
        assert_eq!(path, PathBuf::from("meeting.srt"));
    }

    #[test]
    fn prepare_skips_transcript_when_no_transcribe() {
        let args = RecordArgs::try_parse_from([
            "record",
            "--source",
            "mic",
            "--no-transcribe",
            "-o",
            "voice.wav",
            "--format",
            "wav",
            "--duration",
            "30s",
        ])
        .expect("parse");
        let prepared = prepare_session(&args).expect("prepare");
        assert!(!prepared.config.transcribe);
        assert!(prepared.config.transcript_output_path.is_none());
        assert_eq!(prepared.max_duration, Some(Duration::from_secs(30)));
    }
}
