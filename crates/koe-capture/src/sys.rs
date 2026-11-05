//! Platform backend selection.

#[cfg(not(any(
    all(feature = "linux", target_os = "linux"),
    all(feature = "macos", target_os = "macos"),
    all(feature = "windows", target_os = "windows")
)))]
pub mod unsupported;

#[cfg(all(feature = "linux", target_os = "linux"))]
#[allow(unsafe_code)]
pub mod linux;

#[cfg(all(feature = "macos", target_os = "macos"))]
#[allow(unsafe_code)]
pub mod macos;

#[cfg(all(feature = "windows", target_os = "windows"))]
#[allow(unsafe_code)]
pub mod windows;

#[cfg(all(feature = "linux", target_os = "linux"))]
pub use linux::{devices, open};

#[cfg(all(feature = "macos", target_os = "macos"))]
pub use macos::{devices, open};

#[cfg(all(feature = "windows", target_os = "windows"))]
pub use windows::{devices, open};

#[cfg(not(any(
    all(feature = "linux", target_os = "linux"),
    all(feature = "macos", target_os = "macos"),
    all(feature = "windows", target_os = "windows")
)))]
pub use unsupported::{devices, open};
