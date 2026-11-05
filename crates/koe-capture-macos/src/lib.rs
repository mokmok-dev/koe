//! `CoreAudio` backend.
//!
//! Enumeration reads `AudioObject` properties off the system object. A
//! microphone is captured by a HAL output `AudioUnit` run in input mode; system
//! audio taps the output mix with a `CATapDescription` and runs the same unit
//! against a private aggregate device.

#[cfg(any(target_os = "macos", test))]
mod enumerate;
#[cfg(any(target_os = "macos", test))]
mod pipeline;

#[cfg(target_os = "macos")]
mod ffi;

#[cfg(target_os = "macos")]
pub use ffi::{devices, open};
