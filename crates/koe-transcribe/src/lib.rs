//! Cross-platform speech-to-text.
//!
//! The crate is a thin facade over `koe-transcribe-core` and one platform
//! backend. The core is `no_std` with `alloc`; the backend uses std.
//!
//! # Platform features
//!
//! Enable exactly one feature for the target you build:
//!
//! | Target | Feature |
//! | --- | --- |
//! | Linux and Windows | `linux`, `windows` |
//! | macOS | `macos` |
//!
//! A build with no platform feature keeps the full API and reports
//! [`Error::Unsupported`] from [`Transcriber::new`].
//!
//! ```
//! use koe_transcribe::{AudioSpec, LiveText, Options};
//!
//! let spec = AudioSpec::new(48_000, 2).expect("valid spec");
//! assert_eq!(spec.channels(), 2);
//! assert_eq!(Options::new().language("ja").language_tag(), Some("ja"));
//! assert_eq!(LiveText::new().to_string(), "");
//! ```

mod actor;
mod live;
mod platform;
mod transcriber;

pub use koe_transcribe_core::{
    AudioSpec, ENGINE_RATE, Engine, EngineEvent, EngineSession, Error, Event, Incomplete, LiveText,
    Model, Options, Segment, Transcript,
};
pub use live::LiveSession;
pub use transcriber::Transcriber;
