//! The command-line surface.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use koe_capture::Source;
use koe_record::Channels;

use crate::io::Io;

/// Record and transcribe audio offline.
#[derive(Debug, Parser)]
#[command(name = "koe", version, about, long_about = None)]
pub struct Cli {
    /// The operation to run.
    #[command(subcommand)]
    pub command: Command,
}

/// One operation.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// List capture devices.
    Devices,
    /// Record microphone and system audio into one file.
    Record(RecordArgs),
    /// Transcribe a 16-bit PCM WAV file.
    Transcribe(TranscribeArgs),
}

/// Arguments for `koe record`.
#[derive(Debug, Args)]
pub struct RecordArgs {
    /// Output file: `.ogg`, `.oga`, `.opus`, or `.wav`.
    pub output: PathBuf,
    /// What to capture.
    #[arg(long, value_enum, default_value_t = SourceChoice::Both)]
    pub source: SourceChoice,
    /// Stop after this many seconds.
    #[arg(long, value_parser = parse_seconds)]
    pub duration: Option<Duration>,
    /// Output sample rate in hertz.
    #[arg(long, default_value_t = 48_000)]
    pub rate: u32,
    /// Output channel layout.
    #[arg(long, value_enum, default_value_t = ChannelChoice::Stereo)]
    pub channels: ChannelChoice,
    /// Gain in decibels applied to each source.
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    pub gain: f32,
    /// GGUF speech model. Omit to use the platform's system model.
    #[arg(long, conflicts_with = "no_transcribe")]
    pub model: Option<PathBuf>,
    /// Transcript file. Defaults to the output path with a `.txt` extension.
    #[arg(long, conflicts_with = "no_transcribe")]
    pub transcript: Option<PathBuf>,
    /// BCP 47 language tag for transcription.
    #[arg(long, conflicts_with = "no_transcribe")]
    pub language: Option<String>,
    /// Print time ranges in the transcript file.
    #[arg(long, conflicts_with = "no_transcribe")]
    pub timestamps: bool,
    /// Record without transcription.
    #[arg(long)]
    pub no_transcribe: bool,
}

/// Arguments for `koe transcribe`.
#[derive(Debug, Args)]
pub struct TranscribeArgs {
    /// 16-bit PCM WAV file to transcribe.
    pub input: PathBuf,
    /// GGUF speech model. Omit to use the platform's system model.
    #[arg(long)]
    pub model: Option<PathBuf>,
    /// BCP 47 language tag, for example `ja`.
    #[arg(long)]
    pub language: Option<String>,
    /// Print each committed segment with its time range.
    #[arg(long)]
    pub timestamps: bool,
}

/// Capture source selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SourceChoice {
    /// Microphone and system audio.
    Both,
    /// Microphone only.
    Microphone,
    /// System audio only.
    System,
}

impl SourceChoice {
    /// The sources this choice records.
    #[must_use]
    pub const fn sources(self) -> &'static [Source] {
        match self {
            Self::Both => &[Source::Microphone, Source::SystemAudio],
            Self::Microphone => &[Source::Microphone],
            Self::System => &[Source::SystemAudio],
        }
    }
}

/// Output channel selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ChannelChoice {
    /// One channel.
    Mono,
    /// Two channels.
    Stereo,
}

impl ChannelChoice {
    /// The channel layout this choice names.
    #[must_use]
    pub const fn channels(self) -> Channels {
        match self {
            Self::Mono => Channels::Mono,
            Self::Stereo => Channels::Stereo,
        }
    }
}

fn parse_seconds(text: &str) -> Result<Duration, String> {
    let seconds: f64 = text
        .parse()
        .map_err(|_| format!("`{text}` is not a number of seconds"))?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(format!("`{text}` is not a positive duration"));
    }
    Duration::try_from_secs_f64(seconds).map_err(|_| format!("`{text}` is too long to represent"))
}

/// Reports a clap parse failure to the matching stream and returns its code.
pub fn report(error: &clap::Error, io: &mut Io<'_>) -> ExitCode {
    let rendered = error.render().to_string();
    let _ = if error.use_stderr() {
        io.err.write_all(rendered.as_bytes())
    } else {
        io.out.write_all(rendered.as_bytes())
    };
    ExitCode::from(exit_code(error))
}

fn exit_code(error: &clap::Error) -> u8 {
    if error.exit_code() == 0 { 0 } else { 2 }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use clap::error::ErrorKind;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("koe").chain(args.iter().copied()))
    }

    fn record(args: &[&str]) -> RecordArgs {
        let mut all = vec!["record"];
        all.extend_from_slice(args);
        match parse(&all).unwrap().command {
            Command::Record(args) => args,
            other => panic!("expected record, got {other:?}"),
        }
    }

    #[test]
    fn devices_parses_as_the_devices_command() {
        assert!(matches!(
            parse(&["devices"]).unwrap().command,
            Command::Devices
        ));
    }

    #[test]
    fn record_defaults_are_stereo_at_48_khz_from_both_sources() {
        let args = record(&["take.ogg"]);
        assert_eq!(args.output, PathBuf::from("take.ogg"));
        assert_eq!(args.source, SourceChoice::Both);
        assert_eq!(args.duration, None);
        assert_eq!(args.rate, 48_000);
        assert_eq!(args.channels, ChannelChoice::Stereo);
        assert_eq!(args.gain, 0.0);
        assert_eq!(args.model, None);
        assert_eq!(args.transcript, None);
        assert_eq!(args.language, None);
        assert!(!args.timestamps);
        assert!(!args.no_transcribe);
    }

    #[test]
    fn record_reads_every_option() {
        let args = record(&[
            "take.wav",
            "--source",
            "microphone",
            "--duration",
            "2.5",
            "--rate",
            "16000",
            "--channels",
            "mono",
            "--gain",
            "-3",
            "--model",
            "tiny.gguf",
            "--transcript",
            "take.txt",
            "--language",
            "ja",
            "--timestamps",
        ]);
        assert_eq!(args.source, SourceChoice::Microphone);
        assert_eq!(args.duration, Some(Duration::from_millis(2_500)));
        assert_eq!(args.rate, 16_000);
        assert_eq!(args.channels, ChannelChoice::Mono);
        assert_eq!(args.gain, -3.0);
        assert_eq!(args.model, Some(PathBuf::from("tiny.gguf")));
        assert_eq!(args.transcript, Some(PathBuf::from("take.txt")));
        assert_eq!(args.language.as_deref(), Some("ja"));
        assert!(args.timestamps);
        assert!(!args.no_transcribe);
    }

    #[test]
    fn record_without_transcription_parses() {
        let args = record(&["take.wav", "--no-transcribe"]);
        assert!(args.no_transcribe);
    }

    #[test]
    fn transcribe_reads_its_options() {
        let parsed = parse(&[
            "transcribe",
            "take.wav",
            "--model",
            "tiny.gguf",
            "--language",
            "ja",
            "--timestamps",
        ])
        .unwrap();
        let Command::Transcribe(args) = parsed.command else {
            panic!("expected transcribe");
        };
        assert_eq!(args.input, PathBuf::from("take.wav"));
        assert_eq!(args.model, Some(PathBuf::from("tiny.gguf")));
        assert_eq!(args.language.as_deref(), Some("ja"));
        assert!(args.timestamps);
    }

    #[test]
    fn every_source_choice_names_its_sources() {
        assert_eq!(
            SourceChoice::Both.sources(),
            [Source::Microphone, Source::SystemAudio]
        );
        assert_eq!(SourceChoice::Microphone.sources(), [Source::Microphone]);
        assert_eq!(SourceChoice::System.sources(), [Source::SystemAudio]);
    }

    #[test]
    fn every_channel_choice_names_its_layout() {
        assert_eq!(ChannelChoice::Mono.channels(), Channels::Mono);
        assert_eq!(ChannelChoice::Stereo.channels(), Channels::Stereo);
    }

    #[test]
    fn durations_are_positive_and_finite() {
        assert_eq!(parse_seconds("1"), Ok(Duration::from_secs(1)));
        assert_eq!(parse_seconds("0.5"), Ok(Duration::from_millis(500)));
        for bad in ["0", "-1", "abc", "inf", "NaN", "1e300"] {
            assert!(parse_seconds(bad).is_err(), "`{bad}` must be rejected");
        }
    }

    #[test]
    fn missing_and_unknown_subcommands_are_rejected() {
        assert_eq!(
            parse(&[]).unwrap_err().kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert_eq!(
            parse(&["speak"]).unwrap_err().kind(),
            ErrorKind::InvalidSubcommand
        );
    }

    #[test]
    fn a_record_without_an_output_is_rejected() {
        assert_eq!(
            parse(&["record"]).unwrap_err().kind(),
            ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn invalid_values_are_rejected() {
        for args in [
            vec!["record", "take.ogg", "--source", "speaker"],
            vec!["record", "take.ogg", "--channels", "surround"],
            vec!["record", "take.ogg", "--duration", "0"],
            vec!["record", "take.ogg", "--rate", "fast"],
            vec!["record", "take.ogg", "--gain", "loud"],
            vec!["record", "take.ogg", "--no-transcribe", "--model", "m.gguf"],
            vec!["transcribe"],
        ] {
            assert!(parse(&args).is_err(), "{args:?} must be rejected");
        }
    }

    #[test]
    fn help_and_version_exit_zero_on_stdout() {
        for args in [vec!["--help"], vec!["--version"]] {
            let error = parse(&args).unwrap_err();
            assert!(matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ));
            let mut out = Vec::new();
            let mut err = Vec::new();
            let mut io = Io {
                out: &mut out,
                err: &mut err,
            };
            assert_eq!(report(&error, &mut io), ExitCode::SUCCESS);
            assert!(err.is_empty());
            assert!(!out.is_empty());
        }
    }

    #[test]
    fn help_text_names_every_command() {
        let error = parse(&["--help"]).unwrap_err();
        let text = error.render().to_string();
        for command in ["devices", "record", "transcribe"] {
            assert!(text.contains(command), "help must name `{command}`");
        }
    }

    #[test]
    fn version_text_names_the_package_version() {
        let error = parse(&["--version"]).unwrap_err();
        assert!(
            error
                .render()
                .to_string()
                .contains(env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn parse_failures_exit_two_on_stderr() {
        let error = parse(&["speak"]).unwrap_err();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
        };
        assert_eq!(report(&error, &mut io), ExitCode::from(2));
        assert!(out.is_empty());
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("unrecognized subcommand")
        );
    }
}
