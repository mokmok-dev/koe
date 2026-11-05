//! The `transcribe` command.

use std::path::Path;

use koe_transcribe::{Model, Options, Transcript};

use crate::cli::TranscribeArgs;
use crate::error::AppError;
use crate::host::Host;
use crate::io::Io;
use crate::wav;

/// Transcribes the WAV file `args` names.
pub fn execute(args: &TranscribeArgs, host: &dyn Host, io: &mut Io<'_>) -> Result<(), AppError> {
    let audio = wav::read(&args.input)?;
    let model = match &args.model {
        Some(path) => model(path)?,
        None => Model::system_speech(),
    };
    let recognizer = host.recognizer(&model)?;
    let options = options(args.language.as_deref());
    match recognizer.transcribe(&audio.samples, audio.spec, &options) {
        Ok(transcript) => {
            render(&transcript, args.timestamps, io);
            Ok(())
        }
        Err(incomplete) => {
            render(incomplete.transcript(), args.timestamps, io);
            Err(AppError::Transcribe(incomplete.error().clone()))
        }
    }
}

fn model(path: &Path) -> Result<Model, AppError> {
    let text = path.to_str().ok_or_else(|| {
        AppError::usage(format!(
            "model path `{}` is not valid UTF-8",
            path.display()
        ))
    })?;
    Ok(Model::gguf(text))
}

fn options(language: Option<&str>) -> Options {
    language.map_or_else(Options::new, |language| Options::new().language(language))
}

fn render(transcript: &Transcript, timestamps: bool, io: &mut Io<'_>) {
    if timestamps {
        for segment in transcript.segments() {
            io.line(format_args!(
                "[{}..{}] {}",
                segment.start_ms(),
                segment.end_ms(),
                segment.text()
            ));
        }
    } else {
        io.line(format_args!("{}", transcript.text()));
    }
    if transcript.dropped_ms() > 0 {
        io.warning(format_args!(
            "dropped {} ms of audio",
            transcript.dropped_ms()
        ));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use std::path::PathBuf;
    use std::sync::Arc;

    use koe_transcribe::{Incomplete, Segment, Transcript};

    use super::*;
    use crate::fakes::{FakeHost, FakeRecognizer};

    fn args(input: PathBuf) -> TranscribeArgs {
        TranscribeArgs {
            input,
            model: Some(PathBuf::from("tiny.gguf")),
            language: None,
            timestamps: false,
        }
    }

    fn wav_file(dir: &Path) -> PathBuf {
        let samples = [0_i16, 16_384, -16_384, 0];
        let mut data = Vec::new();
        for sample in samples {
            data.extend_from_slice(&sample.to_le_bytes());
        }
        let mut format = Vec::new();
        format.extend_from_slice(&1_u16.to_le_bytes());
        format.extend_from_slice(&1_u16.to_le_bytes());
        format.extend_from_slice(&16_000_u32.to_le_bytes());
        format.extend_from_slice(&32_000_u32.to_le_bytes());
        format.extend_from_slice(&2_u16.to_le_bytes());
        format.extend_from_slice(&16_u16.to_le_bytes());

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(4_u32 + 24 + 16).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&format);
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(&data);

        let path = dir.join("take.wav");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn run(args: &TranscribeArgs, host: &FakeHost) -> (Result<(), AppError>, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
        };
        let result = execute(args, host, &mut io);
        (
            result,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn transcript() -> Transcript {
        Transcript::new(
            vec![
                Segment::new(0, 100, "hello"),
                Segment::new(100, 200, "world"),
            ],
            0,
        )
    }

    #[test]
    fn a_transcript_is_printed_as_joined_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = wav_file(dir.path());
        let recognizer = FakeRecognizer::new(Ok(transcript()));
        let calls = Arc::clone(&recognizer.calls);
        let host = FakeHost::default().with_recognizer(Box::new(recognizer));
        let (result, out, err) = run(&args(path), &host);
        assert!(result.is_ok());
        assert_eq!(out, "hello world\n");
        assert!(err.is_empty());
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].pcm, [0.0, 0.5, -0.5, 0.0]);
        assert_eq!(
            calls[0].spec,
            koe_transcribe::AudioSpec::new(16_000, 1).unwrap()
        );
        assert_eq!(calls[0].language, None);
    }

    #[test]
    fn timestamps_print_one_line_per_segment() {
        let dir = tempfile::tempdir().unwrap();
        let path = wav_file(dir.path());
        let host =
            FakeHost::default().with_recognizer(Box::new(FakeRecognizer::new(Ok(transcript()))));
        let mut args = args(path);
        args.timestamps = true;
        let (result, out, err) = run(&args, &host);
        assert!(result.is_ok());
        assert_eq!(out, "[0..100] hello\n[100..200] world\n");
        assert!(err.is_empty());
    }

    #[test]
    fn a_language_tag_reaches_the_recognizer() {
        let dir = tempfile::tempdir().unwrap();
        let path = wav_file(dir.path());
        let recognizer = FakeRecognizer::new(Ok(transcript()));
        let calls = Arc::clone(&recognizer.calls);
        let host = FakeHost::default().with_recognizer(Box::new(recognizer));
        let mut args = args(path);
        args.language = Some("ja".to_owned());
        let (result, _, _) = run(&args, &host);
        assert!(result.is_ok());
        assert_eq!(calls.lock().unwrap()[0].language.as_deref(), Some("ja"));
    }

    #[test]
    fn an_empty_transcript_prints_a_blank_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = wav_file(dir.path());
        let host = FakeHost::default()
            .with_recognizer(Box::new(FakeRecognizer::new(Ok(Transcript::default()))));
        let (result, out, err) = run(&args(path), &host);
        assert!(result.is_ok());
        assert_eq!(out, "\n");
        assert!(err.is_empty());
    }

    #[test]
    fn dropped_audio_is_reported_as_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = wav_file(dir.path());
        let dropped = Transcript::new(vec![Segment::new(0, 100, "kept")], 50);
        let host = FakeHost::default().with_recognizer(Box::new(FakeRecognizer::new(Ok(dropped))));
        let (result, out, err) = run(&args(path), &host);
        assert!(result.is_ok());
        assert_eq!(out, "kept\n");
        assert_eq!(err, "warning: dropped 50 ms of audio\n");
    }

    #[test]
    fn an_incomplete_result_keeps_the_committed_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = wav_file(dir.path());
        let incomplete = Incomplete::new(
            koe_transcribe::Error::Unsupported,
            Transcript::new(vec![Segment::new(0, 100, "kept")], 0),
        );
        let host =
            FakeHost::default().with_recognizer(Box::new(FakeRecognizer::new(Err(incomplete))));
        let (result, out, err) = run(&args(path), &host);
        assert!(matches!(
            result,
            Err(AppError::Transcribe(koe_transcribe::Error::Unsupported))
        ));
        assert_eq!(out, "kept\n");
        assert!(err.is_empty());
    }

    #[test]
    fn a_recognizer_creation_failure_is_returned() {
        let dir = tempfile::tempdir().unwrap();
        let path = wav_file(dir.path());
        let host = FakeHost::default().with_recognizer_error(AppError::Transcribe(
            koe_transcribe::Error::ModelNotFound {
                path: "tiny.gguf".into(),
            },
        ));
        let (result, out, err) = run(&args(path), &host);
        assert!(matches!(
            result,
            Err(AppError::Transcribe(
                koe_transcribe::Error::ModelNotFound { .. }
            ))
        ));
        assert!(out.is_empty());
        assert!(err.is_empty());
    }

    #[test]
    fn a_missing_input_is_a_wav_error() {
        let host = FakeHost::default();
        let (result, out, err) = run(&args(PathBuf::from("/nonexistent/take.wav")), &host);
        assert!(matches!(
            result,
            Err(AppError::Wav(crate::wav::Error::Io { .. }))
        ));
        assert!(out.is_empty());
        assert!(err.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_model_path_is_a_usage_error() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = tempfile::tempdir().unwrap();
        let path = wav_file(dir.path());
        let mut args = args(path);
        args.model = Some(PathBuf::from(OsString::from_vec(vec![0xFF])));
        let host = FakeHost::default();
        let (result, _, _) = run(&args, &host);
        let error = result.unwrap_err();
        assert!(matches!(&error, AppError::Usage(_)));
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn an_omitted_model_uses_the_system_speech_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = wav_file(dir.path());
        let host =
            FakeHost::default().with_recognizer(Box::new(FakeRecognizer::new(Ok(transcript()))));
        let mut args = args(path);
        args.model = None;
        let (result, _, _) = run(&args, &host);
        assert!(result.is_ok());
        assert_eq!(
            host.models_seen.lock().unwrap().as_slice(),
            [Model::system_speech()]
        );
    }

    #[test]
    fn a_model_path_reaches_the_host_as_a_gguf_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = wav_file(dir.path());
        let host =
            FakeHost::default().with_recognizer(Box::new(FakeRecognizer::new(Ok(transcript()))));
        let (result, _, _) = run(&args(path), &host);
        assert!(result.is_ok());
        assert_eq!(
            host.models_seen.lock().unwrap().as_slice(),
            [Model::gguf("tiny.gguf")]
        );
    }

    #[test]
    fn a_model_path_becomes_a_gguf_model() {
        assert_eq!(
            model(Path::new("/models/tiny.gguf")).unwrap(),
            Model::gguf("/models/tiny.gguf")
        );
    }

    #[test]
    fn options_keep_a_requested_language() {
        assert_eq!(options(None).language_tag(), None);
        assert_eq!(options(Some("pt-BR")).language_tag(), Some("pt-BR"));
    }
}
