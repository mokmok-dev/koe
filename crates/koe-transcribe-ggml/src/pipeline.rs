//! Mapping from a native recognizer onto [`EngineSession`].
//!
//! The native layer always reports the whole committed text; the session
//! remembers what the caller already received and emits the extension only.

use std::collections::VecDeque;

use koe_transcribe_core::{EngineEvent, EngineSession, Error, Transcript};

/// One recognition step as the native layer reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Update {
    /// Cumulative text the engine will not revise.
    pub committed: String,
    /// Provisional tail after `committed`.
    pub volatile: String,
    /// Offset of the first sample `committed` covers, in milliseconds.
    pub start_ms: u64,
    /// Offset one past the last sample `committed` covers, in milliseconds.
    pub end_ms: u64,
}

/// What a native recognizer can do.
pub trait Native: Send {
    /// Appends 16 kHz mono samples and reports the resulting text.
    ///
    /// # Errors
    ///
    /// Returns the backend's error when inference fails.
    fn feed(&mut self, pcm: &[f32]) -> Result<Update, Error>;

    /// Ends input and returns the final transcript.
    ///
    /// # Errors
    ///
    /// Returns the backend's error when the flush fails.
    fn finish(&mut self) -> Result<Transcript, Error>;
}

/// Which shape the native layer has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// Reports text while audio arrives.
    Streaming,
    /// Reports nothing until `finish` runs once over the buffered audio.
    Batch,
}

/// An engine session over a native recognizer.
pub struct Session<N> {
    native: N,
    shape: Shape,
    committed: String,
    volatile: String,
    revision: u64,
    end_ms: u64,
    events: VecDeque<EngineEvent>,
}

impl<N: Native> Session<N> {
    /// Wraps a recognizer that reports text while audio arrives.
    pub const fn streaming(native: N) -> Self {
        Self::new(native, Shape::Streaming)
    }

    /// Wraps a recognizer that runs once over the whole buffer.
    pub const fn batch(native: N) -> Self {
        Self::new(native, Shape::Batch)
    }

    const fn new(native: N, shape: Shape) -> Self {
        Self {
            native,
            shape,
            committed: String::new(),
            volatile: String::new(),
            revision: 0,
            end_ms: 0,
            events: VecDeque::new(),
        }
    }

    /// Ends input, queues the text the caller has not received yet, and
    /// returns the native final transcript.
    ///
    /// # Errors
    ///
    /// Returns the backend's error when the flush fails, and
    /// [`Error::BackendInvariant`] when the final text does not extend the
    /// committed prefix.
    pub fn finalize(&mut self) -> Result<Transcript, Error> {
        let transcript = self.native.finish()?;
        match self.shape {
            Shape::Streaming => self.apply_final(&transcript)?,
            Shape::Batch => {
                for segment in transcript.segments() {
                    self.events.push_back(EngineEvent::Committed {
                        text: segment.text().into(),
                        start_ms: segment.start_ms(),
                        end_ms: segment.end_ms(),
                    });
                }
            }
        }
        Ok(transcript)
    }

    fn apply(&mut self, update: Update) -> Result<(), Error> {
        if !update.committed.starts_with(self.committed.as_str()) {
            return Err(Error::BackendInvariant {
                operation: "committed text",
            });
        }
        let delta = &update.committed[self.committed.len()..];
        if !delta.is_empty() {
            self.events.push_back(EngineEvent::Committed {
                text: delta.into(),
                start_ms: update.start_ms,
                end_ms: update.end_ms,
            });
            self.committed.push_str(delta);
            self.end_ms = update.end_ms;
        }
        if update.volatile != self.volatile {
            self.revision += 1;
            self.volatile = update.volatile;
            self.events.push_back(EngineEvent::Volatile {
                text: self.volatile.as_str().into(),
            });
        }
        Ok(())
    }

    fn apply_final(&mut self, transcript: &Transcript) -> Result<(), Error> {
        let text = transcript.text();
        if !text.starts_with(self.committed.as_str()) {
            return Err(Error::BackendInvariant {
                operation: "committed text",
            });
        }
        let delta = &text[self.committed.len()..];
        if delta.is_empty() {
            return Ok(());
        }
        let end_ms = transcript
            .segments()
            .last()
            .map_or(self.end_ms, |segment| segment.end_ms().max(self.end_ms));
        self.events.push_back(EngineEvent::Committed {
            text: delta.into(),
            start_ms: self.end_ms,
            end_ms,
        });
        self.committed.push_str(delta);
        self.end_ms = end_ms;
        Ok(())
    }
}

impl<N: Native> EngineSession for Session<N> {
    fn feed(&mut self, pcm: &[f32]) -> Result<(), Error> {
        let update = self.native.feed(pcm)?;
        self.apply(update)
    }

    fn poll(&mut self) -> Result<Option<EngineEvent>, Error> {
        Ok(self.events.pop_front())
    }

    fn finish(&mut self) -> Result<(), Error> {
        self.finalize()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use koe_transcribe_core::Segment;

    use super::*;

    struct Script {
        updates: VecDeque<Result<Update, Error>>,
        outcome: Result<Transcript, Error>,
        feeds: usize,
        finishes: usize,
    }

    impl Script {
        fn streaming(updates: impl IntoIterator<Item = Update>) -> Self {
            Self {
                updates: updates.into_iter().map(Ok).collect(),
                ..Self::empty()
            }
        }

        fn empty() -> Self {
            Self {
                updates: VecDeque::new(),
                outcome: Ok(Transcript::default()),
                feeds: 0,
                finishes: 0,
            }
        }

        fn ending_in(updates: impl IntoIterator<Item = Update>, outcome: Transcript) -> Self {
            Self {
                outcome: Ok(outcome),
                ..Self::streaming(updates)
            }
        }

        fn rejecting_feed(error: Error) -> Self {
            Self {
                updates: VecDeque::from([Err(error)]),
                ..Self::empty()
            }
        }

        fn rejecting_finish(error: Error) -> Self {
            Self {
                outcome: Err(error),
                ..Self::empty()
            }
        }
    }

    impl Native for Script {
        fn feed(&mut self, _pcm: &[f32]) -> Result<Update, Error> {
            self.feeds += 1;
            self.updates
                .pop_front()
                .unwrap_or_else(|| Ok(Update::default()))
        }

        fn finish(&mut self) -> Result<Transcript, Error> {
            self.finishes += 1;
            self.outcome.clone()
        }
    }

    fn update(committed: &str, volatile: &str, start_ms: u64, end_ms: u64) -> Update {
        Update {
            committed: committed.to_owned(),
            volatile: volatile.to_owned(),
            start_ms,
            end_ms,
        }
    }

    fn transcript(segments: &[(u64, u64, &str)]) -> Transcript {
        Transcript::new(
            segments
                .iter()
                .map(|(start_ms, end_ms, text)| Segment::new(*start_ms, *end_ms, *text))
                .collect(),
            0,
        )
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

    fn drain<N: Native>(session: &mut Session<N>) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        while let Some(event) = session.poll().unwrap() {
            events.push(event);
        }
        events
    }

    #[test]
    fn committed_events_carry_only_the_extension() {
        let mut session = Session::streaming(Script::streaming([
            update("hello", "", 0, 500),
            update("hello world", "", 500, 900),
            update("hello world", "", 900, 1_200),
        ]));
        for _ in 0..3 {
            session.feed(&[0.0; 160]).unwrap();
        }
        assert_eq!(
            drain(&mut session),
            [committed("hello", 0, 500), committed(" world", 500, 900)]
        );
        assert_eq!(session.native.feeds, 3);
    }

    #[test]
    fn a_committed_rewrite_is_a_backend_invariant() {
        for rewritten in ["help", "hellp", ""] {
            let mut session = Session::streaming(Script::streaming([
                update("hello", "", 0, 500),
                update(rewritten, "", 500, 900),
            ]));
            session.feed(&[0.0; 160]).unwrap();
            assert_eq!(
                session.feed(&[0.0; 160]),
                Err(Error::BackendInvariant {
                    operation: "committed text"
                }),
                "rewrite to {rewritten:?}"
            );
        }
    }

    #[test]
    fn volatile_events_follow_the_tail_and_start_at_revision_one() {
        let mut session = Session::streaming(Script::streaming([
            update("", "hel", 0, 300),
            update("", "hel", 0, 300),
            update("", "hello", 0, 400),
            update("", "", 0, 400),
        ]));
        session.feed(&[0.0; 160]).unwrap();
        assert_eq!(session.revision, 1);
        session.feed(&[0.0; 160]).unwrap();
        assert_eq!(session.revision, 1);
        session.feed(&[0.0; 160]).unwrap();
        assert_eq!(session.revision, 2);
        session.feed(&[0.0; 160]).unwrap();
        assert_eq!(session.revision, 3);
        assert_eq!(
            drain(&mut session),
            [volatile("hel"), volatile("hello"), volatile("")]
        );
    }

    #[test]
    fn finish_drains_the_text_the_updates_did_not_commit() {
        let mut session = Session::streaming(Script::ending_in(
            [update("hello", "wor", 0, 500)],
            transcript(&[(0, 900, "hello world")]),
        ));
        session.feed(&[0.0; 160]).unwrap();
        let remaining = session.finalize().unwrap();
        assert_eq!(remaining.text(), "hello world");
        assert_eq!(session.native.finishes, 1);
        assert_eq!(
            drain(&mut session),
            [
                committed("hello", 0, 500),
                volatile("wor"),
                committed(" world", 500, 900),
            ]
        );
    }

    #[test]
    fn finish_accepts_a_final_transcript_that_adds_nothing() {
        let mut session = Session::streaming(Script::ending_in(
            [update("hello", "", 0, 500)],
            transcript(&[(0, 500, "hello")]),
        ));
        session.feed(&[0.0; 160]).unwrap();
        session.finish().unwrap();
        assert_eq!(drain(&mut session), [committed("hello", 0, 500)]);
    }

    #[test]
    fn a_final_transcript_that_rewrites_the_prefix_is_an_invariant_failure() {
        let mut session = Session::streaming(Script::ending_in(
            [update("hello", "", 0, 500)],
            transcript(&[(0, 500, "goodbye")]),
        ));
        session.feed(&[0.0; 160]).unwrap();
        assert_eq!(
            session.finalize().err(),
            Some(Error::BackendInvariant {
                operation: "committed text"
            })
        );
    }

    #[test]
    fn a_native_error_keeps_its_operation_and_code() {
        let failure = Error::Backend {
            operation: "stream feed",
            code: -5,
        };
        let mut session = Session::streaming(Script::rejecting_feed(failure.clone()));
        assert_eq!(session.feed(&[0.0; 160]), Err(failure));

        let failure = Error::Backend {
            operation: "stream finalize",
            code: -3,
        };
        let mut session = Session::streaming(Script::rejecting_finish(failure.clone()));
        assert_eq!(session.finish(), Err(failure));
    }

    #[test]
    fn batch_feed_buffers_and_finish_emits_one_event_per_segment() {
        let mut session = Session::batch(Script::ending_in(
            [],
            transcript(&[(0, 400, "hello"), (400, 900, "whole world")]),
        ));
        session.feed(&[0.0; 160]).unwrap();
        session.feed(&[0.0; 160]).unwrap();
        assert_eq!(drain(&mut session), []);
        let remaining = session.finalize().unwrap();
        assert_eq!(remaining.text(), "hello whole world");
        assert_eq!(session.native.feeds, 2);
        assert_eq!(session.native.finishes, 1);
        assert_eq!(
            drain(&mut session),
            [
                committed("hello", 0, 400),
                committed("whole world", 400, 900)
            ]
        );
    }

    #[test]
    fn batch_finish_without_speech_emits_nothing() {
        let mut session = Session::batch(Script::empty());
        session.feed(&[0.0; 160]).unwrap();
        assert_eq!(session.finalize().unwrap(), Transcript::default());
        assert_eq!(drain(&mut session), []);
    }
}
