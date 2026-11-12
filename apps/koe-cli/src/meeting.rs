//! The `meeting` command.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use koe_capture::Source;
use koe_session_store::{Error as StoreError, Meeting, RECORDING_FILE, Store, Transcripts};
use koe_transcribe::Transcript;

use crate::cli::MeetingArgs;
use crate::error::AppError;
use crate::host::Host;
use crate::io::Io;
use crate::record::{self, RecordPlan};

/// Sessions directory a run without `--root` uses.
const DEFAULT_ROOT: &str = "recordings";

/// Records `args` into a new meeting session and stores its transcript.
pub fn execute(
    args: &MeetingArgs,
    host: &dyn Host,
    interrupt: &Arc<AtomicBool>,
    io: &mut Io<'_>,
) -> Result<(), AppError> {
    // The options are validated before the store, so a rejected flag creates
    // no directory.
    let plan = RecordPlan::for_meeting(args)?;
    let store = Store::open(root(args))?;
    let mut meeting = store.create(SystemTime::now())?;
    let output = match meeting.set_recording(RECORDING_FILE) {
        Ok(output) => output,
        Err(error) => {
            discard(&store, &meeting, io);
            return Err(error.into());
        }
    };
    let plan = plan.with_output(output.clone());

    let mut recorded = Vec::new();
    let outcome = record::pump_plan(&plan, host, interrupt, args.capture.duration, &mut recorded);
    // The text is committed whether or not the recorder closes its file.
    let saved = meeting.save_transcripts(&transcripts_of(recorded));
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            if output.is_file() {
                warn_save(saved, io);
                io.line(format_args!("session: {}", meeting.directory().display()));
            } else {
                // A take whose recorder never opened a file leaves nothing to
                // keep.
                discard(&store, &meeting, io);
            }
            return Err(error);
        }
    };
    record::report(&outcome, io);
    if outcome.summary.frames == 0 {
        // An empty take is not a meeting; the desktop discards its session too.
        if discard(&store, &meeting, io) {
            io.line(format_args!("session: removed, no audio was written"));
        }
        return outcome.failure.map_or(Ok(()), Err);
    }
    io.line(format_args!(
        "recorded {} frames over {:.3} s to {}",
        outcome.summary.frames,
        outcome.summary.duration.as_secs_f64(),
        output.display()
    ));
    io.line(format_args!("session: {}", meeting.directory().display()));
    match outcome.failure {
        Some(error) => {
            warn_save(saved, io);
            Err(error)
        }
        None => save_result(saved, io),
    }
}

/// Warns about a save that fell short and returns the failure that must stop
/// the command.
///
/// Only an unwritten record stops it: the record holds the audio name and the
/// transcripts, while `transcript.txt` is rendered from the record and can be
/// rendered from it again, so a missing text file leaves the take readable.
fn save_result(saved: Result<(), StoreError>, io: &mut Io<'_>) -> Result<(), AppError> {
    match saved {
        Ok(()) => Ok(()),
        Err(StoreError::TextNotWritten { source }) => {
            io.warning(format_args!(
                "the transcript text file was not written: {source}"
            ));
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Warns about a save failure whose take is reported by its own error.
fn warn_save(saved: Result<(), StoreError>, io: &mut Io<'_>) {
    if let Err(error) = save_result(saved, io) {
        io.warning(format_args!("the transcript was not saved: {error}"));
    }
}

/// Removes the session of a take that wrote nothing and reports whether it is
/// gone.
///
/// A failed removal warns with the directory, so a caller that reports the
/// removal must not claim it happened and the user can still find the session.
fn discard(store: &Store, meeting: &Meeting, io: &mut Io<'_>) -> bool {
    match store.discard(meeting.id()) {
        Ok(()) => true,
        Err(error) => {
            io.warning(format_args!(
                "the session at {} was not removed: {error}",
                meeting.directory().display()
            ));
            false
        }
    }
}

/// The directory that holds meeting sessions.
fn root(args: &MeetingArgs) -> PathBuf {
    args.root
        .clone()
        .or_else(|| std::env::var_os("KOE_RECORDINGS_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT))
}

fn transcripts_of(recorded: Vec<(Source, Transcript)>) -> Transcripts {
    let mut transcripts = Transcripts::new();
    for (source, transcript) in recorded {
        transcripts.set(source, Some(transcript));
    }
    transcripts
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::fs;
    use std::path::Path;

    use koe_session_store::Meeting;
    use koe_transcribe::{Event, Segment};

    use super::*;
    use crate::cli::{CaptureArgs, ChannelChoice, SourceChoice};
    use crate::fakes::{FakeHost, FakeLiveRecognizer, FakeLiveTranscriber, FakeStream};

    fn args(root: &Path) -> MeetingArgs {
        MeetingArgs {
            root: Some(root.to_path_buf()),
            capture: CaptureArgs {
                source: SourceChoice::Microphone,
                duration: None,
                rate: 48_000,
                channels: ChannelChoice::Mono,
                gain: 0.0,
                model: None,
                language: None,
                no_transcribe: false,
            },
        }
    }

    fn interrupt() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    fn recorded_transcript() -> Transcript {
        Transcript::new(
            vec![
                Segment::new(0, 100, "hello"),
                Segment::new(100, 200, "world"),
            ],
            0,
        )
    }

    fn transcribing_host() -> FakeHost {
        let event = Event::Committed {
            text: "hello".into(),
            start_ms: 0,
            end_ms: 100,
        };
        let session = FakeLiveRecognizer::new(Ok(recorded_transcript())).with_events(vec![event]);
        let host = FakeHost::default()
            .with_sink()
            .with_stream(
                Source::Microphone,
                FakeStream::new(1, vec![Ok(vec![0.25; 960]), Ok(Vec::new())]),
            )
            .with_live_transcriber(Box::new(FakeLiveTranscriber::new(vec![session])));
        {
            let mut sink = host.sink.lock().unwrap();
            sink.create_file = true;
            sink.summary.frames = 960;
        }
        host
    }

    fn run(args: &MeetingArgs, host: &FakeHost) -> (Result<(), AppError>, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
        };
        let result = execute(args, host, &interrupt(), &mut io);
        (
            result,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn only_meeting(store: &Store) -> Meeting {
        let mut ids = store.list().unwrap();
        assert_eq!(ids.len(), 1);
        store.load(ids.remove(0)).unwrap().unwrap()
    }

    #[test]
    fn a_meeting_writes_audio_and_transcript_into_its_session() {
        let dir = tempfile::tempdir().unwrap();
        let args = args(dir.path());
        let (result, out, _) = run(&args, &transcribing_host());
        result.unwrap();

        let store = Store::open(dir.path()).unwrap();
        let meeting = only_meeting(&store);
        let session = meeting.directory();
        assert_eq!(
            session.file_name().unwrap().to_str().unwrap(),
            meeting.id().to_string()
        );
        assert_eq!(meeting.recording(), Some(session.join(RECORDING_FILE)));
        assert!(session.join(RECORDING_FILE).is_file());
        assert!(session.join("session").is_file());
        assert_eq!(
            fs::read_to_string(session.join("transcript.txt")).unwrap(),
            "[0..100] microphone: hello\n[100..200] microphone: world\n"
        );
        assert_eq!(
            meeting.transcripts().get(Source::Microphone),
            Some(&recorded_transcript())
        );
        assert!(out.contains(&format!("session: {}", session.display())));
        assert!(out.contains(RECORDING_FILE));
    }

    #[test]
    fn a_meeting_without_transcription_saves_an_empty_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::default().with_sink().with_stream(
            Source::Microphone,
            FakeStream::new(1, vec![Ok(vec![0.25; 960]), Ok(Vec::new())]),
        );
        {
            let mut sink = host.sink.lock().unwrap();
            sink.create_file = true;
            sink.summary.frames = 960;
        }
        let mut args = args(dir.path());
        args.capture.no_transcribe = true;
        let (result, _, _) = run(&args, &host);
        result.unwrap();

        let store = Store::open(dir.path()).unwrap();
        let meeting = only_meeting(&store);
        assert!(meeting.transcripts().is_empty());
        assert_eq!(
            fs::read_to_string(meeting.directory().join("transcript.txt")).unwrap(),
            ""
        );
    }

    #[test]
    fn a_failed_take_keeps_the_text_it_transcribed() {
        let dir = tempfile::tempdir().unwrap();
        let host = transcribing_host();
        host.sink.lock().unwrap().fail_write = true;
        let (result, _, _) = run(&args(dir.path()), &host);
        assert_eq!(result.unwrap_err().to_string(), "write failed");

        let store = Store::open(dir.path()).unwrap();
        let meeting = only_meeting(&store);
        assert_eq!(
            fs::read_to_string(meeting.directory().join("transcript.txt")).unwrap(),
            "[0..100] microphone: hello\n[100..200] microphone: world\n"
        );
    }

    #[test]
    fn a_take_the_recorder_could_not_close_keeps_its_session() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::default().with_sink().with_stream(
            Source::Microphone,
            FakeStream::new(1, vec![Ok(vec![0.25; 960]), Ok(Vec::new())]),
        );
        {
            let mut sink = host.sink.lock().unwrap();
            sink.create_file = true;
            sink.fail_finish = true;
        }
        let mut args = args(dir.path());
        args.capture.no_transcribe = true;
        let (result, out, _) = run(&args, &host);
        assert_eq!(result.unwrap_err().to_string(), "finish failed");

        let store = Store::open(dir.path()).unwrap();
        let meeting = only_meeting(&store);
        assert!(out.contains(&format!("session: {}", meeting.directory().display())));
        assert!(meeting.directory().join("session").is_file());
    }

    #[test]
    fn a_take_whose_recorder_never_opened_a_file_leaves_no_session() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::default().with_stream(
            Source::Microphone,
            FakeStream::new(1, vec![Ok(vec![0.25; 960]), Ok(Vec::new())]),
        );
        let mut args = args(dir.path());
        args.capture.no_transcribe = true;
        let (result, out, _) = run(&args, &host);
        assert_eq!(result.unwrap_err().to_string(), "no scripted recorder");

        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.list().unwrap(), []);
        assert!(out.is_empty());
    }

    #[test]
    fn an_empty_take_leaves_no_session() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::default()
            .with_sink()
            .with_stream(Source::Microphone, FakeStream::new(1, vec![Ok(Vec::new())]));
        host.sink.lock().unwrap().create_file = true;
        let mut args = args(dir.path());
        args.capture.no_transcribe = true;
        let (result, out, _) = run(&args, &host);
        result.unwrap();

        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.list().unwrap(), []);
        assert!(out.contains("session: removed, no audio was written"));
        assert!(!out.contains("recorded 0 frames"));
    }

    #[test]
    fn a_record_that_cannot_be_written_fails_the_command() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
        };
        let saved = save_result(
            Err(StoreError::Io {
                operation: "write",
                kind: std::io::ErrorKind::PermissionDenied,
            }),
            &mut io,
        );
        assert!(matches!(saved, Err(AppError::Store(_))));
        assert!(out.is_empty());
        assert!(err.is_empty());
    }

    #[test]
    fn missing_transcript_text_warns_without_failing_the_command() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
        };
        let saved = save_result(
            Err(StoreError::TextNotWritten {
                source: Box::new(StoreError::Io {
                    operation: "write",
                    kind: std::io::ErrorKind::PermissionDenied,
                }),
            }),
            &mut io,
        );
        assert!(saved.is_ok());
        assert!(out.is_empty());
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: the transcript text file was not written: session store `write` failed: \
             permission denied\n"
        );
    }

    #[test]
    fn a_save_failure_for_a_failed_take_warns() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
        };
        warn_save(
            Err(StoreError::Io {
                operation: "write",
                kind: std::io::ErrorKind::PermissionDenied,
            }),
            &mut io,
        );
        assert!(out.is_empty());
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: the transcript was not saved: meeting session: session store `write` failed: \
             permission denied\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_session_that_cannot_be_removed_is_reported() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("meetings");
        let store = Store::open(&root).unwrap();
        let meeting = store.create(SystemTime::now()).unwrap();

        let original = fs::metadata(&root).unwrap().permissions().mode();
        let mut read_only = fs::metadata(&root).unwrap().permissions();
        read_only.set_mode(0o555);
        fs::set_permissions(&root, read_only).unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
        };
        let removed = discard(&store, &meeting, &mut io);

        let mut restored = fs::metadata(&root).unwrap().permissions();
        restored.set_mode(original);
        fs::set_permissions(&root, restored).unwrap();

        assert!(!removed);
        assert!(out.is_empty());
        let warning = String::from_utf8(err).unwrap();
        assert!(warning.contains("the session at"));
        assert!(warning.contains(&meeting.directory().display().to_string()));
    }

    #[test]
    fn a_rejected_plan_leaves_no_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut args = args(dir.path());
        args.capture.rate = 1_000;
        let (result, out, _) = run(&args, &FakeHost::default());
        assert_eq!(result.unwrap_err().exit_code(), 2);

        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn an_explicit_root_names_the_sessions_directory() {
        assert_eq!(
            root(&args(Path::new("meetings"))),
            PathBuf::from("meetings")
        );
    }
}
