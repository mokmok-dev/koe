//! koe-ffi — uniffi-generated bindings and type conversions.

mod api;
mod callbacks;
mod error;
mod handles;
mod native;
mod types;

pub use api::{
    check_permission, enumerate_apps, feed_transcription_audio, finalize_transcription,
    pause_recording, request_permission, resume_recording, start_capture, start_recording,
    start_transcription, stop_capture, stop_recording,
};
pub use callbacks::{
    AudioCallback, AudioCallbackRef, ProgressCallback, ProgressCallbackRef, TranscriptionCallback,
    TranscriptionCallbackRef,
};
pub use error::{CaptureError, RecordingError, RecordingSummary, TranscriptionError};
pub use handles::{CaptureHandle, RecordingHandle, TranscriptionHandle};
pub use native::{NativeProvider, register_native_provider};
pub use types::{
    AppInfo, AudioSourceConfig, OutputFormat, Permission, PermissionStatus, RecordingState,
    RecordingStatus, TranscriptFormat, TranscriptionSegment,
};

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
}
