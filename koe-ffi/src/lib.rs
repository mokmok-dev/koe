//! koe-ffi — uniffi-generated bindings and type conversions.

mod api;
mod callbacks;
mod error;
mod handles;
mod native;
mod types;

#[cfg(target_os = "macos")]
mod macos_discovery;

pub use api::{
    check_permission, enumerate_apps, feed_transcription_audio, finalize_transcription,
    pause_recording, request_permission, resume_recording, start_capture, start_recording,
    start_transcription, stop_capture, stop_recording,
};
pub use callbacks::{
    AudioCallback, AudioCallbackRef, ProgressCallback, ProgressCallbackRef, TranscriptionCallback,
    TranscriptionCallbackRef,
};
pub use error::{
    CaptureError, RecordingError, RecordingSummary, TranscriptionError, validate_capture_source,
    validate_locale, validate_output_path,
};
pub use handles::{CaptureHandle, RecordingHandle, TranscriptionHandle};
pub use native::{NativeProvider, native_provider_registered, register_native_provider};
pub use types::{
    AppInfo, AudioSourceConfig, OutputFormat, Permission, PermissionStatus, RecordingState,
    RecordingStatus, TranscriptFormat, TranscriptionSegment,
};

#[cfg(target_os = "macos")]
pub use macos_discovery::install_default_native_provider;

/// No-op on non-macOS targets.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub const fn install_default_native_provider() -> bool {
    false
}

uniffi::setup_scaffolding!();

/// Smoke-test export used to verify uniffi Swift binding generation.
#[uniffi::export]
#[must_use]
pub const fn add(
    left: u64,
    right: u64,
) -> u64 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_works() {
        assert_eq!(add(2, 2), 4);
    }

    #[test]
    fn permission_defaults_without_provider() {
        assert_eq!(
            check_permission(Permission::Microphone),
            PermissionStatus::NotDetermined
        );
    }

    #[test]
    fn enumerate_apps_empty_without_provider() {
        assert!(enumerate_apps().is_empty());
    }

    #[test]
    fn native_provider_starts_unregistered() {
        // Other tests in this crate do not register a provider.
        assert!(!native_provider_registered());
    }
}
