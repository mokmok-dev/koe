//! Exported FFI entry points.

use std::sync::Arc;

use crate::callbacks::{AudioCallbackRef, ProgressCallbackRef, TranscriptionCallbackRef};
use crate::error::{CaptureError, RecordingError, RecordingSummary, TranscriptionError};
use crate::handles::{CaptureHandle, RecordingHandle, TranscriptionHandle};
use crate::native;
use crate::types::{AppInfo, AudioSourceConfig, OutputFormat, Permission, PermissionStatus};

#[must_use]
#[uniffi::export]
pub fn check_permission(permission: Permission) -> PermissionStatus {
    native::provider().map_or(PermissionStatus::NotDetermined, |provider| {
        provider.check_permission(permission)
    })
}

#[must_use]
#[uniffi::export]
pub fn request_permission(permission: Permission) -> PermissionStatus {
    native::provider().map_or(PermissionStatus::NotDetermined, |provider| {
        provider.request_permission(permission)
    })
}

#[must_use]
#[uniffi::export]
pub fn enumerate_apps() -> Vec<AppInfo> {
    native::provider().map_or_else(Vec::new, |provider| provider.enumerate_apps())
}

#[allow(clippy::missing_errors_doc)]
#[uniffi::export]
pub fn start_capture(
    source: AudioSourceConfig,
    callback: AudioCallbackRef,
) -> Result<Arc<CaptureHandle>, CaptureError> {
    Ok(Arc::new(CaptureHandle::new(source, callback)))
}

#[allow(clippy::needless_pass_by_value)]
#[uniffi::export]
pub fn stop_capture(handle: Arc<CaptureHandle>) {
    let _ = (&handle.source, &handle.callback, handle.id);
}

#[allow(clippy::missing_errors_doc)]
#[uniffi::export]
pub fn start_transcription(
    locale: String,
    callback: TranscriptionCallbackRef,
) -> Result<Arc<TranscriptionHandle>, TranscriptionError> {
    Ok(Arc::new(TranscriptionHandle::new(locale, callback)))
}

#[allow(clippy::needless_pass_by_value)]
#[uniffi::export]
pub fn feed_transcription_audio(
    handle: Arc<TranscriptionHandle>,
    pcm: Vec<f32>,
) {
    let _ = (&handle.locale, &handle.callback, handle.id, pcm);
}

#[allow(clippy::needless_pass_by_value)]
#[uniffi::export]
pub fn finalize_transcription(handle: Arc<TranscriptionHandle>) {
    let _ = (&handle.locale, &handle.callback, handle.id);
}

#[allow(clippy::too_many_arguments, clippy::missing_errors_doc)]
#[uniffi::export]
pub fn start_recording(
    source: AudioSourceConfig,
    output_path: String,
    locale: String,
    format: OutputFormat,
    enable_aec: bool,
    comfort_noise: bool,
    progress_callback: ProgressCallbackRef,
) -> Result<Arc<RecordingHandle>, RecordingError> {
    let _ = (format, enable_aec, comfort_noise);
    Ok(Arc::new(RecordingHandle::new(
        source,
        output_path,
        locale,
        progress_callback,
    )))
}

#[allow(clippy::needless_pass_by_value, clippy::missing_errors_doc)]
#[uniffi::export]
pub fn stop_recording(handle: Arc<RecordingHandle>) -> Result<RecordingSummary, RecordingError> {
    let _ = (
        &handle.source,
        &handle.output_path,
        &handle.locale,
        &handle.progress_callback,
        handle.id,
    );
    Ok(RecordingSummary {
        duration_sec: 0.0,
        bytes_written: 0,
        transcript_segment_count: 0,
        dropped_audio_frames: 0,
        format: OutputFormat::Ogg { quality: 0.5 },
    })
}

#[allow(clippy::needless_pass_by_value)]
#[uniffi::export]
pub fn pause_recording(handle: Arc<RecordingHandle>) {
    let _ = (
        &handle.source,
        &handle.output_path,
        &handle.locale,
        &handle.progress_callback,
        handle.id,
    );
}

#[allow(clippy::needless_pass_by_value)]
#[uniffi::export]
pub fn resume_recording(handle: Arc<RecordingHandle>) {
    let _ = (
        &handle.source,
        &handle.output_path,
        &handle.locale,
        &handle.progress_callback,
        handle.id,
    );
}
