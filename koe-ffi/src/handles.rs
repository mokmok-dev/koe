//! Opaque session handles exported across the FFI boundary.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::callbacks::{AudioCallbackRef, ProgressCallbackRef, TranscriptionCallbackRef};
use crate::types::AudioSourceConfig;

static NEXT_HANDLE_ID: AtomicU64 = AtomicU64::new(1);

fn next_handle_id() -> u64 {
    NEXT_HANDLE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Active audio capture session.
#[derive(uniffi::Object)]
pub struct CaptureHandle {
    pub(crate) id: u64,
    pub(crate) source: AudioSourceConfig,
    pub(crate) callback: AudioCallbackRef,
}

impl CaptureHandle {
    pub(crate) fn new(
        source: AudioSourceConfig,
        callback: AudioCallbackRef,
    ) -> Self {
        Self {
            id: next_handle_id(),
            source,
            callback,
        }
    }
}

/// Active speech transcription session.
#[derive(uniffi::Object)]
pub struct TranscriptionHandle {
    pub(crate) id: u64,
    pub(crate) locale: String,
    pub(crate) callback: TranscriptionCallbackRef,
}

impl TranscriptionHandle {
    pub(crate) fn new(
        locale: String,
        callback: TranscriptionCallbackRef,
    ) -> Self {
        Self {
            id: next_handle_id(),
            locale,
            callback,
        }
    }
}

/// Active recording session spanning capture, encoding, and transcription.
#[derive(uniffi::Object)]
pub struct RecordingHandle {
    pub(crate) id: u64,
    pub(crate) source: AudioSourceConfig,
    pub(crate) output_path: String,
    pub(crate) locale: String,
    pub(crate) progress_callback: ProgressCallbackRef,
}

impl RecordingHandle {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        source: AudioSourceConfig,
        output_path: String,
        locale: String,
        progress_callback: ProgressCallbackRef,
    ) -> Self {
        Self {
            id: next_handle_id(),
            source,
            output_path,
            locale,
            progress_callback,
        }
    }
}
