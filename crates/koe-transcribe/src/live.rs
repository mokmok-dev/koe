//! The streaming session a caller holds.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use koe_transcribe_core::{
    AudioSpec, Converter, Engine, Error, Event, Incomplete, Options, Transcript,
};

use crate::actor::{Command, Task, duration_ms, lock};

/// Converted chunks the queue holds before it starts dropping.
const QUEUE_CHUNKS: usize = 64;

/// A live transcription session.
///
/// Audio is converted on the calling thread and handed to a session thread, so
/// [`feed`](Self::feed) never waits for inference. The session is `Send` and
/// `Sync`; a capture stream and an event poller can live on different threads.
pub struct LiveSession {
    spec: AudioSpec,
    converter: Mutex<Converter>,
    commands: SyncSender<Command>,
    events: Mutex<Receiver<Event>>,
    pending: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    latch: Arc<Mutex<Option<Error>>>,
    reply: Mutex<Receiver<Result<Transcript, Incomplete>>>,
    handle: Option<JoinHandle<()>>,
}

impl LiveSession {
    /// Opens a session on `engine`.
    ///
    /// # Errors
    ///
    /// Returns the engine's error when it cannot open a session, and
    /// [`Error::Backend`] when the session thread cannot start.
    pub fn start(engine: &dyn Engine, spec: AudioSpec, options: &Options) -> Result<Self, Error> {
        let (command_sender, commands) = mpsc::sync_channel(QUEUE_CHUNKS);
        let (events, event_receiver) = mpsc::channel();
        let (reply_sender, reply) = mpsc::sync_channel(1);
        let pending = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let latch = Arc::new(Mutex::new(None));

        let task = Task::new(
            engine.new_session(options)?,
            commands,
            events,
            Arc::clone(&pending),
            Arc::clone(&total),
            Arc::clone(&latch),
            reply_sender,
        );
        let handle = task.spawn().map_err(|error| Error::Backend {
            operation: "spawn session task",
            code: error.raw_os_error().unwrap_or_default(),
        })?;

        Ok(Self {
            spec,
            converter: Mutex::new(Converter::new(spec)),
            commands: command_sender,
            events: Mutex::new(event_receiver),
            pending,
            total,
            latch,
            reply: Mutex::new(reply),
            handle: Some(handle),
        })
    }

    /// Appends interleaved samples in the session's [`AudioSpec`].
    ///
    /// Returns as soon as the converted chunk is queued. When the queue is
    /// full the chunk is dropped and counted in [`dropped_ms`](Self::dropped_ms).
    ///
    /// # Errors
    ///
    /// Returns [`Error::MisalignedBuffer`] when the slice does not hold whole
    /// frames, and the engine's error once the session has failed.
    pub fn feed(&self, samples: &[f32]) -> Result<(), Error> {
        if samples.is_empty() {
            return Ok(());
        }
        let channels = usize::from(self.spec.channels());
        if !samples.len().is_multiple_of(channels) {
            return Err(Error::MisalignedBuffer {
                len: samples.len(),
                channels,
            });
        }

        let mut mono = Vec::new();
        {
            let mut converter = lock(&self.converter);
            converter.push(samples, &mut mono);
        }
        if mono.is_empty() {
            return Ok(());
        }
        let frames = u64::try_from(mono.len()).unwrap_or(u64::MAX);

        match self.commands.try_send(Command::Audio(mono)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.pending.fetch_add(frames, Ordering::AcqRel);
                self.total.fetch_add(frames, Ordering::AcqRel);
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => {
                Err(self.latched_error().unwrap_or(Error::BackendInvariant {
                    operation: "session task",
                }))
            }
        }
    }

    /// Takes the next recognition event.
    ///
    /// Queued events are returned before a latched failure.
    ///
    /// # Errors
    ///
    /// Returns the engine's error once the session has failed.
    pub fn poll(&self) -> Result<Option<Event>, Error> {
        {
            let events = lock(&self.events);
            if let Ok(event) = events.try_recv() {
                return Ok(Some(event));
            }
        }
        self.latched_error().map_or(Ok(None), Err)
    }

    /// Audio the queue dropped instead of transcribing, in milliseconds.
    #[must_use]
    pub fn dropped_ms(&self) -> u64 {
        duration_ms(self.total.load(Ordering::Acquire))
    }

    /// Ends the session and waits for the transcript.
    ///
    /// Blocks until the session thread finishes. A native call that never
    /// returns inside the engine keeps it blocked.
    ///
    /// # Errors
    ///
    /// Returns [`Incomplete`] when the engine failed after committing text;
    /// the committed transcript travels with the error.
    pub fn finish(mut self) -> Result<Transcript, Incomplete> {
        let _ = self.commands.send(Command::End);
        drop(self.commands);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }

        let reply = self
            .reply
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        reply.recv().unwrap_or_else(|_| {
            Err(Incomplete::new(
                Error::BackendInvariant {
                    operation: "session task",
                },
                Transcript::default(),
            ))
        })
    }

    fn latched_error(&self) -> Option<Error> {
        lock(&self.latch).clone()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_live_session_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LiveSession>();
    }
}
