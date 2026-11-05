//! Platform-independent capture types and pipeline.
//!
//! The crate is `no_std` with `alloc`. It has no platform code and no system
//! library. A backend implements [`RawStream`], and the facade crate builds a
//! [`CaptureStream`] from it.
//!
//! ```
//! use koe_capture_core::{Source, StreamSpec};
//!
//! let spec = StreamSpec::new(Source::Microphone, 48_000, 2).expect("valid spec");
//! assert_eq!(spec.channels(), 2);
//! ```

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod config;
mod convert;
mod device;
mod error;
mod queue;
mod stream;

pub use config::{MAX_CHANNELS, MAX_SAMPLE_RATE, MIN_SAMPLE_RATE, StreamSpec};
pub use convert::{SampleFormat, decode};
pub use device::{DeviceId, DeviceInfo, Source, select};
pub use error::Error;
pub use queue::FrameQueue;
pub use stream::{CaptureStream, RawStream};
