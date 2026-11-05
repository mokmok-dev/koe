//! The per-session thread that owns the engine session.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use koe_transcribe_core::{
    ENGINE_RATE, EngineEvent, EngineSession, Error, Event, Incomplete, Segment, Transcript,
};

/// Samples fed per call when replaying audio the queue dropped.
const SILENCE_BLOCK: usize = 1_024;

/// One message from the session handle to the task.
pub enum Command {
    /// Converted 16 kHz mono samples.
    Audio(Vec<f32>),
    /// No further audio follows.
    End,
}

/// Acquires `mutex`, ignoring poisoning.
///
/// A panic while a lock is held leaves no state the crate trusts less than the
/// data it already guards, so the poisoned value is used as is.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Milliseconds covered by `frames` samples.
#[must_use]
pub fn duration_ms(frames: u64) -> u64 {
    frames * 1_000 / u64::from(ENGINE_RATE)
}

/// Everything the session thread needs to run to completion.
pub struct Task {
    pub session: Box<dyn EngineSession>,
    pub commands: Receiver<Command>,
    pub events: Sender<Event>,
    pub pending: Arc<AtomicU64>,
    pub total: Arc<AtomicU64>,
    pub latch: Arc<Mutex<Option<Error>>>,
    pub reply: SyncSender<Result<Transcript, Incomplete>>,
    segments: Vec<Segment>,
    revision: u64,
}

impl Task {
    /// Wires a session to the channels the handle drives it with.
    pub fn new(
        session: Box<dyn EngineSession>,
        commands: Receiver<Command>,
        events: Sender<Event>,
        pending: Arc<AtomicU64>,
        total: Arc<AtomicU64>,
        latch: Arc<Mutex<Option<Error>>>,
        reply: SyncSender<Result<Transcript, Incomplete>>,
    ) -> Self {
        Self {
            session,
            commands,
            events,
            pending,
            total,
            latch,
            reply,
            segments: Vec::new(),
            revision: 0,
        }
    }

    /// Starts the thread. It always sends exactly one result before it
    /// returns, on every exit path.
    ///
    /// # Errors
    ///
    /// Returns the operating system's error when the thread cannot start.
    pub fn spawn(self) -> std::io::Result<JoinHandle<()>> {
        std::thread::Builder::new()
            .name("koe-transcribe".to_owned())
            .spawn(move || self.execute())
    }

    fn execute(mut self) {
        let outcome = catch_unwind(AssertUnwindSafe(|| self.run()));
        let transcript = Transcript::new(std::mem::take(&mut self.segments), self.dropped_ms());
        let outcome = match outcome {
            Ok(Ok(())) => Ok(transcript),
            Ok(Err(error)) => {
                *lock(&self.latch) = Some(error.clone());
                Err(Incomplete::new(error, transcript))
            }
            Err(_) => Err(Incomplete::new(
                Error::BackendInvariant {
                    operation: "session task",
                },
                transcript,
            )),
        };
        let _ = self.reply.send(outcome);
    }

    fn run(&mut self) -> Result<(), Error> {
        loop {
            match self.commands.recv() {
                Ok(Command::Audio(chunk)) => {
                    self.replay_dropped()?;
                    self.session.feed(&chunk)?;
                    self.drain()?;
                }
                Ok(Command::End) => {
                    self.replay_dropped()?;
                    let finished = self.session.finish();
                    let drained = self.drain();
                    finished?;
                    return drained;
                }
                Err(_) => return Ok(()),
            }
        }
    }

    /// Feeds silence for audio the queue dropped, so engine timestamps keep
    /// matching the recording.
    fn replay_dropped(&mut self) -> Result<(), Error> {
        let mut remaining = self.pending.swap(0, Ordering::AcqRel);
        if remaining == 0 {
            return Ok(());
        }
        let silence = [0.0_f32; SILENCE_BLOCK];
        while remaining > 0 {
            let len = usize::try_from(remaining.min(SILENCE_BLOCK as u64)).unwrap_or(SILENCE_BLOCK);
            self.session.feed(&silence[..len])?;
            remaining = remaining.saturating_sub(u64::try_from(len).unwrap_or(remaining));
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<(), Error> {
        while let Some(event) = self.session.poll()? {
            match event {
                EngineEvent::Committed {
                    text,
                    start_ms,
                    end_ms,
                } => {
                    self.segments
                        .push(Segment::new(start_ms, end_ms, text.clone()));
                    let _ = self.events.send(Event::Committed {
                        text,
                        start_ms,
                        end_ms,
                    });
                }
                EngineEvent::Volatile { text } => {
                    self.revision += 1;
                    let _ = self.events.send(Event::Volatile {
                        text,
                        revision: self.revision,
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn dropped_ms(&self) -> u64 {
        duration_ms(self.total.load(Ordering::Acquire))
    }
}
