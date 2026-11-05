//! Durable meetings for koe.
//!
//! A [`Store`] keeps one directory per meeting under a root. Inside it, the
//! `session` record holds the meeting's restart-safe checkpoint, its transcript
//! for each source, and the name of the audio file; the audio itself is written
//! by `koe-record` at the path [`Meeting::set_recording`] returns.
//!
//! Every write is atomic: a record is written as `session.partial` and renamed
//! over `session`, a meeting is created in a staging directory and renamed into
//! place, and a discarded meeting is renamed into a trash directory before it is
//! removed. A crash therefore leaves either the old complete record or the new
//! one, and [`Store::open`] sweeps the staging and trash directories a crash can
//! leave behind.
//!
//! ```no_run
//! use std::error::Error;
//! use std::time::SystemTime;
//!
//! use koe_session::{Permissions, Session};
//! use koe_session_store::{Store, Transcripts};
//!
//! fn main() -> Result<(), Box<dyn Error>> {
//!     let store = Store::open("meetings")?;
//!     let permissions = Permissions::default();
//!     let mut meeting = store.create(SystemTime::now())?;
//!
//!     let audio = meeting.set_recording("meeting.ogg")?;
//!     println!("koe-record writes {}", audio.display());
//!
//!     let session = meeting
//!         .session(permissions)
//!         .unwrap_or_else(|| Session::new(permissions));
//!     meeting.save(&session, &Transcripts::new())?;
//!     Ok(())
//! }
//! ```

mod crc;
mod error;
mod id;
mod meeting;
mod record;
mod store;
mod transcripts;

pub use error::{DecodeError, Error};
pub use id::SessionId;
pub use meeting::Meeting;
pub use store::Store;
pub use transcripts::Transcripts;
