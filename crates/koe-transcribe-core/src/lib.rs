//! Platform-independent speech-to-text types and audio conversion.
//!
//! The crate is `no_std` with `alloc`. It has no platform code, no system
//! library, and no dependency outside `libm`. A backend implements
//! [`Engine`], and the facade crate drives it.
//!
//! ```
//! use koe_transcribe_core::{AudioSpec, Converter, ENGINE_RATE};
//!
//! let spec = AudioSpec::new(48_000, 2).expect("valid spec");
//! let mut converter = Converter::new(spec);
//! let mut out = Vec::new();
//! converter.push(&[0.0; 9_600], &mut out);
//! assert_eq!(out.len(), ENGINE_RATE as usize / 10);
//! ```

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod config;
mod convert;
mod engine;
mod error;
mod event;
mod transcript;

pub use config::{AudioSpec, MAX_CHANNELS, MAX_SAMPLE_RATE, MIN_SAMPLE_RATE};
pub use convert::{Converter, ENGINE_RATE, frames};
pub use engine::{Engine, EngineEvent, EngineSession, Model, Options};
pub use error::Error;
pub use event::{Event, LiveText};
pub use transcript::{Incomplete, Segment, Transcript};
