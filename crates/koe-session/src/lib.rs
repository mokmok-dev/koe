//! Meeting recording session state machine.
//!
//! A [`Session`] is a pure value. [`Session::step`] takes one [`Input`] and
//! returns the next session together with the [`Effect`]s the caller must run,
//! so the application and the Stateright model check the same transition
//! function.
//!
//! The crate is `no_std` with `alloc`. It has no platform code and no system
//! library.
//!
//! # The three axes
//!
//! A meeting recording moves along three axes that change independently.
//!
//! - The recording axis is [`Stage`]: `Idle`, `Preparing`, `Recording`,
//!   `Paused`, `Closing`, `Reviewing`, `Confirmed`.
//! - The permission axis is [`Permission`] per [`Source`]: `Unknown`,
//!   `Requested`, `Granted`, `Denied`. An OS settings change arrives as
//!   [`Input::PermissionGranted`] or [`Input::PermissionRevoked`] at any time,
//!   including mid-recording.
//! - The transcription axis is [`Outcome`]: [`Outcome::Complete`], or
//!   [`Outcome::Incomplete`] carrying the [`Fault`] that lost audio.
//!   [`Transcript`] tracks committed and volatile text per source, and a
//!   volatile tail never survives finalization. A committed transcript
//!   survives to the confirmation even when a later segment fails.
//!
//! # Verification
//!
//! `cargo test -p koe-session` runs the unit tests and the Stateright model in
//! `src/model.rs`, which explores every reachable session and checks the
//! invariants over the real [`Session::step`].
//!
//! ```
//! use koe_session::{
//!     Effect, Input, Permission, Permissions, SegmentOutcome, SegmentResult, Session, Source,
//!     SourceSet, Stage,
//! };
//!
//! let session = Session::new(Permissions::new(Permission::Granted, Permission::Granted));
//! let started = session.step(Input::Start(SourceSet::MICROPHONE)).expect("start");
//! assert_eq!(started.effects, vec![Effect::OpenSource(Source::Microphone)]);
//!
//! let recording = started
//!     .session
//!     .step(Input::DeviceOpened(Source::Microphone))
//!     .expect("device opened");
//! assert_eq!(recording.session.stage(), Stage::Recording);
//!
//! let closing = recording.session.step(Input::Stop).expect("stop");
//! assert_eq!(
//!     closing.effects,
//!     vec![
//!         Effect::CloseSource(Source::Microphone),
//!         Effect::FinishSource(Source::Microphone),
//!     ],
//! );
//!
//! let reviewing = closing
//!     .session
//!     .step(Input::SegmentClosed {
//!         source: Source::Microphone,
//!         result: SegmentResult {
//!             outcome: SegmentOutcome::Complete,
//!             committed: true,
//!         },
//!     })
//!     .expect("segment closed");
//! assert_eq!(reviewing.session.stage(), Stage::Reviewing);
//!
//! let confirmed = reviewing.session.step(Input::Confirm).expect("confirm");
//! assert_eq!(confirmed.effects, vec![Effect::SaveTranscript]);
//! assert_eq!(confirmed.session.stage(), Stage::Confirmed);
//! ```

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod input;
#[cfg(test)]
mod model;
mod permission;
mod sources;
mod state;
mod transcript;

pub use koe_capture_core::Source;

pub use input::{Effect, Input};
pub use permission::{Permission, Permissions};
pub use sources::SourceSet;
pub use state::{Rejected, Session, Stage, Step};
pub use transcript::{Fault, Outcome, SegmentOutcome, SegmentResult, Transcript};
