//! Durable session state for koe meetings.
//!
//! [`StateFile`] stores one [`Session`]'s restart-safe checkpoint on disk. An
//! application saves after every transition and loads at startup.
//!
//! One process writes a state file at a time. `save` writes a sibling
//! `<path>.partial` and renames it over the path last, so a crash leaves either
//! the old complete record or the new one. Concurrent writers would race on the
//! shared partial path, so a caller that needs more than one must serialize
//! them.
//!
//! ```no_run
//! use std::error::Error;
//!
//! use koe_session::{Input, Permissions, Session, SourceSet};
//! use koe_session_store::StateFile;
//!
//! fn main() -> Result<(), Box<dyn Error>> {
//!     let file = StateFile::new("meeting.state");
//!     let permissions = Permissions::default();
//!     let mut session = match file.load(permissions)? {
//!         Some(saved) => saved,
//!         None => Session::new(permissions),
//!     };
//!
//!     let step = session.step(Input::Start(SourceSet::MICROPHONE))?;
//!     session = step.session;
//!     file.save(&session)?;
//!     Ok(())
//! }
//! ```

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::PathBuf;

use koe_session::{CHECKPOINT_LEN, Checkpoint, DecodeError, Permissions, Session};

/// A file that stores one meeting's checkpoint.
#[derive(Clone, Debug)]
pub struct StateFile {
    path: PathBuf,
}

impl StateFile {
    /// Names `path` as the state file. Nothing is read or written until
    /// [`Self::save`] or [`Self::load`].
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Saves `session`, replacing any previous record.
    ///
    /// An idle session has no audio to restore, so saving it removes the file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the partial file cannot be written or
    /// renamed over the state file.
    pub fn save(&self, session: &Session) -> Result<(), Error> {
        let Some(checkpoint) = Checkpoint::of(session) else {
            return match fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(Error::Io {
                    operation: "remove",
                    kind: error.kind(),
                }),
            };
        };
        let bytes = checkpoint.to_bytes();
        let partial = self.partial_path();
        {
            let mut file = File::create(&partial).map_err(|error| Error::Io {
                operation: "create",
                kind: error.kind(),
            })?;
            file.write_all(&bytes).map_err(|error| Error::Io {
                operation: "write",
                kind: error.kind(),
            })?;
        }
        fs::rename(&partial, &self.path).map_err(|error| Error::Io {
            operation: "rename",
            kind: error.kind(),
        })
    }

    /// Loads the stored session with the permissions the operating system
    /// reports now, or [`None`] when no file exists.
    ///
    /// The permissions are never taken from the file: a checkpoint records no
    /// permission, and an answer from a previous run can be stale.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the file cannot be read and
    /// [`Error::Decode`] when its bytes are not a checkpoint.
    pub fn load(&self, permissions: Permissions) -> Result<Option<Session>, Error> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(Error::Io {
                    operation: "open",
                    kind: error.kind(),
                });
            }
        };
        let mut bytes = Vec::new();
        file.take((CHECKPOINT_LEN + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| Error::Io {
                operation: "read",
                kind: error.kind(),
            })?;
        let checkpoint = Checkpoint::from_bytes(&bytes).map_err(Error::Decode)?;
        Ok(Some(Session::restore(checkpoint, permissions)))
    }

    fn partial_path(&self) -> PathBuf {
        let mut partial = self.path.clone().into_os_string();
        partial.push(".partial");
        PathBuf::from(partial)
    }
}

/// Why a state file operation failed.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A file operation failed.
    Io {
        /// Operation that failed.
        operation: &'static str,
        /// Native error kind.
        kind: io::ErrorKind,
    },
    /// The file holds bytes that are not a session checkpoint.
    Decode(DecodeError),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, kind } => {
                write!(formatter, "session state `{operation}` failed: {kind}")
            }
            Self::Decode(error) => write!(formatter, "session state is not a checkpoint: {error}"),
        }
    }
}

impl std::error::Error for Error {}
