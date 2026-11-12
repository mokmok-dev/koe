//! The recorder thread and the events it sends to the UI.
//!
//! One session thread owns the file and one scoped thread per source reads
//! the blocking capture stream. The UI never touches audio, so a stalled
//! device delays only the save, not the frame loop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use koe_capture::{Source, StreamSpec};
use koe_record::{Channels, Recorder, RecordingSpec, Summary};
use koe_transcribe::{AudioSpec, Event, LiveSession, Model, Options, Transcriber};

use crate::dictation_worker::{BACKLOG_LIMIT, MODEL_LOAD, push_bounded};
use crate::error::DesktopError;
use crate::state::Sources;
use crate::transcript::at_offset;

/// Sample rate every take uses.
pub const SAMPLE_RATE: u32 = 48_000;

/// Frames each capture thread hands the recorder per read.
const BLOCK_FRAMES: usize = 960;

/// How often a source reports its peak level.
const LEVEL_INTERVAL: Duration = Duration::from_millis(100);

/// What the UI asked the recorder to write.
#[derive(Clone, Debug)]
pub struct SessionPlan {
    /// Where the audio goes.
    pub output: PathBuf,
    /// Container and channel layout.
    pub spec: RecordingSpec,
    /// The stream each source opens.
    pub streams: Vec<(Source, StreamSpec)>,
    /// The sources the plan covers.
    pub sources: Sources,
    /// The speech-to-text to run beside the recorder, when one is configured.
    pub transcription: Option<TranscriptionPlan>,
}

/// The speech-to-text a take runs beside its recorder.
#[derive(Clone, Debug)]
pub struct TranscriptionPlan {
    /// The model every source's session loads.
    pub model: Model,
    /// The BCP 47 language to request, or the engine's choice.
    pub language: Option<Box<str>>,
}

impl SessionPlan {
    /// Builds a plan for `sources` at the app's fixed rate.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopError::NoSources`] for an empty set and the capture or
    /// recording error when a fixed value is rejected.
    pub fn meeting(output: PathBuf, sources: Sources) -> Result<Self, DesktopError> {
        if sources.is_empty() {
            return Err(DesktopError::NoSources);
        }
        let spec = RecordingSpec::new(SAMPLE_RATE, Channels::Stereo)?;
        let mut streams = Vec::with_capacity(sources.len());
        for source in sources.iter() {
            let channels = match source {
                Source::Microphone => 1,
                Source::SystemAudio => 2,
            };
            streams.push((source, StreamSpec::new(source, SAMPLE_RATE, channels)?));
        }
        Ok(Self {
            output,
            spec,
            streams,
            sources,
            transcription: None,
        })
    }
}

/// What the recorder thread reports.
#[derive(Debug)]
pub enum EngineEvent {
    /// A source's device opened.
    Opened(Source),
    /// A source never opened, or stopped delivering.
    Lost {
        /// Source that went away.
        source: Source,
        /// Why it went away.
        message: String,
    },
    /// The peak sample of a block, for the HUD meter.
    Level {
        /// Source the level came from.
        source: Source,
        /// Peak absolute sample in `0.0..=1.0`.
        level: f32,
    },
    /// A non-fatal problem the user should see.
    Warning(String),
    /// A finalized utterance from one source.
    Transcript {
        /// Source that produced the utterance.
        source: Source,
        /// Time the utterance began.
        at: SystemTime,
        /// Recognized text.
        text: String,
    },
    /// Text a source's engine is still revising; empty text clears it.
    Draft {
        /// Source the draft belongs to.
        source: Source,
        /// Time the draft arrived.
        at: SystemTime,
        /// Recognized text so far.
        text: String,
    },
    /// The file is complete.
    Finished(Box<Completion>),
}

/// The result of one take.
#[derive(Debug)]
pub struct Completion {
    /// File the recorder wrote.
    pub output: PathBuf,
    /// What the file holds, when the recorder finished cleanly.
    pub summary: Option<Summary>,
    /// Why it did not.
    pub failure: Option<DesktopError>,
}

/// A running recorder.
pub struct Session {
    output: PathBuf,
    stop: Arc<AtomicBool>,
    events: Receiver<EngineEvent>,
}

impl Session {
    /// Starts a recorder thread for `plan`.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopError::Thread`] when the thread cannot be spawned.
    pub fn start(plan: SessionPlan) -> Result<Self, DesktopError> {
        let output = plan.output.clone();
        let (sender, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        thread::Builder::new()
            .name("koe-recorder".to_owned())
            .spawn(move || run(&plan, &thread_stop, &sender))
            .map_err(|error| DesktopError::Thread(error.to_string()))?;
        Ok(Self {
            output,
            stop,
            events,
        })
    }

    /// Asks the capture threads to stop after their current block.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Takes the next event without blocking.
    #[must_use]
    pub fn poll(&self) -> Option<EngineEvent> {
        self.events.try_recv().ok()
    }

    /// The file this session writes.
    #[must_use]
    pub fn output(&self) -> &Path {
        &self.output
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(plan: &SessionPlan, stop: &Arc<AtomicBool>, sender: &Sender<EngineEvent>) {
    let output = plan.output.clone();
    let mut recorder = match Recorder::create(&plan.output, plan.spec) {
        Ok(recorder) => recorder,
        Err(error) => {
            finish(sender, output, None, Some(error.into()));
            return;
        }
    };
    for (source, spec) in &plan.streams {
        if let Err(error) = recorder.add_source(*source, spec) {
            let failure = Some(error.into());
            let closed = recorder.finish();
            finish(sender, output, closed.ok(), failure);
            return;
        }
    }
    let mut ready = spawn_transcription(plan, stop, sender);
    let recorder = Mutex::new(recorder);
    let transcriptions: Vec<SourceTranscript> = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(plan.streams.len());
        for (source, spec) in &plan.streams {
            let source = *source;
            let spec = *spec;
            let sender = sender.clone();
            let stop = Arc::clone(stop);
            let recorder = &recorder;
            let ready = ready.remove(&source);
            handles.push(
                scope.spawn(move || pump(source, spec, &sender, &stop, recorder, ready.as_ref())),
            );
        }
        let mut exits = Vec::with_capacity(handles.len());
        for handle in handles {
            match handle.join() {
                Ok(exit) => exits.push(exit),
                Err(_) => warn(sender, "a capture thread stopped unexpectedly".to_owned()),
            }
        }
        if let Some(message) = no_transcript_notice(&exits) {
            warn(sender, message.to_owned());
        }
        exits
            .into_iter()
            .filter_map(|exit| exit.transcript)
            .collect()
    });
    let recorder = recorder
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    let summary = recorder.finish();
    // `LiveSession::finish` can block inside the engine, so the recorder is
    // finalized first; a stuck flush must not leave the audio file unfinalized.
    for transcription in transcriptions {
        finish_transcription(transcription, sender);
    }
    match summary {
        Ok(summary) => finish(sender, output, Some(summary), None),
        Err(error) => finish(sender, output, None, Some(error.into())),
    }
}

/// The transcription of one source, held until the audio file is finalized.
struct SourceTranscript {
    source: Source,
    live: LiveSession,
    base: SystemTime,
    emitted: usize,
}

/// What a source's capture loop leaves for the take.
struct SourceExit {
    transcript: Option<SourceTranscript>,
    /// Whether the source was still waiting for its model session.
    waiting: bool,
    /// Whether this source emitted at least one transcript event.
    spoke: bool,
}

/// Shown when a take ends before any source produced a transcript.
const NO_TRANSCRIPT: &str =
    "this take has no transcript: transcription was not ready when it stopped";

fn no_transcript_notice(exits: &[SourceExit]) -> Option<&'static str> {
    let awaiting = exits.iter().any(|exit| exit.waiting);
    let transcribed = exits.iter().any(|exit| exit.transcript.is_some());
    let spoke = exits.iter().any(|exit| exit.spoke);
    (awaiting && !transcribed && !spoke).then_some(NO_TRANSCRIPT)
}

/// Starts the model load without holding up the capture streams.
fn spawn_transcription(
    plan: &SessionPlan,
    stop: &Arc<AtomicBool>,
    sender: &Sender<EngineEvent>,
) -> HashMap<Source, Receiver<Result<LiveSession, String>>> {
    let Some(transcription) = plan.transcription.as_ref() else {
        return HashMap::new();
    };
    let options = transcription
        .language
        .clone()
        .map_or_else(Options::new, |language| Options::new().language(language))
        .live();
    let model = transcription.model.clone();
    let mut ready = HashMap::with_capacity(plan.streams.len());
    let mut wanted = Vec::with_capacity(plan.streams.len());
    for (source, spec) in &plan.streams {
        let (session_sender, session_receiver) = mpsc::channel();
        ready.insert(*source, session_receiver);
        wanted.push((*source, *spec, session_sender));
    }
    let stop = Arc::clone(stop);
    let spawned = thread::Builder::new()
        .name("koe-transcript-model".to_owned())
        .spawn(move || {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let _guard = MODEL_LOAD.lock().unwrap_or_else(PoisonError::into_inner);
            if stop.load(Ordering::Acquire) {
                return;
            }
            let transcriber = match Transcriber::new(&model) {
                Ok(transcriber) => transcriber,
                Err(error) => {
                    let message = format!("meeting transcription is unavailable: {error}");
                    for (_, _, sender) in wanted {
                        let _ = sender.send(Err(message.clone()));
                    }
                    return;
                }
            };
            for (source, spec, sender) in wanted {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let session = AudioSpec::new(spec.sample_rate(), spec.channels())
                    .map_err(|error| {
                        format!("meeting transcription is unavailable for {source}: {error}")
                    })
                    .and_then(|audio| {
                        transcriber.live(audio, &options).map_err(|error| {
                            format!("meeting transcription is unavailable for {source}: {error}")
                        })
                    });
                let _ = sender.send(session);
            }
        });
    if let Err(error) = spawned {
        warn(
            sender,
            format!("meeting transcription is unavailable: {error}"),
        );
        return HashMap::new();
    }
    ready
}

#[allow(
    clippy::too_many_lines,
    reason = "one loop owns the recorder, the level probe, and the transcription of one source; splitting it would scatter the state each block updates"
)]
fn pump(
    source: Source,
    spec: StreamSpec,
    sender: &Sender<EngineEvent>,
    stop: &AtomicBool,
    recorder: &Mutex<Recorder>,
    ready: Option<&Receiver<Result<LiveSession, String>>>,
) -> SourceExit {
    let mut waiting = ready.is_some();
    let mut live = None;
    let mut backlog = Vec::new();
    let mut emitted = 0_usize;
    let mut base = SystemTime::now();
    let mut started = false;
    let mut behind = false;
    let mut stream = match koe_capture::open(&spec) {
        Ok(stream) => {
            let _ = sender.send(EngineEvent::Opened(source));
            Some(stream)
        }
        Err(error) => {
            let _ = sender.send(EngineEvent::Lost {
                source,
                message: error.to_string(),
            });
            waiting = false;
            None
        }
    };
    if let Some(stream) = stream.as_mut() {
        let channels = usize::from(spec.channels());
        let mut buffer = vec![0.0_f32; channels * BLOCK_FRAMES];
        let mut last_level = Instant::now();
        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            if waiting {
                match ready.map(Receiver::try_recv) {
                    Some(Ok(Ok(session))) => {
                        waiting = false;
                        live =
                            start_live(session, &mut backlog, sender, source, base, &mut emitted);
                    }
                    Some(Ok(Err(message))) => {
                        warn(sender, message);
                        waiting = false;
                        backlog.clear();
                    }
                    Some(Err(TryRecvError::Empty)) | None => {}
                    Some(Err(TryRecvError::Disconnected)) => {
                        warn(
                            sender,
                            "meeting transcription stopped before its model loaded".to_owned(),
                        );
                        waiting = false;
                        backlog.clear();
                    }
                }
            }
            match stream.read(&mut buffer) {
                Ok(0) => {
                    if !stop.load(Ordering::Acquire) {
                        let _ = sender.send(EngineEvent::Lost {
                            source,
                            message: "the capture stream ended".to_owned(),
                        });
                    }
                    break;
                }
                Ok(frames) => {
                    let samples = &buffer[..frames * channels];
                    if !started {
                        base = SystemTime::now();
                        started = true;
                    }
                    if last_level.elapsed() >= LEVEL_INTERVAL {
                        last_level = Instant::now();
                        let _ = sender.send(EngineEvent::Level {
                            source,
                            level: peak(samples),
                        });
                    }
                    let mut guard = recorder.lock().unwrap_or_else(PoisonError::into_inner);
                    let written = guard.write(source, samples);
                    drop(guard);
                    if let Err(error) = written {
                        let _ = sender.send(EngineEvent::Lost {
                            source,
                            message: error.to_string(),
                        });
                        break;
                    }
                    let mut transcription_error = None;
                    if let Some(session) = live.as_ref() {
                        match session.feed(samples) {
                            Ok(()) => {
                                if let Err(error) =
                                    drain(session, sender, source, base, &mut emitted)
                                {
                                    transcription_error = Some(error);
                                }
                            }
                            Err(error) => transcription_error = Some(error),
                        }
                    } else if waiting {
                        let dropped = push_bounded(&mut backlog, samples, BACKLOG_LIMIT * channels);
                        if dropped > 0 {
                            base = at_offset(
                                base,
                                Duration::from_millis(audio_ms(
                                    dropped,
                                    channels,
                                    spec.sample_rate(),
                                )),
                            );
                            if !behind {
                                warn(
                                    sender,
                                    "meeting transcription is behind: audio dropped while the model loaded is missing from the transcript".to_owned(),
                                );
                                behind = true;
                            }
                        }
                    }
                    if let Some(error) = transcription_error {
                        warn(sender, format!("meeting transcription stopped: {error}"));
                        live = None;
                    }
                }
                Err(error) => {
                    let _ = sender.send(EngineEvent::Lost {
                        source,
                        message: error.to_string(),
                    });
                    break;
                }
            }
        }
    }
    let mut guard = recorder.lock().unwrap_or_else(PoisonError::into_inner);
    let ended = guard.end(source);
    drop(guard);
    if let Err(error) = ended {
        let _ = sender.send(EngineEvent::Warning(error.to_string()));
    }
    // The stop check can beat the ready check. Feed a session that arrived by
    // now so the take's backlog is transcribed instead of dropped with it.
    if waiting {
        match ready.map(Receiver::try_recv) {
            Some(Ok(Ok(session))) => {
                live = start_live(session, &mut backlog, sender, source, base, &mut emitted);
                waiting = false;
            }
            Some(Ok(Err(message))) => {
                warn(sender, message);
                waiting = false;
            }
            Some(Err(TryRecvError::Empty | TryRecvError::Disconnected)) | None => {}
        }
    }
    SourceExit {
        transcript: live.map(|live| SourceTranscript {
            source,
            live,
            base,
            emitted,
        }),
        waiting,
        spoke: emitted > 0,
    }
}

/// Feeds the audio a source buffered while the model loaded.
fn start_live(
    session: LiveSession,
    backlog: &mut Vec<f32>,
    sender: &Sender<EngineEvent>,
    source: Source,
    base: SystemTime,
    emitted: &mut usize,
) -> Option<LiveSession> {
    if !backlog.is_empty() {
        let ready = session
            .feed(backlog)
            .and_then(|()| drain(&session, sender, source, base, emitted));
        backlog.clear();
        if let Err(error) = ready {
            warn(sender, format!("meeting transcription stopped: {error}"));
            return None;
        }
    }
    Some(session)
}

fn finish_transcription(transcription: SourceTranscript, sender: &Sender<EngineEvent>) {
    let SourceTranscript {
        source,
        live,
        base,
        emitted,
    } = transcription;
    let transcript = match live.finish() {
        Ok(transcript) => transcript,
        Err(incomplete) => {
            warn(
                sender,
                format!(
                    "meeting transcription is incomplete: {}",
                    incomplete.error()
                ),
            );
            incomplete.into_transcript()
        }
    };
    if transcript.dropped_ms() > 0 {
        warn(
            sender,
            format!(
                "meeting transcription dropped {} ms of audio",
                transcript.dropped_ms()
            ),
        );
    }
    for segment in transcript.segments().iter().skip(emitted) {
        emit_utterance(sender, source, base, segment.start_ms(), segment.text());
    }
    // The engine will not revise anything past this point.
    let _ = sender.send(EngineEvent::Draft {
        source,
        at: SystemTime::now(),
        text: String::new(),
    });
}

fn drain(
    session: &LiveSession,
    sender: &Sender<EngineEvent>,
    source: Source,
    base: SystemTime,
    emitted: &mut usize,
) -> Result<(), koe_transcribe::Error> {
    // The session pushes one segment per committed event in the same order, so
    // `emitted` is the prefix `finish_transcription` must skip.
    while let Some(event) = session.poll()? {
        match event {
            Event::Committed { text, start_ms, .. } => {
                *emitted += 1;
                emit_utterance(sender, source, base, start_ms, &text);
            }
            // A volatile tail covers no span the engine will keep, so it is
            // stamped when it arrives rather than at a segment offset.
            Event::Volatile { text, .. } => {
                let _ = sender.send(EngineEvent::Draft {
                    source,
                    at: SystemTime::now(),
                    text: text.to_string(),
                });
            }
            // `Event` is non-exhaustive; a kind this shell cannot place in a
            // thread is dropped rather than guessed at.
            _ => {}
        }
    }
    Ok(())
}

fn emit_utterance(
    sender: &Sender<EngineEvent>,
    source: Source,
    base: SystemTime,
    start_ms: u64,
    text: &str,
) {
    let _ = sender.send(EngineEvent::Transcript {
        source,
        at: at_offset(base, Duration::from_millis(start_ms)),
        text: text.to_owned(),
    });
}

/// Milliseconds covered by `samples` interleaved at `channels`.
fn audio_ms(samples: usize, channels: usize, rate: u32) -> u64 {
    let frames = samples / channels;
    u64::try_from(frames).unwrap_or(u64::MAX) * 1_000 / u64::from(rate)
}

fn warn(sender: &Sender<EngineEvent>, message: String) {
    let _ = sender.send(EngineEvent::Warning(message));
}

fn peak(samples: &[f32]) -> f32 {
    samples
        .iter()
        .map(|sample| sample.abs())
        .fold(0.0_f32, f32::max)
}

fn finish(
    sender: &Sender<EngineEvent>,
    output: PathBuf,
    summary: Option<Summary>,
    failure: Option<DesktopError>,
) {
    let _ = sender.send(EngineEvent::Finished(Box::new(Completion {
        output,
        summary,
        failure,
    })));
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::VecDeque;

    use koe_transcribe::{
        ENGINE_RATE, Engine, EngineEvent as TranscribeEvent, EngineSession,
        Error as TranscribeError,
    };

    use super::*;
    use crate::state::Sources;
    use crate::transcript::{MeetingTranscript, Speaker};

    #[test]
    fn a_meeting_plan_asks_for_stereo_output_and_the_usual_stream_layout() {
        let plan = SessionPlan::meeting(PathBuf::from("take.ogg"), Sources::MEETING).unwrap();
        assert_eq!(plan.spec.sample_rate(), SAMPLE_RATE);
        assert_eq!(plan.spec.channels(), Channels::Stereo);
        assert_eq!(plan.sources, Sources::MEETING);
        assert_eq!(plan.streams.len(), 2);
        assert_eq!(plan.streams[0].0, Source::Microphone);
        assert_eq!(plan.streams[0].1.channels(), 1);
        assert_eq!(plan.streams[1].0, Source::SystemAudio);
        assert_eq!(plan.streams[1].1.channels(), 2);
        assert!(plan.transcription.is_none());
    }

    #[test]
    fn a_plan_carries_the_transcription_it_is_given() {
        let mut plan = SessionPlan::meeting(PathBuf::from("take.ogg"), Sources::MEETING).unwrap();
        plan.transcription = Some(TranscriptionPlan {
            model: Model::gguf("tiny.gguf"),
            language: Some("ja".into()),
        });
        let transcription = plan.transcription.unwrap();
        assert_eq!(transcription.model, Model::gguf("tiny.gguf"));
        assert_eq!(transcription.language.as_deref(), Some("ja"));
    }

    #[derive(Default)]
    struct Script {
        events: Mutex<VecDeque<TranscribeEvent>>,
        on_finish: Mutex<VecDeque<TranscribeEvent>>,
        failure: Mutex<Option<TranscribeError>>,
    }

    struct ScriptedEngine(Arc<Script>);

    struct ScriptedSession(Arc<Script>);

    impl Engine for ScriptedEngine {
        fn new_session(&self, _: &Options) -> Result<Box<dyn EngineSession>, TranscribeError> {
            Ok(Box::new(ScriptedSession(Arc::clone(&self.0))))
        }
    }

    impl EngineSession for ScriptedSession {
        fn feed(&mut self, _pcm: &[f32]) -> Result<(), TranscribeError> {
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<TranscribeEvent>, TranscribeError> {
            if let Some(event) = self.0.events.lock().unwrap().pop_front() {
                return Ok(Some(event));
            }
            if let Some(error) = self.0.failure.lock().unwrap().take() {
                return Err(error);
            }
            Ok(None)
        }

        fn finish(&mut self) -> Result<(), TranscribeError> {
            let finished: Vec<_> = self.0.on_finish.lock().unwrap().drain(..).collect();
            self.0.events.lock().unwrap().extend(finished);
            Ok(())
        }
    }

    fn live(script: Arc<Script>) -> LiveSession {
        Transcriber::with_engine(Arc::new(ScriptedEngine(script)))
            .live(AudioSpec::new(ENGINE_RATE, 1).unwrap(), &Options::new())
            .unwrap()
    }

    fn committed(text: &str, start_ms: u64) -> TranscribeEvent {
        TranscribeEvent::Committed {
            text: text.into(),
            start_ms,
            end_ms: start_ms + 100,
        }
    }

    fn source_transcript(live: LiveSession, base: SystemTime, emitted: usize) -> SourceTranscript {
        SourceTranscript {
            source: Source::Microphone,
            live,
            base,
            emitted,
        }
    }

    fn drain_until(
        session: &LiveSession,
        sender: &Sender<EngineEvent>,
        base: SystemTime,
        emitted: &mut usize,
        expected: usize,
    ) -> Result<(), TranscribeError> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            drain(session, sender, Source::Microphone, base, emitted)?;
            if *emitted >= expected {
                return Ok(());
            }
            assert!(
                Instant::now() < deadline,
                "timed out after {emitted} events"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn drain_until_error(
        session: &LiveSession,
        sender: &Sender<EngineEvent>,
        base: SystemTime,
        emitted: &mut usize,
    ) -> TranscribeError {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Err(error) = drain(session, sender, Source::Microphone, base, emitted) {
                return error;
            }
            assert!(Instant::now() < deadline, "timed out waiting for the error");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn reported(events: &Receiver<EngineEvent>) -> Vec<EngineEvent> {
        events.try_iter().collect()
    }

    /// The committed utterances, ignoring the empty draft that closes a source.
    fn transcripts(events: Vec<EngineEvent>) -> Vec<EngineEvent> {
        events
            .into_iter()
            .filter(|event| matches!(event, EngineEvent::Transcript { .. }))
            .collect()
    }

    #[test]
    fn a_commit_during_the_take_is_reported_once() {
        let script = Arc::new(Script::default());
        script
            .events
            .lock()
            .unwrap()
            .push_back(committed("hello", 100));
        let session = live(Arc::clone(&script));
        let (sender, events) = mpsc::channel();
        let base = SystemTime::now();
        let mut emitted = 0;
        session.feed(&[0.5; 160]).unwrap();
        drain_until(&session, &sender, base, &mut emitted, 1).unwrap();
        finish_transcription(source_transcript(session, base, emitted), &sender);
        drop(sender);
        let events = transcripts(reported(&events));
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], EngineEvent::Transcript { text, .. } if text == "hello"));
    }

    #[test]
    fn a_volatile_tail_is_reported_as_a_draft() {
        let script = Arc::new(Script::default());
        script
            .events
            .lock()
            .unwrap()
            .push_back(TranscribeEvent::Volatile { text: "hel".into() });
        let session = live(script);
        let (sender, events) = mpsc::channel();
        let base = SystemTime::now();
        let mut emitted = 0;
        session.feed(&[0.5; 160]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let draft = loop {
            drain(&session, &sender, Source::Microphone, base, &mut emitted).unwrap();
            if let Ok(event) = events.try_recv() {
                break event;
            }
            assert!(Instant::now() < deadline, "timed out waiting for the draft");
            thread::sleep(Duration::from_millis(1));
        };
        assert!(matches!(&draft, EngineEvent::Draft { text, .. } if text == "hel"));
        assert_eq!(emitted, 0, "a draft is not a commit");
    }

    #[test]
    fn a_commit_reported_at_finish_is_not_lost() {
        let script = Arc::new(Script::default());
        script
            .on_finish
            .lock()
            .unwrap()
            .push_back(committed("late", 500));
        let session = live(script);
        let (sender, events) = mpsc::channel();
        let base = SystemTime::now();
        let mut emitted = 0;
        session.feed(&[0.5; 160]).unwrap();
        drain(&session, &sender, Source::Microphone, base, &mut emitted).unwrap();
        assert_eq!(emitted, 0);
        finish_transcription(source_transcript(session, base, emitted), &sender);
        drop(sender);
        let events = transcripts(reported(&events));
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], EngineEvent::Transcript { text, .. } if text == "late"));
    }

    #[test]
    fn a_failed_session_keeps_the_text_it_already_committed() {
        let script = Arc::new(Script::default());
        script
            .events
            .lock()
            .unwrap()
            .push_back(committed("kept", 0));
        *script.failure.lock().unwrap() = Some(TranscribeError::Backend {
            operation: "decode",
            code: 3,
        });
        let session = live(script);
        let (sender, events) = mpsc::channel();
        let base = SystemTime::now();
        let mut emitted = 0;
        session.feed(&[0.5; 160]).unwrap();
        let _ = drain_until(&session, &sender, base, &mut emitted, 1);
        assert_eq!(emitted, 1);
        let _ = session.feed(&[0.5; 160]);
        drain_until_error(&session, &sender, base, &mut emitted);
        assert_eq!(emitted, 1);
        let exits = [
            SourceExit {
                transcript: None,
                waiting: false,
                spoke: emitted > 0,
            },
            SourceExit {
                transcript: None,
                waiting: true,
                spoke: false,
            },
        ];
        assert_eq!(no_transcript_notice(&exits), None);
        drop(sender);
        let events = transcripts(reported(&events));
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], EngineEvent::Transcript { text, .. } if text == "kept"));
    }

    #[test]
    fn a_take_without_a_transcript_reports_it() {
        let exits = [
            SourceExit {
                transcript: None,
                waiting: true,
                spoke: false,
            },
            SourceExit {
                transcript: None,
                waiting: false,
                spoke: false,
            },
        ];
        assert_eq!(no_transcript_notice(&exits), Some(NO_TRANSCRIPT));
    }

    #[test]
    fn a_whitespace_commit_is_reported_but_not_retained() {
        let script = Arc::new(Script::default());
        script.events.lock().unwrap().push_back(committed("   ", 0));
        let session = live(script);
        let (sender, events) = mpsc::channel();
        let base = SystemTime::now();
        let mut emitted = 0;
        session.feed(&[0.5; 160]).unwrap();
        drain_until(&session, &sender, base, &mut emitted, 1).unwrap();
        finish_transcription(source_transcript(session, base, emitted), &sender);
        drop(sender);
        let events = transcripts(reported(&events));
        assert_eq!(events.len(), 1);
        let EngineEvent::Transcript { at, text, .. } = &events[0] else {
            unreachable!("the drain only reports transcript events");
        };
        let mut transcript = MeetingTranscript::default();
        transcript.push(Speaker::of(Source::Microphone), *at, text);
        assert!(transcript.is_empty());
    }

    #[test]
    fn a_full_backlog_of_one_block_advances_the_clock_by_that_block() {
        let mut backlog = vec![0.0_f32; BACKLOG_LIMIT];
        backlog[0] = 9.0;
        let block = vec![1.0_f32; BLOCK_FRAMES];
        let dropped = push_bounded(&mut backlog, &block, BACKLOG_LIMIT);
        assert_eq!(dropped, BLOCK_FRAMES);
        assert_eq!(backlog.len(), BACKLOG_LIMIT);
        assert!(!backlog.contains(&9.0));
        assert_eq!(backlog[BACKLOG_LIMIT - 1], 1.0);
        assert_eq!(audio_ms(dropped, 1, SAMPLE_RATE), 20);
        assert_eq!(audio_ms(dropped * 2, 2, SAMPLE_RATE), 20);
    }

    #[test]
    fn a_plan_with_no_source_is_rejected() {
        assert!(matches!(
            SessionPlan::meeting(PathBuf::from("take.ogg"), Sources::EMPTY),
            Err(DesktopError::NoSources)
        ));
    }

    #[test]
    fn peak_tracks_the_largest_absolute_sample() {
        assert!((peak(&[0.1, -0.8, 0.3]) - 0.8).abs() < f32::EPSILON);
        assert_eq!(peak(&[]), 0.0);
    }
}
