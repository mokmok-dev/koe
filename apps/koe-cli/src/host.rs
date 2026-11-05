//! The seam between the commands and the platform crates.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use koe_capture::{CaptureStream, DeviceInfo, Source, StreamSpec};
use koe_record::{Recorder, RecordingSpec, Summary};
use koe_transcribe::{AudioSpec, Incomplete, Model, Options, Transcriber, Transcript};

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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

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
}
