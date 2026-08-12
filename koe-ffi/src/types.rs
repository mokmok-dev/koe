//! Shared FFI value types exported to Swift.

#[derive(Debug, Clone, uniffi::Enum)]
pub enum AudioSourceConfig {
    /// Capture system audio from a specific app via `ScreenCaptureKit`.
    AppAudio { bundle_id: String },
    /// Capture system audio from a specific process via Process Tap.
    PidAudio { pid: i32 },
    /// Capture microphone input.
    Microphone,
    /// Capture both system audio and microphone (AEC active).
    Both { bundle_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Permission {
    Microphone,
    ScreenRecording,
    Accessibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PermissionStatus {
    Authorized,
    Denied,
    Restricted,
    NotDetermined,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct TranscriptionSegment {
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub is_final: bool,
    pub confidence: f32,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct AppInfo {
    pub pid: i32,
    pub name: String,
    pub bundle_id: Option<String>,
    pub has_audio: bool,
}

/// Default Core Audio device identity (name + persistent UID).
///
/// Not a `UniFFI` record: only consumed by the Rust CLI (`koe info`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDeviceInfo {
    pub name: String,
    pub uid: String,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum OutputFormat {
    Ogg { quality: f32 },
    Wav { bits_per_sample: u16 },
    Flac { compression_level: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TranscriptFormat {
    Txt,
    Srt,
    Vtt,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RecordingState {
    Recording,
    Paused,
    Stopping,
    Stopped,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct RecordingStatus {
    pub elapsed_ms: u64,
    pub bytes_written: u64,
    pub level_left: f32,
    pub level_right: f32,
    pub state: RecordingState,
}
