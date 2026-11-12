//! The directory of meetings and its create, list, load, and discard.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::error::{Error, io_error};
use crate::id::SessionId;
use crate::meeting::Meeting;
use crate::record::{self, Record, SESSION_FILE};

const STAGING_PREFIX: &str = ".new-";
const TRASH_PREFIX: &str = ".trash-";

/// A directory that holds one directory per meeting.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Opens the store at `root`, creating it and its parents if needed.
    ///
    /// Best-effort sweeps `.new-*` and `.trash-*` leftovers, which a crash, or a
    /// failed cleanup of its own staging or trash directory, can leave behind. A
    /// sweep that fails is ignored on purpose: the leftover costs space, not
    /// correctness.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the root cannot be created or listed.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, Error> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(io_error("create"))?;
        let store = Self { root };
        store.sweep()?;
        Ok(store)
    }

    /// Creates a meeting with a draft record, using `created_at` for its id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the meeting cannot be written and
    /// [`Error::IdCollision`] when every id near `created_at` is taken.
    pub fn create(&self, created_at: SystemTime) -> Result<Meeting, Error> {
        let base = SessionId::of(created_at).as_millis();
        for offset in 0..record::CREATE_ATTEMPTS {
            let id = SessionId::from_millis(base.saturating_add(offset));
            let staging = self.root.join(format!("{STAGING_PREFIX}{id}"));
            let directory = self.root.join(id.to_string());
            if directory.exists() || staging.exists() {
                continue;
            }
            fs::create_dir(&staging).map_err(io_error("create"))?;
            let record = Record::default();
            let published = record::write(&staging, &record)
                .and_then(|()| fs::rename(&staging, &directory).map_err(io_error("rename")));
            if let Err(error) = published {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
            record::sync_directory(&self.root);
            return Ok(Meeting {
                id,
                directory,
                record,
            });
        }
        Err(Error::IdCollision)
    }

    /// Lists stored meetings in id order.
    ///
    /// Entries that are not meeting directories, or that have no record file,
    /// are skipped.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the root cannot be listed.
    pub fn list(&self) -> Result<Vec<SessionId>, Error> {
        let entries = fs::read_dir(&self.root).map_err(io_error("read"))?;
        let mut ids = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let Some(id) = SessionId::parse(&name) else {
                continue;
            };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || !entry.path().join(SESSION_FILE).is_file() {
                continue;
            }
            ids.push(id);
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Loads `id`, or [`None`] when the record no longer exists.
    ///
    /// The record stores no permission; [`Meeting::session`] takes the
    /// permissions the OS reports now, because an answer from a previous run
    /// can be stale.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the record cannot be read and
    /// [`Error::Decode`] when its bytes are not a record this build wrote.
    pub fn load(&self, id: SessionId) -> Result<Option<Meeting>, Error> {
        let directory = self.root.join(id.to_string());
        let Some(record) = record::read(&directory)? else {
            return Ok(None);
        };
        Ok(Some(Meeting {
            id,
            directory,
            record,
        }))
    }

    /// Removes `id` from the store.
    ///
    /// The meeting's directory is renamed into a trash directory, so
    /// [`Self::list`] stops reporting it and [`Self::load`] returns [`None`].
    /// Removing the trash copy is best effort; a copy that cannot be removed is
    /// left for [`Self::open`] to sweep, so the call still succeeds. Idempotent:
    /// a missing meeting is `Ok`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the directory cannot be renamed, or a stale
    /// trash copy left by an earlier discard cannot be removed.
    pub fn discard(&self, id: SessionId) -> Result<(), Error> {
        let trash = self.root.join(format!("{TRASH_PREFIX}{id}"));
        if trash.exists() {
            fs::remove_dir_all(&trash).map_err(io_error("remove"))?;
        }
        let directory = self.root.join(id.to_string());
        match fs::rename(&directory, &trash) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error("rename")(error)),
        }
        record::sync_directory(&self.root);
        let _ = fs::remove_dir_all(&trash);
        record::sync_directory(&self.root);
        Ok(())
    }

    fn sweep(&self) -> Result<(), Error> {
        let entries = fs::read_dir(&self.root).map_err(io_error("read"))?;
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(STAGING_PREFIX) || name.starts_with(TRASH_PREFIX) {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
        Ok(())
    }
}
