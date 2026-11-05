//! The handle a caller keeps.

use std::sync::Arc;

use koe_transcribe_core::{AudioSpec, Engine, Error, Incomplete, Model, Options, Transcript};

use crate::live::LiveSession;
use crate::platform;

/// A configured transcription engine.
///
/// [`new`](Self::new) loads the model once; sessions are cheap and can be run
/// side by side, each on its own thread.
pub struct Transcriber {
    engine: Arc<dyn Engine>,
}

impl Transcriber {
    /// Prepares the backend for `model`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] when no backend is enabled for this
    /// target, and the backend's error when the model cannot be loaded.
    pub fn new(model: &Model) -> Result<Self, Error> {
        let engine = platform::engine(model)?;
        Ok(Self {
            engine: Arc::from(engine),
        })
    }

    /// Transcribes a complete buffer.
    ///
    /// # Errors
    ///
    /// Returns [`Incomplete`] when the session cannot start or the engine
    /// fails; the text committed before the failure travels with the error.
    pub fn transcribe(
        &self,
        pcm: &[f32],
        spec: AudioSpec,
        options: &Options,
    ) -> Result<Transcript, Incomplete> {
        let session = self.live(spec, options).map_err(incomplete)?;
        session.feed(pcm).map_err(incomplete)?;
        session.finish()
    }

    /// Opens a streaming session.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Transcriber::new`] plus anything the
    /// backend reports while it opens a session.
    pub fn live(&self, spec: AudioSpec, options: &Options) -> Result<LiveSession, Error> {
        LiveSession::start(self.engine.as_ref(), spec, options)
    }

    /// Builds a transcriber over an engine the caller supplies.
    ///
    /// [`new`](Self::new) is the usual constructor. This one lets an embedder
    /// provide its own [`Engine`] implementation.
    #[must_use]
    pub fn with_engine(engine: Arc<dyn Engine>) -> Self {
        Self { engine }
    }

    #[cfg(test)]
    pub fn from_engine(engine: Arc<dyn Engine>) -> Self {
        Self::with_engine(engine)
    }
}

fn incomplete(error: Error) -> Incomplete {
    Incomplete::new(error, Transcript::default())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Condvar, Mutex};
    use std::time::{Duration, Instant};

    use koe_transcribe_core::{ENGINE_RATE, EngineEvent, EngineSession, Event};

    use super::*;

    const WAIT: Duration = Duration::from_secs(2);

    struct Gate {
        open: Mutex<bool>,
        changed: Condvar,
    }

    impl Default for Gate {
        fn default() -> Self {
            Self {
                open: Mutex::new(true),
                changed: Condvar::new(),
            }
        }
    }

    impl Gate {
        fn hold(&self) {
            *self.open.lock().unwrap() = false;
        }

        fn wait(&self) {
            let open = self.open.lock().unwrap();
            drop(self.changed.wait_while(open, |open| !*open).unwrap());
        }

        fn release(&self) {
            *self.open.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }

    #[derive(Default)]
    struct Inner {
        events: Mutex<VecDeque<EngineEvent>>,
        feeds: Mutex<Vec<usize>>,
        silence_frames: AtomicU64,
        failure: Mutex<Option<Error>>,
        panic_on_feed: AtomicBool,
        gate: Gate,
        started: Mutex<Option<Sender<()>>>,
    }

    struct Mock {
        inner: Arc<Inner>,
    }

    impl Mock {
        fn scripted(events: impl IntoIterator<Item = EngineEvent>) -> Self {
            Self {
                inner: Arc::new(Inner {
                    events: Mutex::new(events.into_iter().collect()),
                    ..Inner::default()
                }),
            }
        }

        fn failing(events: impl IntoIterator<Item = EngineEvent>, error: Error) -> Self {
            let mock = Self::scripted(events);
            *mock.inner.failure.lock().unwrap() = Some(error);
            mock
        }

        fn panicking() -> Self {
            let mock = Self::scripted(std::iter::empty());
            mock.inner.panic_on_feed.store(true, Ordering::Release);
            mock
        }

        fn gated() -> (Self, Receiver<()>) {
            let (sender, receiver) = mpsc::channel();
            let mock = Self::scripted(std::iter::empty());
            mock.inner.gate.hold();
            *mock.inner.started.lock().unwrap() = Some(sender);
            (mock, receiver)
        }
    }

    struct Session {
        inner: Arc<Inner>,
    }

    impl Engine for Mock {
        fn new_session(&self, _options: &Options) -> Result<Box<dyn EngineSession>, Error> {
            Ok(Box::new(Session {
                inner: Arc::clone(&self.inner),
            }))
        }
    }

    impl EngineSession for Session {
        fn feed(&mut self, pcm: &[f32]) -> Result<(), Error> {
            assert!(
                !self.inner.panic_on_feed.load(Ordering::Acquire),
                "scripted engine failure"
            );
            if let Some(sender) = self.inner.started.lock().unwrap().take() {
                let _ = sender.send(());
            }
            self.inner.gate.wait();
            self.inner.feeds.lock().unwrap().push(pcm.len());
            if !pcm.is_empty() && pcm.iter().all(|sample| *sample == 0.0) {
                self.inner
                    .silence_frames
                    .fetch_add(u64::try_from(pcm.len()).unwrap(), Ordering::AcqRel);
            }
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<EngineEvent>, Error> {
            {
                let mut events = self.inner.events.lock().unwrap();
                if let Some(event) = events.pop_front() {
                    return Ok(Some(event));
                }
            }
            self.inner
                .failure
                .lock()
                .unwrap()
                .take()
                .map_or(Ok(None), Err)
        }

        fn finish(&mut self) -> Result<(), Error> {
            Ok(())
        }
    }

    fn mono() -> AudioSpec {
        AudioSpec::new(ENGINE_RATE, 1).unwrap()
    }

    fn committed(text: &str, start_ms: u64, end_ms: u64) -> EngineEvent {
        EngineEvent::Committed {
            text: text.into(),
            start_ms,
            end_ms,
        }
    }

    fn volatile(text: &str) -> EngineEvent {
        EngineEvent::Volatile { text: text.into() }
    }

    fn with_engine(mock: Mock) -> Transcriber {
        Transcriber::from_engine(Arc::new(mock))
    }

    fn wait_for_events(session: &LiveSession, count: usize) -> Vec<Event> {
        let deadline = Instant::now() + WAIT;
        let mut events = Vec::new();
        while events.len() < count {
            assert!(Instant::now() < deadline, "timed out after {events:?}");
            if let Ok(Some(event)) = session.poll() {
                events.push(event);
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        events
    }

    fn wait_for_error(session: &LiveSession) -> Error {
        let deadline = Instant::now() + WAIT;
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for a failure");
            if let Err(error) = session.poll() {
                return error;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn committed_and_volatile_events_reach_the_caller() {
        let mock = Mock::scripted([volatile("hel"), committed("hello", 0, 500)]);
        let session = with_engine(mock).live(mono(), &Options::new()).unwrap();
        session.feed(&[0.5; 160]).unwrap();

        let events = wait_for_events(&session, 2);
        assert_eq!(
            events[0],
            Event::Volatile {
                text: "hel".into(),
                revision: 1,
            }
        );
        assert_eq!(
            events[1],
            Event::Committed {
                text: "hello".into(),
                start_ms: 0,
                end_ms: 500,
            }
        );
        assert!(session.poll().unwrap().is_none());
        assert_eq!(session.dropped_ms(), 0);
        assert_eq!(session.finish().unwrap().text(), "hello");
    }

    #[test]
    fn each_volatile_event_carries_a_new_revision() {
        let mock = Mock::scripted([volatile("a"), volatile("ab"), volatile("abc")]);
        let session = with_engine(mock).live(mono(), &Options::new()).unwrap();
        session.feed(&[0.5; 160]).unwrap();

        let events = wait_for_events(&session, 3);
        let revisions: Vec<u64> = events
            .iter()
            .map(|event| match event {
                Event::Volatile { revision, .. } => *revision,
                _ => unreachable!("scripted only volatile events"),
            })
            .collect();
        assert_eq!(revisions, [1, 2, 3]);
        session.finish().unwrap();
    }

    #[test]
    fn finish_assembles_segments_from_committed_text() {
        let mock = Mock::scripted([committed("hello", 100, 600), committed("world", 600, 1_100)]);
        let session = with_engine(mock).live(mono(), &Options::new()).unwrap();
        session.feed(&[0.5; 160]).unwrap();

        let transcript = session.finish().unwrap();
        assert_eq!(transcript.text(), "hello world");
        assert_eq!(transcript.segments().len(), 2);
        assert_eq!(transcript.segments()[0].start_ms(), 100);
        assert_eq!(transcript.segments()[0].end_ms(), 600);
        assert_eq!(transcript.segments()[1].start_ms(), 600);
        assert_eq!(transcript.segments()[1].end_ms(), 1_100);
        assert_eq!(transcript.dropped_ms(), 0);
    }

    #[test]
    fn transcribe_runs_the_whole_buffer_through_the_session() {
        let mock = Mock::scripted([committed("spoken", 200, 1_000)]);
        let transcriber = with_engine(mock);
        let transcript = transcriber
            .transcribe(&vec![0.25; 16_000], mono(), &Options::new())
            .unwrap();
        assert_eq!(transcript.text(), "spoken");
    }

    #[test]
    fn an_empty_buffer_transcribes_to_nothing() {
        let transcriber = with_engine(Mock::scripted(std::iter::empty()));
        let transcript = transcriber
            .transcribe(&[], mono(), &Options::new())
            .unwrap();
        assert!(transcript.is_empty());
    }

    #[test]
    fn a_misaligned_buffer_is_rejected_before_conversion() {
        let session = with_engine(Mock::scripted(std::iter::empty()))
            .live(AudioSpec::new(48_000, 2).unwrap(), &Options::new())
            .unwrap();
        assert_eq!(
            session.feed(&[0.0; 3]),
            Err(Error::MisalignedBuffer {
                len: 3,
                channels: 2,
            })
        );
        session.finish().unwrap();
    }

    #[test]
    fn a_zero_length_feed_is_accepted() {
        let session = with_engine(Mock::scripted(std::iter::empty()))
            .live(mono(), &Options::new())
            .unwrap();
        assert_eq!(session.feed(&[]), Ok(()));
        session.finish().unwrap();
    }

    #[test]
    fn a_full_queue_drops_audio_and_replays_it_as_silence() {
        let (mock, started) = Mock::gated();
        let silence = Arc::clone(&mock.inner);
        let session = with_engine(mock).live(mono(), &Options::new()).unwrap();

        session.feed(&[0.5; 1_600]).unwrap();
        started.recv_timeout(WAIT).unwrap();
        for _ in 0..64 {
            session.feed(&[0.5; 1_600]).unwrap();
        }
        assert_eq!(session.dropped_ms(), 0);

        for _ in 0..5 {
            session.feed(&[0.5; 1_600]).unwrap();
        }
        assert_eq!(session.dropped_ms(), 500);

        silence.gate.release();
        let transcript = session.finish().unwrap();
        assert_eq!(transcript.dropped_ms(), 500);
        assert_eq!(silence.silence_frames.load(Ordering::Acquire), 8_000);
    }

    #[test]
    fn a_failure_is_latched_after_the_queued_events() {
        let error = Error::Backend {
            operation: "decode",
            code: -3,
        };
        let mock = Mock::failing([committed("kept", 0, 400)], error.clone());
        let session = with_engine(mock).live(mono(), &Options::new()).unwrap();
        session.feed(&[0.5; 160]).unwrap();

        assert_eq!(
            wait_for_events(&session, 1),
            [Event::Committed {
                text: "kept".into(),
                start_ms: 0,
                end_ms: 400,
            }]
        );
        assert_eq!(wait_for_error(&session), error);
        assert_eq!(session.poll(), Err(error.clone()));

        let incomplete = session.finish().unwrap_err();
        assert_eq!(incomplete.error(), &error);
        assert_eq!(incomplete.transcript().text(), "kept");
    }

    #[test]
    fn a_panicking_engine_reports_a_backend_invariant() {
        let session = with_engine(Mock::panicking())
            .live(mono(), &Options::new())
            .unwrap();
        session.feed(&[0.5; 160]).unwrap();

        let incomplete = session.finish().unwrap_err();
        assert_eq!(
            incomplete.error(),
            &Error::BackendInvariant {
                operation: "session task",
            }
        );
        assert!(incomplete.transcript().is_empty());
    }

    #[cfg(not(any(feature = "linux", feature = "macos", feature = "windows")))]
    #[test]
    fn a_build_without_a_platform_feature_reports_unsupported() {
        assert!(matches!(
            Transcriber::new(&Model::gguf("/models/tiny.gguf")),
            Err(Error::Unsupported)
        ));
        assert!(matches!(
            Transcriber::new(&Model::system_speech()),
            Err(Error::Unsupported)
        ));
    }
}
