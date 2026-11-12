//! Session plumbing over the shim C ABI, testable without macOS.

use std::collections::VecDeque;
use std::ffi::{CStr, CString};

use koe_transcribe_core::{EngineEvent, EngineSession, Error, Options};

/// One committed phrase the shim handed back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedPhrase {
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// A volatile tail that changed since the previous take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolatileTail {
    pub text: String,
    pub revision: u32,
}

/// The shim's C ABI, trait-shaped so hosts without Swift can script it.
pub trait Shim: Clone + Send + Sync + 'static {
    /// One recognition session's handle.
    type Handle: Send;

    /// Opens a recognition session.
    ///
    /// `live` asks for the tentative tail at the shim's lowest latency.
    ///
    /// # Errors
    ///
    /// Returns the shim's status when the session cannot start.
    fn create(&self, locale: Option<&CStr>, live: bool) -> Result<Self::Handle, i32>;

    /// Appends 16 kHz mono samples.
    ///
    /// # Errors
    ///
    /// Returns the shim's status when the engine rejects the audio.
    fn feed(&self, handle: &mut Self::Handle, pcm: &[f32]) -> Result<(), i32>;

    /// Pops the next committed phrase, if one is queued.
    ///
    /// # Errors
    ///
    /// Returns the shim's status when the engine failed.
    fn take_committed(&self, handle: &mut Self::Handle) -> Result<Option<CommittedPhrase>, i32>;

    /// Returns the volatile tail, if it changed since the previous take.
    ///
    /// # Errors
    ///
    /// Returns the shim's status when the engine failed.
    fn take_volatile(&self, handle: &mut Self::Handle) -> Result<Option<VolatileTail>, i32>;

    /// Ends the stream and flushes what the engine still holds.
    ///
    /// # Errors
    ///
    /// Returns the shim's status when the flush fails or runs past its bound.
    fn finish(&self, handle: &mut Self::Handle) -> Result<(), i32>;

    /// Consumes the handle and tears the session down.
    fn destroy(&self, handle: Self::Handle);
}

/// Engine that opens [`Session`]s over a shim.
pub struct Engine<S: Shim> {
    shim: S,
}

impl<S: Shim> Engine<S> {
    /// Wraps `shim`.
    pub const fn new(shim: S) -> Self {
        Self { shim }
    }
}

impl<S: Shim> koe_transcribe_core::Engine for Engine<S> {
    fn new_session(&self, options: &Options) -> Result<Box<dyn EngineSession>, Error> {
        let requested = options.language_tag();
        let locale = requested
            .map(CString::new)
            .transpose()
            .map_err(|_| Error::OptionsInvalid { option: "language" })?;
        let handle = self
            .shim
            .create(locale.as_deref(), options.is_live())
            .map_err(|status| create_error(status, requested))?;
        Ok(Box::new(Session {
            shim: self.shim.clone(),
            handle: Some(handle),
            pending: VecDeque::new(),
        }))
    }
}

/// One recognition pass over a shim handle.
pub struct Session<S: Shim> {
    shim: S,
    handle: Option<S::Handle>,
    /// Committed phrases drained off the shim ahead of their polls.
    pending: VecDeque<EngineEvent>,
}

impl<S: Shim> EngineSession for Session<S> {
    fn feed(&mut self, pcm: &[f32]) -> Result<(), Error> {
        if pcm.is_empty() {
            return Ok(());
        }
        let Self { shim, handle, .. } = self;
        let Some(handle) = handle.as_mut() else {
            return Err(Error::BackendInvariant {
                operation: "engine handle",
            });
        };
        shim.feed(handle, pcm)
            .map_err(|status| status_error("feed", status))
    }

    fn poll(&mut self) -> Result<Option<EngineEvent>, Error> {
        let Self {
            shim,
            handle,
            pending,
        } = self;
        let Some(handle) = handle.as_mut() else {
            return Err(Error::BackendInvariant {
                operation: "engine handle",
            });
        };
        // Committed phrases clear before the volatile tail so a final never
        // trails the provisional text it replaced.
        if pending.is_empty() {
            loop {
                match shim.take_committed(handle) {
                    Ok(Some(phrase)) => pending.push_back(phrase_event(phrase)?),
                    Ok(None) => break,
                    Err(status) => return Err(status_error("take_committed", status)),
                }
            }
        }
        if let Some(event) = pending.pop_front() {
            return Ok(Some(event));
        }
        match shim.take_volatile(handle) {
            Ok(Some(tail)) => Ok(Some(EngineEvent::Volatile {
                text: tail.text.into_boxed_str(),
            })),
            Ok(None) => Ok(None),
            Err(status) => Err(status_error("take_volatile", status)),
        }
    }

    fn finish(&mut self) -> Result<(), Error> {
        let Self { shim, handle, .. } = self;
        let Some(handle) = handle.as_mut() else {
            return Err(Error::BackendInvariant {
                operation: "engine handle",
            });
        };
        shim.finish(handle)
            .map_err(|status| status_error("finish", status))
    }
}

impl<S: Shim> Drop for Session<S> {
    fn drop(&mut self) {
        let Self { shim, handle, .. } = self;
        if let Some(handle) = handle.take() {
            shim.destroy(handle);
        }
    }
}

/// Maps `create`'s status onto the session-open errors.
fn create_error(status: i32, language: Option<&str>) -> Error {
    match status {
        -1 => Error::Unavailable {
            feature: "SpeechAnalyzer",
        },
        -2 => Error::OptionsInvalid { option: "language" },
        -3 => Error::ModelMissing {
            language: language.unwrap_or("system locale").into(),
        },
        -5 => Error::BackendInvariant {
            operation: "create",
        },
        code => Error::Backend {
            operation: "create",
            code,
        },
    }
}

/// Maps a status from an already-open session onto the engine errors.
const fn status_error(operation: &'static str, status: i32) -> Error {
    if status == -5 {
        Error::BackendInvariant { operation }
    } else {
        Error::Backend {
            operation,
            code: status,
        }
    }
}

/// Converts a committed phrase, including its engine timeline offset.
fn phrase_event(phrase: CommittedPhrase) -> Result<EngineEvent, Error> {
    let start_ms = u64::try_from(phrase.start_ms).map_err(|_| Error::BackendInvariant {
        operation: "committed range",
    })?;
    let end_ms = u64::try_from(phrase.end_ms).map_err(|_| Error::BackendInvariant {
        operation: "committed range",
    })?;
    Ok(EngineEvent::Committed {
        text: phrase.text.into_boxed_str(),
        start_ms,
        end_ms,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use koe_transcribe_core::Engine as CoreEngine;

    use super::*;

    /// One session's recorded calls and canned responses.
    #[derive(Default)]
    struct Inner {
        create_status: Mutex<i32>,
        feed_status: Mutex<i32>,
        creates: Mutex<usize>,
        locales: Mutex<Vec<Option<String>>>,
        lives: Mutex<Vec<bool>>,
        feeds: Mutex<Vec<Vec<f32>>>,
        committed: Mutex<VecDeque<Result<Option<CommittedPhrase>, i32>>>,
        volatile: Mutex<VecDeque<Result<Option<VolatileTail>, i32>>>,
        finishes: Mutex<VecDeque<Result<(), i32>>>,
        destroys: Mutex<usize>,
    }

    #[derive(Clone, Default)]
    struct Scripted(Arc<Inner>);

    struct Handle;

    impl Shim for Scripted {
        type Handle = Handle;

        fn create(&self, locale: Option<&CStr>, live: bool) -> Result<Handle, i32> {
            *self.0.creates.lock().unwrap() += 1;
            self.0
                .locales
                .lock()
                .unwrap()
                .push(locale.map(|tag| tag.to_string_lossy().into_owned()));
            self.0.lives.lock().unwrap().push(live);
            match *self.0.create_status.lock().unwrap() {
                0 => Ok(Handle),
                status => Err(status),
            }
        }

        fn feed(&self, _handle: &mut Handle, pcm: &[f32]) -> Result<(), i32> {
            self.0.feeds.lock().unwrap().push(pcm.to_owned());
            match *self.0.feed_status.lock().unwrap() {
                0 => Ok(()),
                status => Err(status),
            }
        }

        fn take_committed(&self, _handle: &mut Handle) -> Result<Option<CommittedPhrase>, i32> {
            self.0
                .committed
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(None))
        }

        fn take_volatile(&self, _handle: &mut Handle) -> Result<Option<VolatileTail>, i32> {
            self.0
                .volatile
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(None))
        }

        fn finish(&self, _handle: &mut Handle) -> Result<(), i32> {
            self.0
                .finishes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(()))
        }

        fn destroy(&self, _handle: Self::Handle) {
            *self.0.destroys.lock().unwrap() += 1;
        }
    }

    fn script() -> Scripted {
        Scripted::default()
    }

    /// A session built straight off a fresh handle, bypassing `create`.
    fn session(shim: &Scripted) -> Session<Scripted> {
        let handle = shim.create(None, false).unwrap();
        Session {
            shim: shim.clone(),
            handle: Some(handle),
            pending: VecDeque::new(),
        }
    }

    fn committed(text: &str, start_ms: i64, end_ms: i64) -> CommittedPhrase {
        CommittedPhrase {
            text: String::from(text),
            start_ms,
            end_ms,
        }
    }

    fn volatile(text: &str, revision: u32) -> VolatileTail {
        VolatileTail {
            text: String::from(text),
            revision,
        }
    }

    /// The engine error of a session open, if it failed.
    fn open_error(result: Result<Box<dyn EngineSession>, Error>) -> Option<Error> {
        result.err()
    }

    fn engine_event_committed(text: &str, start_ms: u64, end_ms: u64) -> EngineEvent {
        EngineEvent::Committed {
            text: text.into(),
            start_ms,
            end_ms,
        }
    }

    #[test]
    fn create_passes_the_requested_locale() {
        let shim = script();
        let engine = Engine::new(shim.clone());
        engine.new_session(&Options::new().language("ja")).unwrap();
        engine.new_session(&Options::new()).unwrap();
        assert_eq!(
            *shim.0.locales.lock().unwrap(),
            vec![Some(String::from("ja")), None]
        );
    }

    #[test]
    fn create_passes_the_requested_liveness() {
        let shim = script();
        let engine = Engine::new(shim.clone());
        engine.new_session(&Options::new().live()).unwrap();
        engine.new_session(&Options::new()).unwrap();
        assert_eq!(*shim.0.lives.lock().unwrap(), vec![true, false]);
    }

    #[test]
    fn a_locale_with_a_nul_byte_never_reaches_the_shim() {
        let shim = script();
        let engine = Engine::new(shim.clone());
        assert_eq!(
            open_error(engine.new_session(&Options::new().language("j\0a"))),
            Some(Error::OptionsInvalid { option: "language" })
        );
        assert_eq!(*shim.0.creates.lock().unwrap(), 0);
    }

    #[test]
    fn create_status_maps_to_session_errors() {
        let cases = [
            (
                -1,
                Some("ja"),
                Error::Unavailable {
                    feature: "SpeechAnalyzer",
                },
            ),
            (-2, Some("ja"), Error::OptionsInvalid { option: "language" }),
            (
                -3,
                Some("fr"),
                Error::ModelMissing {
                    language: "fr".into(),
                },
            ),
            (
                -3,
                None,
                Error::ModelMissing {
                    language: "system locale".into(),
                },
            ),
            (
                -4,
                None,
                Error::Backend {
                    operation: "create",
                    code: -4,
                },
            ),
            (
                -5,
                None,
                Error::BackendInvariant {
                    operation: "create",
                },
            ),
            (
                7,
                None,
                Error::Backend {
                    operation: "create",
                    code: 7,
                },
            ),
        ];
        for (status, language, expected) in cases {
            let shim = script();
            *shim.0.create_status.lock().unwrap() = status;
            let engine = Engine::new(shim);
            let options = language.map_or_else(Options::new, |tag| Options::new().language(tag));
            assert_eq!(open_error(engine.new_session(&options)), Some(expected));
        }
    }

    #[test]
    fn feed_forwards_whole_mono_samples() {
        let shim = script();
        let mut session = session(&shim);
        session.feed(&[0.25, -0.25, 0.5]).unwrap();
        assert_eq!(*shim.0.feeds.lock().unwrap(), vec![vec![0.25, -0.25, 0.5]]);
    }

    #[test]
    fn an_empty_feed_skips_the_shim() {
        let shim = script();
        let mut session = session(&shim);
        session.feed(&[]).unwrap();
        assert!(shim.0.feeds.lock().unwrap().is_empty());
    }

    #[test]
    fn feed_status_maps_to_engine_errors() {
        let cases = [
            (
                -4,
                Error::Backend {
                    operation: "feed",
                    code: -4,
                },
            ),
            (-5, Error::BackendInvariant { operation: "feed" }),
            (
                9,
                Error::Backend {
                    operation: "feed",
                    code: 9,
                },
            ),
        ];
        for (status, expected) in cases {
            let shim = script();
            *shim.0.feed_status.lock().unwrap() = status;
            let mut session = session(&shim);
            assert_eq!(session.feed(&[0.5]), Err(expected));
        }
    }

    #[test]
    fn committed_phrases_drain_before_the_volatile_tail() {
        let shim = script();
        *shim.0.committed.lock().unwrap() = VecDeque::from(vec![
            Ok(Some(committed("hello", 0, 500))),
            Ok(Some(committed("whole", 500, 900))),
        ]);
        *shim.0.volatile.lock().unwrap() = VecDeque::from(vec![
            Ok(Some(volatile("wor", 1))),
            Ok(Some(volatile("world", 2))),
        ]);
        let mut session = session(&shim);
        assert_eq!(
            session.poll().unwrap(),
            Some(engine_event_committed("hello", 0, 500))
        );
        assert_eq!(
            session.poll().unwrap(),
            Some(engine_event_committed("whole", 500, 900))
        );
        assert_eq!(
            session.poll().unwrap(),
            Some(EngineEvent::Volatile { text: "wor".into() })
        );
        assert_eq!(
            session.poll().unwrap(),
            Some(EngineEvent::Volatile {
                text: "world".into(),
            })
        );
        assert_eq!(session.poll().unwrap(), None);
    }

    #[test]
    fn a_volatile_tail_is_only_emitted_when_the_shim_says_it_changed() {
        let shim = script();
        *shim.0.volatile.lock().unwrap() = VecDeque::from(vec![Ok(Some(volatile("hel", 3)))]);
        let mut session = session(&shim);
        assert_eq!(
            session.poll().unwrap(),
            Some(EngineEvent::Volatile { text: "hel".into() })
        );
        assert_eq!(session.poll().unwrap(), None);
        assert_eq!(session.poll().unwrap(), None);
    }

    #[test]
    fn a_negative_timestamp_breaks_the_contract() {
        let shim = script();
        *shim.0.committed.lock().unwrap() = VecDeque::from(vec![Ok(Some(committed("x", -1, 500)))]);
        let mut session = session(&shim);
        assert_eq!(
            session.poll(),
            Err(Error::BackendInvariant {
                operation: "committed range",
            })
        );
    }

    #[test]
    fn take_status_maps_to_engine_errors() {
        let cases = [
            (
                -4,
                Error::Backend {
                    operation: "take_committed",
                    code: -4,
                },
            ),
            (
                -5,
                Error::BackendInvariant {
                    operation: "take_committed",
                },
            ),
        ];
        for (status, expected) in cases {
            let shim = script();
            *shim.0.committed.lock().unwrap() = VecDeque::from(vec![Err(status)]);
            let mut session = session(&shim);
            assert_eq!(session.poll(), Err(expected));
        }
        let shim = script();
        *shim.0.volatile.lock().unwrap() = VecDeque::from(vec![Err(-4)]);
        let mut session = session(&shim);
        assert_eq!(
            session.poll(),
            Err(Error::Backend {
                operation: "take_volatile",
                code: -4,
            })
        );
    }

    #[test]
    fn finish_status_maps_to_engine_errors() {
        let shim = script();
        *shim.0.finishes.lock().unwrap() = VecDeque::from(vec![Err(-4)]);
        let mut session = session(&shim);
        assert_eq!(
            session.finish(),
            Err(Error::Backend {
                operation: "finish",
                code: -4,
            })
        );
    }

    #[test]
    fn a_dropped_session_destroys_its_handle_once() {
        let shim = script();
        let mut session = session(&shim);
        session.feed(&[0.5]).unwrap();
        session.finish().unwrap();
        drop(session);
        assert_eq!(*shim.0.destroys.lock().unwrap(), 1);
    }

    #[test]
    fn a_session_without_finish_destroys_on_drop() {
        let shim = script();
        drop(session(&shim));
        assert_eq!(*shim.0.destroys.lock().unwrap(), 1);
    }

    #[test]
    fn sessions_and_engines_are_send() {
        fn assert_send<T: Send>(_: T) {}
        let engine = Engine::new(script());
        let session: Box<dyn EngineSession> = engine.new_session(&Options::new()).unwrap();
        assert_send(session);
        assert_send::<Engine<Scripted>>(Engine::new(script()));
    }
}
