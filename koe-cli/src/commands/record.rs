//! `koe record` — capture, encode, and optionally transcribe.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Parser;
use koe_core::{
    AudioSourceConfig, OutputFormat, PipelineConfig, PipelineError, RecordingError,
    RecordingPipeline, RecordingSummary, StopResult, TranscriptFormat, enumerate_apps,
    native_provider_registered,
};

use super::Run;
use super::apps_table::{format_apps_table, prepare_apps};
use super::duration::parse_duration;
use crate::MainError;
use crate::config::{self, KoeConfig, builtin};
use crate::signals::{SignalEvent, SignalListener, StopSignal};

/// Canonical capture / encode rate (pipeline is fixed to this today).
const CANONICAL_SAMPLE_RATE_HZ: u32 = builtin::SAMPLE_RATE_HZ;
/// Canonical channel count (stereo interleaved).
const CANONICAL_CHANNELS: u8 = builtin::CHANNELS;
/// Peak level below this is treated as silence for `--silence-timeout`.
const SILENCE_PEAK_THRESHOLD: f32 = 0.01;

/// Start a recording session with optional live transcription.
///
/// Overridable fields are `Option` so config file values can fill gaps
/// (CLI > config > built-in). `--no-*` bools force-disable when set.
#[derive(Debug, Parser)]
#[allow(clippy::struct_excessive_bools)]
pub struct RecordArgs {
    /// Audio source: `system`, `mic`, or `both` (default: system).
    #[arg(long)]
    pub source: Option<String>,

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
    #[arg(long)]
    pub sample_rate: Option<u32>,

    /// Output channel count (only `2` is supported).
    #[arg(long)]
    pub channels: Option<u8>,

    /// Disable acoustic echo cancellation for `--source both`.
    #[arg(long)]
    pub no_aec: bool,

    /// Disable comfort noise in AEC output.
    #[arg(long)]
    pub no_comfort_noise: bool,

    /// Speech recognition locale (BCP-47).
    #[arg(long)]
    pub locale: Option<String>,

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
    #[arg(long)]
    pub format: Option<String>,

    /// Transcript format: `txt`, `srt`, `vtt`, or `json`.
    #[arg(long)]
    pub transcript_format: Option<String>,

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
    /// SIGINT / SIGTERM. `force` is set when the double-tap window already fired.
    Interrupted {
        force: bool,
    },
}

impl Run for RecordArgs {
    fn run(self, config: &KoeConfig) -> Result<(), MainError> {
        if self.list_sources {
            return list_sources();
        }
        if self.list_locales {
            list_locales();
            return Ok(());
        }

        let prepared = prepare_session(&self, config)?;
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

fn prepare_session(
    args: &RecordArgs,
    file: &KoeConfig,
) -> Result<PreparedSession, MainError> {
    if args.display.is_some() {
        return Err(MainError::InvalidArgs(
            "--display is not supported yet (no display capture source in the pipeline)".into(),
        ));
    }

    let merged = merge_record_options(args, file)?;
    let output = args.output.clone().ok_or_else(|| {
        MainError::InvalidArgs("--output is required unless listing sources/locales".into())
    })?;
    let output = config::resolve_under_output_dir(&output, file);
    let source = resolve_source(args, &merged.source)?;
    let audio_format = parse_audio_format(&merged.format)?;
    let transcript_format = parse_transcript_format(&merged.transcript_format)?;

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
            || args.transcript_format.is_some()
            || args.locale.is_some())
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

    Ok(PreparedSession {
        config: PipelineConfig {
            source,
            output_path: output,
            transcript_output_path,
            locale: merged.locale,
            audio_format,
            transcript_format,
            enable_aec: merged.enable_aec,
            comfort_noise: merged.comfort_noise,
            monitor: args.monitor,
            transcribe,
            estimated_duration_hours: max_duration.map(|d| d.as_secs_f64() / 3600.0),
        },
        max_duration,
        max_bytes,
        silence_timeout,
    })
}

struct MergedRecordOptions {
    source: String,
    format: String,
    transcript_format: String,
    locale: String,
    enable_aec: bool,
    comfort_noise: bool,
}

fn merge_record_options(
    args: &RecordArgs,
    file: &KoeConfig,
) -> Result<MergedRecordOptions, MainError> {
    let sample_rate = config::coalesce_copy(
        args.sample_rate,
        file.defaults.sample_rate,
        builtin::SAMPLE_RATE_HZ,
    );
    let channels = config::coalesce_copy(
        args.channels,
        file.defaults.channels,
        builtin::CHANNELS,
    );
    if sample_rate != CANONICAL_SAMPLE_RATE_HZ {
        return Err(MainError::InvalidArgs(format!(
            "--sample-rate must be {CANONICAL_SAMPLE_RATE_HZ} (canonical pipeline rate)"
        )));
    }
    if channels != CANONICAL_CHANNELS {
        return Err(MainError::InvalidArgs(format!(
            "--channels must be {CANONICAL_CHANNELS} (canonical stereo pipeline)"
        )));
    }

    Ok(MergedRecordOptions {
        source: config::coalesce_owned(
            args.source.clone(),
            file.defaults.source.as_deref(),
            builtin::SOURCE,
        ),
        format: config::coalesce_owned(
            args.format.clone(),
            file.defaults.format.as_deref(),
            builtin::FORMAT,
        ),
        transcript_format: config::coalesce_owned(
            args.transcript_format.clone(),
            file.defaults.transcript_format.as_deref(),
            builtin::TRANSCRIPT_FORMAT,
        ),
        locale: config::coalesce_owned(
            args.locale.clone(),
            file.defaults.locale.as_deref(),
            builtin::LOCALE,
        ),
        enable_aec: if args.no_aec {
            false
        } else {
            file.aec.enabled.unwrap_or(builtin::AEC_ENABLED)
        },
        comfort_noise: if args.no_comfort_noise {
            false
        } else {
            file.aec
                .comfort_noise
                .unwrap_or(builtin::COMFORT_NOISE)
        },
    })
}

fn resolve_source(
    args: &RecordArgs,
    source_name: &str,
) -> Result<AudioSourceConfig, MainError> {
    if args.app_id.is_some() && args.pid.is_some() {
        return Err(MainError::InvalidArgs(
            "--app-id and --pid are mutually exclusive".into(),
        ));
    }

    let source = source_name.trim().to_ascii_lowercase();
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
    let locales = koe_core::supported_speech_locales();
    if locales.is_empty() {
        println!("Supported locales: (none reported on this host)");
        return;
    }
    println!("Supported locales:");
    for locale in locales {
        println!("  {locale}");
    }
}

async fn run_recording(prepared: PreparedSession) -> Result<(), MainError> {
    let output_path = prepared.config.output_path.clone();
    let transcript_path = prepared.config.transcript_output_path.clone();

    let mut pipeline = RecordingPipeline::start(prepared.config)
        .await
        .map_err(map_pipeline_error)?;

    eprintln!("Recording → {}", output_path.display());

    let mut signals =
        SignalListener::install().map_err(MainError::Internal)?;

    let stop_reason = wait_until_done(
        &mut pipeline,
        &mut signals,
        prepared.max_duration,
        prepared.max_bytes,
        prepared.silence_timeout,
    )
    .await?;

    let summary = match stop_reason {
        StopReason::Interrupted { force } => {
            stop_after_interrupt(&mut pipeline, &mut signals, force).await?
        },
        _ => pipeline.stop().await.map_err(map_pipeline_error)?,
    };

    print_summary(&summary, &output_path, transcript_path.as_deref());

    if matches!(stop_reason, StopReason::Interrupted { .. }) {
        return Err(MainError::Interrupted);
    }
    Ok(())
}

/// Stop after SIGINT/SIGTERM: prefer `force_stop` when the wait loop already
/// saw a double-tap or a second interrupt is queued; otherwise graceful `stop`
/// with a hard-exit watchdog for in-flight double-tap.
async fn stop_after_interrupt(
    pipeline: &mut RecordingPipeline,
    signals: &mut SignalListener,
    force: bool,
) -> Result<StopResult, MainError> {
    let force = force || second_interrupt_pending(signals).await;
    if force {
        eprintln!("Force stop — skipping transcript finalize…");
        return pipeline.force_stop().await.map_err(map_pipeline_error);
    }

    crate::signals::spawn_force_exit_watchdog(signals.interrupt_flag());
    pipeline.stop().await.map_err(map_pipeline_error)
}

/// Non-blocking poll: `true` when a second interrupt is already queued.
async fn second_interrupt_pending(signals: &mut SignalListener) -> bool {
    tokio::select! {
        biased;
        () = signals.recv_force_during_stop() => true,
        () = std::future::ready(()) => false,
    }
}

async fn wait_until_done(
    pipeline: &mut RecordingPipeline,
    signals: &mut SignalListener,
    max_duration: Option<Duration>,
    max_bytes: Option<u64>,
    silence_timeout: Option<Duration>,
) -> Result<StopReason, MainError> {
    let mut progress = pipeline.subscribe_progress();
    let deadline = max_duration.map(|d| Instant::now() + d);
    let mut last_sound = Instant::now();
    // Poll silence even when no meter updates arrive.
    let silence_tick = Duration::from_millis(200);

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
            event = signals.recv() => {
                match event {
                    SignalEvent::Stop(kind) => {
                        let force = kind == StopSignal::Force;
                        if force {
                            eprintln!("Force interrupt — stopping immediately…");
                        } else {
                            eprintln!("Interrupted — finishing recording…");
                        }
                        return Ok(StopReason::Interrupted { force });
                    }
                    SignalEvent::TogglePause => {
                        if pipeline.is_paused() {
                            pipeline.resume();
                            eprintln!("Resumed");
                        } else {
                            pipeline.pause();
                            eprintln!("Paused");
                        }
                    }
                }
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
        assert_eq!(args.source.as_deref(), Some("mic"));
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
        let err = prepare_session(&args, &KoeConfig::default()).expect_err("rate");
        assert!(matches!(err, MainError::InvalidArgs(_)));
    }

    #[test]
    fn resolve_system_requires_target() {
        let args = RecordArgs::try_parse_from(["record", "--source", "system", "-o", "out.ogg"])
            .expect("parse");
        let err = resolve_source(&args, "system").expect_err("need target");
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
        match resolve_source(&args, "both").expect("source") {
            AudioSourceConfig::Both { bundle_id } => assert_eq!(bundle_id, "us.zoom.xos"),
            other => panic!("unexpected {other:?}"),
        }
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
        let prepared = prepare_session(&args, &KoeConfig::default()).expect("prepare");
        assert!(!prepared.config.transcribe);
        assert!(prepared.config.transcript_output_path.is_none());
        assert_eq!(prepared.max_duration, Some(Duration::from_secs(30)));
    }

    #[test]
    fn prepare_inherits_config_defaults() {
        let args = RecordArgs::try_parse_from([
            "record",
            "--source",
            "mic",
            "-o",
            "clip.flac",
        ])
        .expect("parse");
        let file = config::parse_toml(
            r#"
[defaults]
format = "flac"
locale = "ja-JP"
transcript-format = "srt"
[aec]
enabled = false
comfort-noise = false
"#,
        )
        .expect("config");
        let prepared = prepare_session(&args, &file).expect("prepare");
        assert!(matches!(
            prepared.config.audio_format,
            OutputFormat::Flac { .. }
        ));
        assert_eq!(prepared.config.locale, "ja-JP");
        assert_eq!(prepared.config.transcript_format, TranscriptFormat::Srt);
        assert!(!prepared.config.enable_aec);
        assert!(!prepared.config.comfort_noise);
    }

    #[test]
    fn prepare_cli_overrides_config() {
        let args = RecordArgs::try_parse_from([
            "record",
            "--source",
            "mic",
            "--format",
            "wav",
            "--locale",
            "en-US",
            "--no-aec",
            "-o",
            "out.wav",
        ])
        .expect("parse");
        let file = config::parse_toml(
            r#"
[defaults]
format = "flac"
locale = "ja-JP"
[aec]
enabled = true
"#,
        )
        .expect("config");
        let prepared = prepare_session(&args, &file).expect("prepare");
        assert!(matches!(
            prepared.config.audio_format,
            OutputFormat::Wav { .. }
        ));
        assert_eq!(prepared.config.locale, "en-US");
        assert!(!prepared.config.enable_aec);
    }

    #[test]
    fn prepare_resolves_relative_output_via_config_dir() {
        let args = RecordArgs::try_parse_from(["record", "--source", "mic", "-o", "meet.ogg"])
            .expect("parse");
        let mut file = KoeConfig::default();
        file.output.directory = Some("/tmp/koe-recs".into());
        let prepared = prepare_session(&args, &file).expect("prepare");
        assert_eq!(
            prepared.config.output_path,
            PathBuf::from("/tmp/koe-recs/meet.ogg")
        );
    }
}
