//! macOS audio capture sessions for [`crate::start_capture`].
//!
//! Implements Process Tap (`PidAudio`), ScreenCaptureKit (`AppAudio`), and
//! AudioQueue microphone capture without linking the Swift `koe-native` dylib.

#![allow(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_ptr_alignment,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::map_unwrap_or,
    clippy::needless_pass_by_value,
    clippy::redundant_pub_crate,
    clippy::struct_field_names,
    clippy::unwrap_used
)]

mod microphone;
mod process_tap;
mod screen_audio;
mod timestamp;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::CaptureError;
use crate::handles::CaptureHandle;
use crate::types::AudioSourceConfig;

pub use timestamp::monotonic_ms;

/// When true, [`start_session`] returns a no-op session (unit tests / CI).
static STUB_CAPTURE: AtomicBool = AtomicBool::new(false);

/// Enables or disables no-op capture for tests that only need lifecycle.
///
/// Not part of the supported public API — test / CI harness only.
#[doc(hidden)]
pub fn set_capture_stub(enabled: bool) {
    STUB_CAPTURE.store(enabled, Ordering::SeqCst);
}

fn capture_stubbed() -> bool {
    STUB_CAPTURE.load(Ordering::SeqCst) || std::env::var_os("KOE_STUB_CAPTURE").is_some()
}

/// Running native capture; stopped on [`Drop`] or [`CaptureSession::stop`].
pub trait CaptureSession: Send {
    fn stop(&mut self);
}

struct NullSession;

impl CaptureSession for NullSession {
    fn stop(&mut self) {}
}

/// Starts a single-source capture session that forwards PCM into `handle`.
///
/// [`AudioSourceConfig::Both`] is rejected here — the pipeline opens system +
/// mic sessions separately and runs AEC / mix itself.
pub fn start_session(
    source: &AudioSourceConfig,
    handle: Arc<CaptureHandle>,
) -> Result<Box<dyn CaptureSession>, CaptureError> {
    if capture_stubbed() {
        let _ = handle;
        return Ok(Box::new(NullSession));
    }
    match source {
        AudioSourceConfig::Microphone => microphone::start(handle),
        AudioSourceConfig::PidAudio { pid } => process_tap::start(*pid, handle),
        AudioSourceConfig::AppAudio { bundle_id } => screen_audio::start(bundle_id, handle),
        AudioSourceConfig::Both { .. } => Err(CaptureError::Internal {
            msg: "Both must be split by RecordingPipeline into AppAudio + Microphone".to_owned(),
        }),
    }
}
