//! Platform backend selection.

#[cfg(any(
    all(feature = "linux", feature = "macos"),
    all(feature = "linux", feature = "windows"),
    all(feature = "macos", feature = "windows"),
))]
compile_error!("enable at most one of the linux, macos, and windows features");

#[cfg(all(feature = "linux", not(target_os = "linux")))]
compile_error!("the linux feature requires a Linux target");

#[cfg(all(feature = "macos", not(target_os = "macos")))]
compile_error!("the macos feature requires a macOS target");

#[cfg(all(feature = "windows", not(target_os = "windows")))]
compile_error!("the windows feature requires a Windows target");

#[cfg(any(feature = "linux", feature = "windows"))]
pub use koe_transcribe_ggml::engine;

#[cfg(not(any(feature = "linux", feature = "windows")))]
mod unsupported;

#[cfg(not(any(feature = "linux", feature = "windows")))]
pub use unsupported::engine;
