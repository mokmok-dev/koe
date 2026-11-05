//! `WASAPI` backend.
//!
//! Enumeration walks `IMMDeviceEnumerator` over the active capture and render
//! endpoints. Capture opens a shared-mode `IAudioClient` in event-driven mode,
//! so `read` blocks on a kernel event instead of polling.

#[cfg(any(target_os = "windows", test))]
mod enumerate;
#[cfg(any(target_os = "windows", test))]
mod pipeline;

#[cfg(target_os = "windows")]
mod ffi;

#[cfg(target_os = "windows")]
pub use ffi::{devices, open};
