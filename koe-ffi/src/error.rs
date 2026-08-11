//! Error types that cross the FFI boundary.

use crate::types::OutputFormat;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CaptureError {
    #[error("Permission denied: {0}")]
    PermissionDenied(String),
    #[error("No audio source found for {bundle_id}")]
    NoAudioSource { bundle_id: String },
    #[error("Capture stream error: {msg}")]
    StreamError { msg: String },
    #[error("Internal error: {msg}")]
    Internal { msg: String },
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum TranscriptionError {
    #[error("Unsupported locale: {locale}")]
    UnsupportedLocale { locale: String },
    #[error("Analyzer not available on this OS version")]
    NotAvailable,
    #[error("Transcription internal error: {msg}")]
    Internal { msg: String },
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum RecordingError {
    #[error("{0}")]
    Capture(#[from] CaptureError),
    #[error("{0}")]
    Transcription(#[from] TranscriptionError),
    #[error("Insufficient disk space: need {needed}, have {available}")]
    InsufficientDiskSpace { needed: u64, available: u64 },
    #[error("Output already exists: {path}")]
    OutputExists { path: String },
    #[error("Config validation error: {msg}")]
    ConfigError { msg: String },
    #[error("Internal error: {msg}")]
    Internal { msg: String },
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct RecordingSummary {
    pub duration_sec: f64,
    pub bytes_written: u64,
    pub transcript_segment_count: u64,
    pub dropped_audio_frames: u64,
    pub format: OutputFormat,
}
