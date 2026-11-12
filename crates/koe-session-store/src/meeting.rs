//! A stored meeting and the operations a caller performs on it.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use koe_session::{Checkpoint, Permissions, Session};

use crate::error::Error;
use crate::id::SessionId;
use crate::record::{self, Record};
use crate::text;
use crate::transcripts::Transcripts;

/// One meeting in the store.
///
/// The handle mirrors the record on disk: every method reads what the last
/// successful write left, so a failed write never makes the handle claim state
/// the next [`Store::load`][crate::Store::load] will not find.
#[derive(Debug)]
pub struct Meeting {
    pub(crate) id: SessionId,
    pub(crate) directory: PathBuf,
    pub(crate) record: Record,
}

impl Meeting {
    /// The meeting's id.
    #[must_use]
    pub const fn id(&self) -> SessionId {
        self.id
    }

    /// The directory that holds this meeting.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The instant the meeting was created, derived from the id.
    #[must_use]
    pub fn created_at(&self) -> SystemTime {
        UNIX_EPOCH
            .checked_add(Duration::from_millis(self.id.as_millis()))
            .unwrap_or(UNIX_EPOCH)
    }

    /// The stored session restored with today's permissions, or [`None`] for a
    /// draft that never captured durable state.
    #[must_use]
    pub fn session(&self, permissions: Permissions) -> Option<Session> {
        self.record
            .checkpoint
            .map(|checkpoint| Session::restore(checkpoint, permissions))
    }

    /// The stored per-source transcripts.
    #[must_use]
    pub const fn transcripts(&self) -> &Transcripts {
        &self.record.transcripts
    }

    /// The full path of the audio file the record names, if any.
    #[must_use]
    pub fn recording(&self) -> Option<PathBuf> {
        self.record
            .recording
            .as_deref()
            .map(|name| self.directory.join(name))
    }

    /// Validates `name`, records it as this meeting's audio file, saves the
    /// record, and returns the full path inside the meeting directory.
    ///
    /// Repeating the same name is a no-op. Calling it with another name
    /// replaces the recorded name and leaves any earlier file in place.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRecording`] when `name` is not one plain file
    /// name, or names a file the store writes itself, and [`Error::Io`] when
    /// the record cannot be written.
    pub fn set_recording(&mut self, name: impl Into<Box<str>>) -> Result<PathBuf, Error> {
        let name = name.into();
        if !record::is_valid_name(&name) {
            return Err(Error::InvalidRecording);
        }
        let path = self.directory.join(&*name);
        if self.record.recording.as_deref() != Some(&*name) {
            let mut candidate = self.record.clone();
            candidate.recording = Some(name);
            record::write(&self.directory, &candidate)?;
            self.record = candidate;
        }
        Ok(path)
    }

    /// Saves `session` and `transcripts` as one record and rewrites the text
    /// rendered from it.
    ///
    /// A session with no settled checkpoint keeps the stored checkpoint.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the record cannot be written,
    /// [`Error::TextTooLarge`] when a transcript does not fit it, and
    /// [`Error::TextNotWritten`] when the record holds the transcripts but
    /// `transcript.txt` could not be written. The handle keeps reporting what
    /// the disk actually holds.
    pub fn save(&mut self, session: &Session, transcripts: &Transcripts) -> Result<(), Error> {
        let candidate = Record {
            checkpoint: Checkpoint::of(session).or(self.record.checkpoint),
            transcripts: transcripts.clone(),
            recording: self.record.recording.clone(),
        };
        self.commit(candidate)
    }

    /// Saves `transcripts` without a session, keeping the stored checkpoint.
    ///
    /// The `session` record is written first and is the source of truth;
    /// `transcript.txt` is rendered from it. A failed render therefore leaves
    /// the record the text was rendered from, and the next save rewrites the
    /// copy.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the record cannot be written,
    /// [`Error::TextTooLarge`] when a transcript does not fit it, and
    /// [`Error::TextNotWritten`] when the record holds the transcripts but
    /// `transcript.txt` could not be written. The handle keeps reporting what
    /// the disk holds.
    pub fn save_transcripts(&mut self, transcripts: &Transcripts) -> Result<(), Error> {
        let candidate = Record {
            checkpoint: self.record.checkpoint,
            transcripts: transcripts.clone(),
            recording: self.record.recording.clone(),
        };
        self.commit(candidate)
    }

    /// Writes `candidate` as the record, then rewrites the text derived from it.
    fn commit(&mut self, candidate: Record) -> Result<(), Error> {
        record::write(&self.directory, &candidate)?;
        self.record = candidate;
        text::write(&self.directory, &self.record.transcripts).map_err(|error| {
            Error::TextNotWritten {
                source: Box::new(error),
            }
        })
    }
}
