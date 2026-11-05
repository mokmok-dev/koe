//! `transcribe.cpp` model and session handles.

#![allow(unsafe_code)]

use std::mem::ManuallyDrop;

use koe_transcribe_core::{
    ENGINE_RATE, Engine, EngineSession, Error, Options, Segment, Transcript,
};
use transcribe_cpp::{
    CommitPolicy, Error as CppError, ErrorKind, Model as Gguf, RunOptions, Session as CppSession,
    Stream, StreamOptions, StreamUpdate, Transcript as CppTranscript,
};

use crate::pipeline::{self, Native, Update};

/// A loaded GGUF model that opens sessions.
pub struct Backend {
    model: Gguf,
}

/// Loads the model at `path`.
///
/// # Errors
///
/// Returns [`Error::ModelNotFound`] when the file is absent and
/// [`Error::ModelInvalid`] when the binding cannot load it.
pub fn load(path: &str) -> Result<Backend, Error> {
    Gguf::load(path)
        .map(|model| Backend { model })
        .map_err(|error| load_error(path, &error))
}

impl Engine for Backend {
    fn new_session(&self, options: &Options) -> Result<Box<dyn EngineSession>, Error> {
        let run = run_options(options);
        let streaming = self
            .model
            .capabilities()
            .map_err(|error| backend("capabilities", &error))?
            .supports_streaming;
        let session = self
            .model
            .session()
            .map_err(|error| backend("session init", &error))?;
        if streaming {
            let native = Streaming::new(session, &run)?;
            Ok(Box::new(pipeline::Session::streaming(native)))
        } else {
            Ok(Box::new(pipeline::Session::batch(Batch::new(session, run))))
        }
    }
}

fn load_error(path: &str, error: &CppError) -> Error {
    if matches!(error, CppError::ModelFileNotFound(_)) {
        return Error::ModelNotFound { path: path.into() };
    }
    Error::ModelInvalid {
        path: path.into(),
        reason: invalid_reason(error),
    }
}

fn invalid_reason(error: &CppError) -> &'static str {
    match error.kind() {
        ErrorKind::ModelLoad | ErrorKind::Unsupported | ErrorKind::UnsupportedRole => {
            "unsupported model"
        }
        ErrorKind::OutOfMemory => "out of memory",
        ErrorKind::VersionMismatch => "binding and library versions disagree",
        _ => "model load failed",
    }
}

fn run_options(options: &Options) -> RunOptions {
    options
        .language_tag()
        .map_or_else(RunOptions::default, |language| RunOptions {
            language: Some(language.to_owned()),
            ..RunOptions::default()
        })
}

/// A status of 0 means the binding rejected the call before reaching the
/// library, so there is no native code to report.
fn backend(operation: &'static str, error: &CppError) -> Error {
    Error::Backend {
        operation,
        code: error.raw_status(),
    }
}

struct Streams {
    _session: Box<CppSession>,
    stream: ManuallyDrop<Stream<'static>>,
}

impl Streams {
    fn new(mut session: Box<CppSession>, run: &RunOptions) -> Result<Self, Error> {
        // Mid-stream commits follow the raw hypothesis, which can still
        // rewrite already-committed words; a committed event cannot be
        // retracted, so only the finalize text is committed.
        let stream_options = StreamOptions {
            commit_policy: CommitPolicy::OnFinalize,
            ..StreamOptions::default()
        };
        let stream = session
            .stream(run, &stream_options)
            .map_err(|error| backend("stream begin", &error))?;
        let stream = extend(stream);
        Ok(Self {
            _session: session,
            stream: ManuallyDrop::new(stream),
        })
    }
}

impl Drop for Streams {
    fn drop(&mut self) {
        // SAFETY: the stream is taken once and never used again; the boxed
        // session it borrows is dropped after this method returns.
        drop(unsafe { ManuallyDrop::take(&mut self.stream) });
    }
}

fn extend<'a>(stream: Stream<'a>) -> Stream<'static> {
    // The boxed session keeps its address stable, and the stream is dropped
    // before the box, so the extended borrow never outlives its owner.
    unsafe { std::mem::transmute::<Stream<'a>, Stream<'static>>(stream) }
}

struct Streaming {
    streams: Streams,
    end_ms: u64,
}

impl Streaming {
    fn new(session: CppSession, run: &RunOptions) -> Result<Self, Error> {
        Ok(Self {
            streams: Streams::new(Box::new(session), run)?,
            end_ms: 0,
        })
    }

    fn advance(&mut self, update: &StreamUpdate) -> u64 {
        let progress = u64::try_from(update.audio_committed_ms).unwrap_or_default();
        let received = u64::try_from(update.input_received_ms).unwrap_or_default();
        let end = if progress == 0 { received } else { progress };
        self.end_ms = end.max(self.end_ms);
        self.end_ms
    }
}

impl Native for Streaming {
    fn feed(&mut self, pcm: &[f32]) -> Result<Update, Error> {
        let update = self
            .streams
            .stream
            .feed(pcm)
            .map_err(|error| backend("stream feed", &error))?;
        let text = self.streams.stream.text();
        let start_ms = self.end_ms;
        let end_ms = self.advance(&update);
        Ok(Update {
            committed: text.committed,
            volatile: text.tentative,
            start_ms,
            end_ms,
        })
    }

    fn finish(&mut self) -> Result<Transcript, Error> {
        let update = self
            .streams
            .stream
            .finalize()
            .map_err(|error| backend("stream finalize", &error))?;
        let end_ms = self.advance(&update);
        let text = self.streams.stream.text().display();
        if text.trim().is_empty() {
            return Ok(Transcript::default());
        }
        Ok(Transcript::new(vec![Segment::new(0, end_ms, text)], 0))
    }
}

struct Batch {
    session: CppSession,
    run: RunOptions,
    pcm: Vec<f32>,
}

impl Batch {
    const fn new(session: CppSession, run: RunOptions) -> Self {
        Self {
            session,
            run,
            pcm: Vec::new(),
        }
    }
}

impl Native for Batch {
    fn feed(&mut self, pcm: &[f32]) -> Result<Update, Error> {
        self.pcm.extend_from_slice(pcm);
        Ok(Update::default())
    }

    fn finish(&mut self) -> Result<Transcript, Error> {
        let result = self
            .session
            .run(&self.pcm, &self.run)
            .map_err(|error| backend("run", &error))?;
        Ok(transcript_of(&result, self.pcm.len()))
    }
}

fn transcript_of(result: &CppTranscript, samples: usize) -> Transcript {
    let mut segments: Vec<Segment> = result
        .segments
        .iter()
        .filter_map(|segment| {
            let text = segment.text.trim();
            (!text.is_empty()).then(|| Segment::new(ms(segment.t0_ms), ms(segment.t1_ms), text))
        })
        .collect();
    if segments.is_empty() && !result.text.trim().is_empty() {
        segments.push(Segment::new(0, duration_ms(samples), result.text.trim()));
    }
    Transcript::new(segments, 0)
}

fn ms(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

fn duration_ms(samples: usize) -> u64 {
    let frames = u64::try_from(samples).unwrap_or(u64::MAX);
    frames * 1_000 / u64::from(ENGINE_RATE)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_missing_file_is_reported_as_not_found() {
        let error = CppError::ModelFileNotFound("missing.gguf".to_owned());
        assert_eq!(
            load_error("/models/missing.gguf", &error),
            Error::ModelNotFound {
                path: "/models/missing.gguf".into()
            }
        );
    }

    #[test]
    fn a_failed_load_is_reported_as_invalid() {
        let error = CppError::ModelLoad("unsupported architecture".to_owned());
        assert_eq!(
            load_error("/models/bad.gguf", &error),
            Error::ModelInvalid {
                path: "/models/bad.gguf".into(),
                reason: "unsupported model"
            }
        );
    }

    #[test]
    fn a_duration_covers_whole_milliseconds() {
        assert_eq!(duration_ms(16_000), 1_000);
        assert_eq!(duration_ms(8_000), 500);
    }

    #[test]
    fn segments_without_text_are_dropped() {
        let result = CppTranscript {
            text: "hello world".to_owned(),
            segments: vec![
                transcribe_cpp::Segment {
                    t0_ms: 0,
                    t1_ms: 400,
                    text: " hello ".to_owned(),
                    ..transcribe_cpp::Segment::default()
                },
                transcribe_cpp::Segment {
                    t0_ms: 400,
                    t1_ms: 800,
                    text: "  ".to_owned(),
                    ..transcribe_cpp::Segment::default()
                },
                transcribe_cpp::Segment {
                    t0_ms: 400,
                    t1_ms: 900,
                    text: "world".to_owned(),
                    ..transcribe_cpp::Segment::default()
                },
            ],
            ..CppTranscript::default()
        };
        let transcript = transcript_of(&result, 16_000);
        assert_eq!(transcript.text(), "hello world");
        assert_eq!(transcript.segments().len(), 2);
        assert_eq!(transcript.segments()[1].start_ms(), 400);
    }
}
