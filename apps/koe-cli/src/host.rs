//! The seam between the commands and the platform crates.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use koe_capture::{CaptureStream, DeviceInfo, Source, StreamSpec};
use koe_record::{Recorder, RecordingSpec, Summary};
use koe_transcribe::{
    AudioSpec, Event, Incomplete, LiveSession, Model, Options, Transcriber, Transcript,
};

use crate::error::AppError;

/// A capture stream the record pump reads.
pub trait BlockStream {
    /// Reads whole frames into `out`.
    ///
    /// A return of zero means the stream ended.
    ///
    /// # Errors
    ///
    /// Returns the capture backend's error when the read fails.
    fn read(&mut self, out: &mut [f32]) -> Result<usize, AppError>;
}

/// A recorder the record pump writes into.
pub trait TrackSink: Send {
    /// Registers a source and the format its stream delivers.
    ///
    /// # Errors
    ///
    /// Returns the recorder's error when the source cannot be registered.
    fn add_source(&mut self, source: Source, spec: StreamSpec) -> Result<(), AppError>;

    /// Sets the gain mixed in from one source.
    ///
    /// # Errors
    ///
    /// Returns the recorder's error when the gain is outside its range.
    fn set_gain(&mut self, source: Source, gain: f32) -> Result<(), AppError>;

    /// Appends interleaved samples from `source`.
    ///
    /// # Errors
    ///
    /// Returns the recorder's error when the samples cannot be mixed.
    fn write(&mut self, source: Source, samples: &[f32]) -> Result<(), AppError>;

    /// Stops mixing `source`.
    ///
    /// # Errors
    ///
    /// Returns the recorder's error when the source cannot be ended.
    fn end(&mut self, source: Source) -> Result<(), AppError>;

    /// Completes the file and reports what it holds.
    ///
    /// # Errors
    ///
    /// Returns the recorder's error when the file cannot be completed.
    fn finish(&mut self) -> Result<Summary, AppError>;
}

/// A speech recognizer the transcribe command drives.
pub trait Recognizer: Send {
    /// Transcribes a complete buffer.
    ///
    /// # Errors
    ///
    /// Returns [`Incomplete`] with the text committed before a backend failure.
    fn transcribe(
        &self,
        pcm: &[f32],
        spec: AudioSpec,
        options: &Options,
    ) -> Result<Transcript, Incomplete>;
}

/// A streaming recognizer fed with capture blocks.
pub trait LiveRecognizer: Send {
    /// Appends interleaved samples in the capture format.
    ///
    /// # Errors
    ///
    /// Returns the engine's error when the audio cannot be queued.
    fn feed(&self, samples: &[f32]) -> Result<(), AppError>;

    /// Takes the next recognition event.
    ///
    /// # Errors
    ///
    /// Returns the engine's error when the event stream fails.
    fn poll(&self) -> Result<Option<Event>, AppError>;

    /// Ends the stream and returns the committed transcript.
    ///
    /// # Errors
    ///
    /// Returns [`Incomplete`] with the text committed before a backend failure.
    fn finish(self: Box<Self>) -> Result<Transcript, Incomplete>;
}

/// A speech engine that opens streaming sessions.
pub trait LiveTranscriber: Send + Sync {
    /// Opens a session for `spec`.
    ///
    /// # Errors
    ///
    /// Returns the engine's error when the session cannot start.
    fn live(&self, spec: AudioSpec) -> Result<Box<dyn LiveRecognizer>, AppError>;
}

/// The platform operations the commands use.
pub trait Host: Sync {
    /// Lists the capture devices the platform reports.
    ///
    /// # Errors
    ///
    /// Returns the capture backend's error when it cannot enumerate devices.
    fn devices(&self) -> Result<Vec<DeviceInfo>, AppError>;

    /// Opens the default device for `spec`.
    ///
    /// # Errors
    ///
    /// Returns the capture backend's error when no device can deliver `spec`.
    fn open_capture(&self, spec: StreamSpec) -> Result<Box<dyn BlockStream>, AppError>;

    /// Creates a recording at `path`.
    ///
    /// # Errors
    ///
    /// Returns the recorder's error when the format or the file is rejected.
    fn create_recorder(
        &self,
        path: &Path,
        spec: RecordingSpec,
    ) -> Result<Box<dyn TrackSink>, AppError>;

    /// Loads a speech model.
    ///
    /// # Errors
    ///
    /// Returns the transcription backend's error when the model cannot be
    /// loaded.
    fn recognizer(&self, model: &Model) -> Result<Box<dyn Recognizer>, AppError>;

    /// Loads a speech model for streaming sessions.
    ///
    /// # Errors
    ///
    /// Returns the transcription backend's error when the model cannot be
    /// loaded.
    fn live_transcriber(
        &self,
        model: &Model,
        options: &Options,
    ) -> Result<Box<dyn LiveTranscriber>, AppError>;
}

/// The host backed by the koe platform crates.
pub struct RealHost;

impl Host for RealHost {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AppError> {
        koe_capture::devices().map_err(AppError::from)
    }

    fn open_capture(&self, spec: StreamSpec) -> Result<Box<dyn BlockStream>, AppError> {
        let stream = koe_capture::open(&spec)?;
        Ok(Box::new(RealStream(stream)))
    }

    fn create_recorder(
        &self,
        path: &Path,
        spec: RecordingSpec,
    ) -> Result<Box<dyn TrackSink>, AppError> {
        let recorder = Recorder::create(path, spec)?;
        Ok(Box::new(RealRecorder(Some(recorder))))
    }

    fn recognizer(&self, model: &Model) -> Result<Box<dyn Recognizer>, AppError> {
        let transcriber = Transcriber::new(model)?;
        Ok(Box::new(RealRecognizer(transcriber)))
    }

    fn live_transcriber(
        &self,
        model: &Model,
        options: &Options,
    ) -> Result<Box<dyn LiveTranscriber>, AppError> {
        let transcriber = Transcriber::new(model)?;
        Ok(Box::new(RealLiveTranscriber {
            transcriber,
            options: options.clone(),
        }))
    }
}

/// Sets the stop flag from a signal handler.
pub fn request_stop(interrupt: &AtomicBool) {
    interrupt.store(true, Ordering::Release);
}

/// Installs a Ctrl-C handler that sets `interrupt`.
///
/// # Errors
///
/// Returns the signal crate's error when a handler is already installed.
pub fn install_interrupt(interrupt: &Arc<AtomicBool>) -> Result<(), ctrlc::Error> {
    let flag = Arc::clone(interrupt);
    ctrlc::set_handler(move || request_stop(&flag))
}

struct RealStream(CaptureStream);

impl BlockStream for RealStream {
    fn read(&mut self, out: &mut [f32]) -> Result<usize, AppError> {
        self.0.read(out).map_err(AppError::from)
    }
}

struct RealRecorder(Option<Recorder>);

impl RealRecorder {
    fn recorder(&mut self) -> Result<&mut Recorder, AppError> {
        self.0.as_mut().ok_or(AppError::RecorderFinished)
    }
}

impl TrackSink for RealRecorder {
    fn add_source(&mut self, source: Source, spec: StreamSpec) -> Result<(), AppError> {
        self.recorder()?.add_source(source, &spec)?;
        Ok(())
    }

    fn set_gain(&mut self, source: Source, gain: f32) -> Result<(), AppError> {
        self.recorder()?.set_gain(source, gain)?;
        Ok(())
    }

    fn write(&mut self, source: Source, samples: &[f32]) -> Result<(), AppError> {
        self.recorder()?.write(source, samples)?;
        Ok(())
    }

    fn end(&mut self, source: Source) -> Result<(), AppError> {
        self.recorder()?.end(source)?;
        Ok(())
    }

    fn finish(&mut self) -> Result<Summary, AppError> {
        let recorder = self.0.take().ok_or(AppError::RecorderFinished)?;
        recorder.finish().map_err(AppError::from)
    }
}

struct RealRecognizer(Transcriber);

impl Recognizer for RealRecognizer {
    fn transcribe(
        &self,
        pcm: &[f32],
        spec: AudioSpec,
        options: &Options,
    ) -> Result<Transcript, Incomplete> {
        self.0.transcribe(pcm, spec, options)
    }
}

struct RealLiveTranscriber {
    transcriber: Transcriber,
    options: Options,
}

impl LiveTranscriber for RealLiveTranscriber {
    fn live(&self, spec: AudioSpec) -> Result<Box<dyn LiveRecognizer>, AppError> {
        let session = self.transcriber.live(spec, &self.options)?;
        Ok(Box::new(RealLiveRecognizer(session)))
    }
}

struct RealLiveRecognizer(LiveSession);

impl LiveRecognizer for RealLiveRecognizer {
    fn feed(&self, samples: &[f32]) -> Result<(), AppError> {
        self.0.feed(samples).map_err(AppError::from)
    }

    fn poll(&self) -> Result<Option<Event>, AppError> {
        self.0.poll().map_err(AppError::from)
    }

    fn finish(self: Box<Self>) -> Result<Transcript, Incomplete> {
        self.0.finish()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    use koe_capture::DeviceId;
    use koe_capture::Error as CaptureError;
    use koe_capture_core::{CaptureStream, RawStream};
    use koe_record::{Channels, Error as RecordError};
    use koe_transcribe::{Engine, EngineEvent, EngineSession, Error as TranscribeError};

    use super::*;

    fn mono() -> StreamSpec {
        StreamSpec::new(Source::Microphone, 48_000, 1).unwrap()
    }

    fn device() -> DeviceInfo {
        DeviceInfo::new(
            DeviceId::new("device").unwrap(),
            "Device",
            Source::Microphone,
            true,
            48_000,
            1,
        )
    }

    #[test]
    fn a_requested_stop_sets_the_flag() {
        let flag = AtomicBool::new(false);
        request_stop(&flag);
        assert!(flag.load(Ordering::Acquire));
    }

    #[test]
    fn a_second_interrupt_handler_is_rejected() {
        let first = install_interrupt(&Arc::new(AtomicBool::new(false)));
        assert!(first.is_ok());
        let second = install_interrupt(&Arc::new(AtomicBool::new(false)));
        assert!(second.is_err());
    }

    struct OneBlock(Option<Vec<f32>>);

    impl RawStream for OneBlock {
        fn read(&mut self, out: &mut [f32]) -> Result<usize, CaptureError> {
            let block = self.0.take().ok_or(CaptureError::StreamClosed)?;
            out[..block.len()].copy_from_slice(&block);
            Ok(block.len())
        }
    }

    #[test]
    fn the_real_stream_delegates_reads_and_errors() {
        let stream =
            CaptureStream::new(Box::new(OneBlock(Some(vec![0.25, -0.5]))), device(), mono());
        let mut stream = RealStream(stream);
        let mut buffer = [0.0; 2];
        assert_eq!(stream.read(&mut buffer).unwrap(), 2);
        assert_eq!(buffer, [0.25, -0.5]);
        assert!(matches!(
            stream.read(&mut buffer),
            Err(AppError::Capture(CaptureError::StreamClosed))
        ));
    }

    #[test]
    fn the_real_recorder_writes_a_wav_and_refuses_a_second_finish() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("take.wav");
        let spec = RecordingSpec::new(48_000, Channels::Mono).unwrap();
        let mut sink = RealHost.create_recorder(&path, spec).unwrap();
        sink.add_source(Source::Microphone, mono()).unwrap();
        sink.set_gain(Source::Microphone, 1.0).unwrap();
        sink.write(Source::Microphone, &[0.5, -0.5]).unwrap();
        sink.end(Source::Microphone).unwrap();
        let summary = sink.finish().unwrap();
        assert_eq!(summary.frames, 2);

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");

        assert!(matches!(sink.finish(), Err(AppError::RecorderFinished)));
        assert!(matches!(
            sink.add_source(Source::Microphone, mono()),
            Err(AppError::RecorderFinished)
        ));
        assert!(matches!(
            sink.set_gain(Source::Microphone, 1.0),
            Err(AppError::RecorderFinished)
        ));
        assert!(matches!(
            sink.write(Source::Microphone, &[0.0]),
            Err(AppError::RecorderFinished)
        ));
        assert!(matches!(
            sink.end(Source::Microphone),
            Err(AppError::RecorderFinished)
        ));
    }

    #[test]
    fn the_real_host_rejects_an_unknown_output_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("take.mp3");
        let spec = RecordingSpec::new(48_000, Channels::Mono).unwrap();
        assert!(matches!(
            RealHost.create_recorder(&path, spec),
            Err(AppError::Record(RecordError::UnknownFormat { .. }))
        ));
    }

    #[test]
    fn the_real_host_rejects_a_rate_the_container_cannot_carry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("take.ogg");
        let spec = RecordingSpec::new(44_100, Channels::Stereo).unwrap();
        assert!(matches!(
            RealHost.create_recorder(&path, spec),
            Err(AppError::Record(RecordError::UnsupportedRate {
                rate: 44_100
            }))
        ));
    }

    #[cfg(not(any(feature = "linux", feature = "macos", feature = "windows")))]
    #[test]
    fn without_a_backend_the_platform_operations_report_unsupported() {
        assert!(matches!(
            RealHost.devices(),
            Err(AppError::Capture(CaptureError::Unsupported))
        ));
        assert!(matches!(
            RealHost.open_capture(mono()),
            Err(AppError::Capture(CaptureError::Unsupported))
        ));
        assert!(matches!(
            RealHost.recognizer(&Model::gguf("model.gguf")),
            Err(AppError::Transcribe(TranscribeError::Unsupported))
        ));
        assert!(matches!(
            RealHost.live_transcriber(&Model::gguf("model.gguf"), &Options::new()),
            Err(AppError::Transcribe(TranscribeError::Unsupported))
        ));
    }

    struct Scripted;

    struct ScriptedSession {
        polled: bool,
    }

    impl Engine for Scripted {
        fn new_session(
            &self,
            _options: &Options,
        ) -> Result<Box<dyn EngineSession>, TranscribeError> {
            Ok(Box::new(ScriptedSession { polled: false }))
        }
    }

    impl EngineSession for ScriptedSession {
        fn feed(&mut self, _pcm: &[f32]) -> Result<(), TranscribeError> {
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<EngineEvent>, TranscribeError> {
            if self.polled {
                return Ok(None);
            }
            self.polled = true;
            Ok(Some(EngineEvent::Committed {
                text: "hello".into(),
                start_ms: 0,
                end_ms: 100,
            }))
        }

        fn finish(&mut self) -> Result<(), TranscribeError> {
            Ok(())
        }
    }

    #[test]
    fn the_real_recognizer_delegates_to_the_engine() {
        let recognizer = RealRecognizer(Transcriber::with_engine(Arc::new(Scripted)));
        let spec = AudioSpec::new(16_000, 1).unwrap();
        let transcript = recognizer
            .transcribe(&[0.0; 1_600], spec, &Options::new())
            .unwrap();
        assert_eq!(transcript.text(), "hello");
    }

    fn live_transcriber(engine: Arc<dyn Engine>) -> RealLiveTranscriber {
        RealLiveTranscriber {
            transcriber: Transcriber::with_engine(engine),
            options: Options::new(),
        }
    }

    fn poll_until(live: &dyn LiveRecognizer) -> Option<Event> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match live.poll() {
                Ok(Some(event)) => return Some(event),
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => return None,
            }
        }
        None
    }

    fn poll_error(live: &dyn LiveRecognizer) -> Option<AppError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match live.poll() {
                Ok(_) => std::thread::sleep(Duration::from_millis(5)),
                Err(error) => return Some(error),
            }
        }
        None
    }

    #[test]
    fn the_real_live_transcriber_delegates_a_session() {
        let transcriber = live_transcriber(Arc::new(Scripted));
        let live = transcriber
            .live(AudioSpec::new(16_000, 1).unwrap())
            .unwrap();
        live.feed(&[0.0; 1_600]).unwrap();
        match poll_until(live.as_ref()) {
            Some(Event::Committed { text, .. }) => assert_eq!(&*text, "hello"),
            other => panic!("expected a committed event, got {other:?}"),
        }
        let transcript = live.finish().unwrap();
        assert_eq!(transcript.text(), "hello");
    }

    #[test]
    fn the_real_live_recognizer_rejects_a_misaligned_buffer() {
        let transcriber = live_transcriber(Arc::new(Scripted));
        let live = transcriber
            .live(AudioSpec::new(16_000, 2).unwrap())
            .unwrap();
        assert!(matches!(
            live.feed(&[0.0]),
            Err(AppError::Transcribe(TranscribeError::MisalignedBuffer {
                len: 1,
                channels: 2
            }))
        ));
    }

    struct NoSession;

    impl Engine for NoSession {
        fn new_session(
            &self,
            _options: &Options,
        ) -> Result<Box<dyn EngineSession>, TranscribeError> {
            Err(TranscribeError::Backend {
                operation: "open",
                code: 9,
            })
        }
    }

    #[test]
    fn a_live_session_creation_failure_is_returned() {
        let transcriber = live_transcriber(Arc::new(NoSession));
        assert!(matches!(
            transcriber.live(AudioSpec::new(16_000, 1).unwrap()),
            Err(AppError::Transcribe(TranscribeError::Backend {
                code: 9,
                ..
            }))
        ));
    }

    struct PollError;
    struct PollErrorSession;

    impl Engine for PollError {
        fn new_session(
            &self,
            _options: &Options,
        ) -> Result<Box<dyn EngineSession>, TranscribeError> {
            Ok(Box::new(PollErrorSession))
        }
    }

    impl EngineSession for PollErrorSession {
        fn feed(&mut self, _pcm: &[f32]) -> Result<(), TranscribeError> {
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<EngineEvent>, TranscribeError> {
            Err(TranscribeError::Backend {
                operation: "poll",
                code: 7,
            })
        }

        fn finish(&mut self) -> Result<(), TranscribeError> {
            Ok(())
        }
    }

    #[test]
    fn a_live_poll_failure_is_returned() {
        let transcriber = live_transcriber(Arc::new(PollError));
        let live = transcriber
            .live(AudioSpec::new(16_000, 1).unwrap())
            .unwrap();
        live.feed(&[0.0; 1_600]).unwrap();
        let error = poll_error(live.as_ref()).expect("the poll error must arrive");
        assert!(matches!(
            error,
            AppError::Transcribe(TranscribeError::Backend { code: 7, .. })
        ));
        let _ = live.finish();
    }

    struct FinishError;
    struct FinishErrorSession {
        polled: bool,
    }

    impl Engine for FinishError {
        fn new_session(
            &self,
            _options: &Options,
        ) -> Result<Box<dyn EngineSession>, TranscribeError> {
            Ok(Box::new(FinishErrorSession { polled: false }))
        }
    }

    impl EngineSession for FinishErrorSession {
        fn feed(&mut self, _pcm: &[f32]) -> Result<(), TranscribeError> {
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<EngineEvent>, TranscribeError> {
            if self.polled {
                return Ok(None);
            }
            self.polled = true;
            Ok(Some(EngineEvent::Committed {
                text: "hello".into(),
                start_ms: 0,
                end_ms: 100,
            }))
        }

        fn finish(&mut self) -> Result<(), TranscribeError> {
            Err(TranscribeError::Backend {
                operation: "finish",
                code: 3,
            })
        }
    }

    #[test]
    fn a_live_finish_failure_keeps_the_committed_text() {
        let transcriber = live_transcriber(Arc::new(FinishError));
        let live = transcriber
            .live(AudioSpec::new(16_000, 1).unwrap())
            .unwrap();
        live.feed(&[0.0; 1_600]).unwrap();
        let incomplete = live.finish().unwrap_err();
        assert!(matches!(
            incomplete.error(),
            TranscribeError::Backend {
                operation: "finish",
                code: 3
            }
        ));
        assert_eq!(incomplete.transcript().text(), "hello");
    }
}
