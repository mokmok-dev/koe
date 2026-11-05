//! `PulseAudio` backend.
//!
//! Enumeration uses `libpulse`'s asynchronous context API over a private
//! main loop. Capture uses `libpulse-simple`, which blocks in `read` and keeps
//! the rest of the crate free of callback plumbing.
//!
//! Only the FFI module touches the C library. The record mapping and the wait
//! loops sit beside it, so they build and test on a host without `libpulse`.

#[cfg(any(target_os = "linux", test))]
mod enumerate;
#[cfg(any(target_os = "linux", test))]
mod pipeline;

#[cfg(target_os = "linux")]
mod ffi;

#[cfg(target_os = "linux")]
pub use ffi::{devices, open};
