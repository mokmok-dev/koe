//! Scripted hosts, streams, sinks, and recognizers for the command tests.

#![allow(clippy::unwrap_used, clippy::needless_pass_by_value)]

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use koe_capture::{DeviceInfo, Source, StreamSpec};
use koe_record::{RecordingSpec, Summary};
use koe_transcribe::{AudioSpec, Incomplete, Model, Options, Transcript};

use crate::error::AppError;
use crate::host::{BlockStream, Host, Recognizer, TrackSink};

/// A host whose every reply is scripted by the test.
#[derive(Default)]
pub struct FakeHost {
    devices: Mutex<Option<Result<Vec<DeviceInfo>, AppError>>>,
    opens: Mutex<HashMap<Source, VecDeque<Result<FakeStream, AppError>>>>,
    recorder: Mutex<Option<Result<FakeSink, AppError>>>,
    recognizer: Mutex<Option<Result<Box<dyn Recognizer>, AppError>>>,
    /// The sink state the scripted recorder shares with the test.
    pub sink: Arc<Mutex<SinkState>>,
    /// Every stream spec the pump asked to open.
    pub opens_seen: Mutex<Vec<StreamSpec>>,
    /// Every model the transcribe command asked for.
    pub models_seen: Mutex<Vec<Model>>,
}

impl FakeHost {
    /// A host whose device list is `devices`.
    pub fn with_devices(devices: Vec<DeviceInfo>) -> Self {
        let host = Self::default();
        *host.devices.lock().unwrap() = Some(Ok(devices));
        host
    }

    /// A host whose device list fails with `Unsupported`.
    pub fn without_devices() -> Self {
        let host = Self::default();
        *host.devices.lock().unwrap() =
            Some(Err(AppError::Capture(koe_capture::Error::Unsupported)));
        host
    }

    /// Scripts a stream for `source`.
    pub fn with_stream(self, source: Source, stream: FakeStream) -> Self {
        self.opens
            .lock()
            .unwrap()
            .entry(source)
            .or_default()
            .push_back(Ok(stream));
        self
    }

    /// Scripts an open failure for `source`.
    pub fn with_open_error(self, source: Source, error: AppError) -> Self {
        self.opens
            .lock()
            .unwrap()
            .entry(source)
            .or_default()
            .push_back(Err(error));
        self
    }

    /// Scripts a recorder that shares `sink`.
    pub fn with_sink(self) -> Self {
        *self.recorder.lock().unwrap() = Some(Ok(FakeSink {
            state: Arc::clone(&self.sink),
        }));
        self
    }

    /// Scripts a recorder creation failure.
    pub fn with_create_error(self, error: AppError) -> Self {
        *self.recorder.lock().unwrap() = Some(Err(error));
        self
    }

    /// Scripts a recognizer.
    pub fn with_recognizer(self, recognizer: Box<dyn Recognizer>) -> Self {
        *self.recognizer.lock().unwrap() = Some(Ok(recognizer));
        self
    }

    /// Scripts a recognizer creation failure.
    pub fn with_recognizer_error(self, error: AppError) -> Self {
        *self.recognizer.lock().unwrap() = Some(Err(error));
        self
    }
}

impl Host for FakeHost {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AppError> {
        self.devices
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| Err(AppError::usage("no scripted devices")))
    }

    fn open_capture(&self, spec: StreamSpec) -> Result<Box<dyn BlockStream>, AppError> {
        self.opens_seen.lock().unwrap().push(spec);
        let next = self
            .opens
            .lock()
            .unwrap()
            .get_mut(&spec.source())
            .and_then(VecDeque::pop_front);
        match next {
            Some(Ok(stream)) => Ok(Box::new(stream)),
            Some(Err(error)) => Err(error),
            None => Err(AppError::usage("no scripted stream")),
        }
    }

    fn create_recorder(
        &self,
        _path: &Path,
        _spec: RecordingSpec,
    ) -> Result<Box<dyn TrackSink>, AppError> {
        match self.recorder.lock().unwrap().take() {
            Some(Ok(sink)) => Ok(Box::new(sink)),
            Some(Err(error)) => Err(error),
            None => Err(AppError::usage("no scripted recorder")),
        }
    }

    fn recognizer(&self, model: &Model) -> Result<Box<dyn Recognizer>, AppError> {
        self.models_seen.lock().unwrap().push(model.clone());
        match self.recognizer.lock().unwrap().take() {
            Some(Ok(recognizer)) => Ok(recognizer),
            Some(Err(error)) => Err(error),
            None => Err(AppError::usage("no scripted recognizer")),
        }
    }
}

/// A stream that returns its scripted reads in order.
pub struct FakeStream {
    script: Mutex<VecDeque<Result<Vec<f32>, AppError>>>,
    channels: usize,
    /// The buffer length of every read the pump made.
    pub reads: Arc<Mutex<Vec<usize>>>,
}

impl FakeStream {
    /// A stream with `channels` that returns `script`, then reports the stream
    /// closed.
    pub fn new(channels: usize, script: Vec<Result<Vec<f32>, AppError>>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            channels,
            reads: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl BlockStream for FakeStream {
    fn read(&mut self, out: &mut [f32]) -> Result<usize, AppError> {
        self.reads.lock().unwrap().push(out.len());
        let next = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(AppError::Capture(koe_capture::Error::StreamClosed)));
        match next {
            Ok(samples) => {
                out[..samples.len()].copy_from_slice(&samples);
                Ok(samples.len() / self.channels)
            }
            Err(error) => Err(error),
        }
    }
}

/// What the scripted sink saw.
pub struct SinkState {
    /// Every registered source and its stream spec.
    pub added: Vec<(Source, StreamSpec)>,
    /// Every gain set.
    pub gains: Vec<(Source, f32)>,
    /// Every written block, keyed by source.
    pub writes: Vec<(Source, Vec<f32>)>,
    /// Every ended source.
    pub ended: Vec<Source>,
    /// Whether the sink was finished.
    pub finished: bool,
    /// Whether writes fail.
    pub fail_write: bool,
    /// Whether ends fail.
    pub fail_end: bool,
    /// Whether finishing fails.
    pub fail_finish: bool,
    /// The summary finishing reports.
    pub summary: Summary,
}

impl Default for SinkState {
    fn default() -> Self {
        Self {
            added: Vec::new(),
            gains: Vec::new(),
            writes: Vec::new(),
            ended: Vec::new(),
            finished: false,
            fail_write: false,
            fail_end: false,
            fail_finish: false,
            summary: Summary {
                frames: 0,
                clipped_samples: 0,
                duration: std::time::Duration::ZERO,
            },
        }
    }
}

/// A sink that records every call and fails where the state says.
pub struct FakeSink {
    /// The state shared with the test.
    pub state: Arc<Mutex<SinkState>>,
}

impl FakeSink {
    fn state(&self) -> MutexGuard<'_, SinkState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl TrackSink for FakeSink {
    fn add_source(&mut self, source: Source, spec: StreamSpec) -> Result<(), AppError> {
        self.state().added.push((source, spec));
        Ok(())
    }

    fn set_gain(&mut self, source: Source, gain: f32) -> Result<(), AppError> {
        self.state().gains.push((source, gain));
        Ok(())
    }

    fn write(&mut self, source: Source, samples: &[f32]) -> Result<(), AppError> {
        let mut state = self.state();
        if state.fail_write {
            return Err(AppError::usage("write failed"));
        }
        state.writes.push((source, samples.to_vec()));
        Ok(())
    }

    fn end(&mut self, source: Source) -> Result<(), AppError> {
        let mut state = self.state();
        if state.fail_end {
            return Err(AppError::usage("end failed"));
        }
        state.ended.push(source);
        Ok(())
    }

    fn finish(&mut self) -> Result<Summary, AppError> {
        let mut state = self.state();
        if state.fail_finish {
            return Err(AppError::usage("finish failed"));
        }
        state.finished = true;
        Ok(state.summary)
    }
}

/// One recognizer call.
pub struct Call {
    /// The samples the command handed over.
    pub pcm: Vec<f32>,
    /// The format the command named.
    pub spec: AudioSpec,
    /// The requested language, if any.
    pub language: Option<String>,
}

/// A recognizer that returns its scripted result once.
pub struct FakeRecognizer {
    result: Mutex<Option<Result<Transcript, Incomplete>>>,
    /// Every call the command made.
    pub calls: Arc<Mutex<Vec<Call>>>,
}

impl FakeRecognizer {
    /// A recognizer that returns `result` on its first call.
    pub fn new(result: Result<Transcript, Incomplete>) -> Self {
        Self {
            result: Mutex::new(Some(result)),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Recognizer for FakeRecognizer {
    fn transcribe(
        &self,
        pcm: &[f32],
        spec: AudioSpec,
        options: &Options,
    ) -> Result<Transcript, Incomplete> {
        self.calls.lock().unwrap().push(Call {
            pcm: pcm.to_vec(),
            spec,
            language: options.language_tag().map(str::to_owned),
        });
        self.result
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| Ok(Transcript::default()))
    }
}
