//! Durable meetings for koe.
//!
//! A [`Store`] keeps one directory per meeting under a root. Inside it, the
//! `session` record holds the meeting's restart-safe checkpoint, its transcript
//! for each source, and the name of the audio file; the audio itself is written
//! by `koe-record` at the path [`Meeting::set_recording`] returns.
//!
//! `transcript.txt` beside the record is the same transcript as readable text,
//! one `[start..end] source: text` line per segment. It is derived: the record
//! is the source of truth, and [`Meeting::save`] and
//! [`Meeting::save_transcripts`] each rewrite the file from the record they
//! write.
//!
//! A recording name is one plain file name that the store does not write
//! itself: `session`, `session.partial`, `transcript.txt`, and
//! `transcript.partial` are refused in any case, ignoring a trailing dot or
//! space that Win32 strips, by [`Meeting::set_recording`] and by the decoder
//! alike. A record written by a build without that reservation therefore does
//! not load, even though the format version did not change.
//!
//! Every write is atomic: a record is written as `session.partial` and renamed
//! over `session`, the text is written as `transcript.partial` and renamed over
//! `transcript.txt`, a meeting is created in a staging directory and renamed
//! into place, and a discarded meeting is renamed into a trash directory out of
//! the store. A crash therefore leaves either the old complete record or the new
//! one; the trash copy is removed best effort, and [`Store::open`] sweeps the
//! staging and trash directories a crash or a failed cleanup can leave behind.
//!
//! ```no_run
//! use std::error::Error;
//! use std::time::SystemTime;
//!
//! use koe_session::{Permissions, Session};
//! use koe_session_store::{Meeting, RECORDING_FILE, Store, Transcripts};
//!
//! fn main() -> Result<(), Box<dyn Error>> {
//!     let store = Store::open("meetings")?;
//!     let permissions = Permissions::default();
//!     let mut meeting = store.create(SystemTime::now())?;
//!
//!     let audio = meeting.set_recording(RECORDING_FILE)?;
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
mod text;
mod transcripts;

pub use error::{DecodeError, Error};
pub use id::SessionId;
pub use meeting::Meeting;
pub use store::Store;
pub use transcripts::Transcripts;

/// Default audio file name inside a meeting directory.
///
/// The desktop replaces the extension with the one its format setting selects,
/// so only the base name applies to every take.
pub const RECORDING_FILE: &str = "meeting.ogg";
