//! Live monitoring: route clean PCM to the default output device.
//!
//! Signal path (spec): Ring Buffer → AEC → Clean Audio ─┬─→ Encoder
//!                                                      └─→ `AudioQueue` output
//!
//! The actual `AudioQueue` lives in `koe-native` (`AudioMonitor.swift`). This
//! module owns the pipeline-side contract and an FFI-backed implementation.
//! Monitoring failures are non-fatal: the recording path must keep running.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub use koe_ffi::MonitorError;
use koe_ffi::{MonitorHandle, feed_monitor, start_monitor, stop_monitor};

/// Canonical sample rate for monitored audio (matches capture / AEC).
pub const MONITOR_SAMPLE_RATE_HZ: u32 = 48_000;
/// Interleaved stereo.
pub const MONITOR_CHANNEL_COUNT: u16 = 2;
/// One host buffer = 20 ms at 48 kHz.
pub const MONITOR_BUFFER_FRAMES: usize = 960;
/// Bytes per interleaved stereo frame (Float32 × 2).
pub const MONITOR_BYTES_PER_FRAME: usize = 8;

/// Sink for clean (post-AEC) PCM destined for the default output device.
///
/// PCM must be interleaved stereo [`f32`] at [`MONITOR_SAMPLE_RATE_HZ`]
/// ([`MONITOR_CHANNEL_COUNT`] channels). Each write is typically one
/// [`MONITOR_BUFFER_FRAMES`]-frame block (~20 ms).
pub trait AudioMonitor: Send + Sync {
    /// Enqueues interleaved stereo Float32 samples for playback.
    ///
    /// Implementations must not block for longer than one buffer period.
    ///
    /// # Errors
    ///
    /// Returns [`MonitorError`] when the output device rejects the write or
    /// the monitor has already been stopped.
    fn write(
        &self,
        pcm: &[f32],
    ) -> Result<(), MonitorError>;

    /// Tears down the output queue. Safe to call more than once.
    fn stop(&self);
}

/// No-op monitor used when [`super::PipelineConfig::monitor`] is false.
#[derive(Debug, Default)]
pub struct NullMonitor;

impl AudioMonitor for NullMonitor {
    fn write(
        &self,
        _pcm: &[f32],
    ) -> Result<(), MonitorError> {
        Ok(())
    }

    fn stop(&self) {}
}

/// FFI-backed monitor that forwards PCM to the native `AudioQueue` bridge.
struct FfiMonitor {
    handle: Arc<MonitorHandle>,
    stopped: AtomicBool,
}

impl FfiMonitor {
    /// Opens a native monitoring session (canonical 48 kHz stereo Float32).
    ///
    /// # Errors
    ///
    /// Returns [`MonitorError::CreateFailed`] when the FFI layer cannot start
    /// the output queue.
    fn start() -> Result<Self, MonitorError> {
        let handle = start_monitor()?;
        Ok(Self {
            handle,
            stopped: AtomicBool::new(false),
        })
    }
}

impl AudioMonitor for FfiMonitor {
    fn write(
        &self,
        pcm: &[f32],
    ) -> Result<(), MonitorError> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(MonitorError::NotRunning);
        }
        feed_monitor(Arc::clone(&self.handle), pcm.to_vec())?;
        // Re-check after feed so a concurrent stop is visible to the caller.
        if self.stopped.load(Ordering::Acquire) {
            return Err(MonitorError::NotRunning);
        }
        Ok(())
    }

    fn stop(&self) {
        if self
            .stopped
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            stop_monitor(Arc::clone(&self.handle));
        }
    }
}

impl Drop for FfiMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Builds the monitor sink for a pipeline session.
///
/// Returns [`NullMonitor`] when monitoring is disabled so the consumer always
/// has a concrete sink (avoids branching on every chunk).
///
/// # Errors
///
/// Returns [`MonitorError`] when an enabled monitor cannot open the native
/// output queue. Callers that must keep recording running should fall back to
/// [`NullMonitor`] (see [`super::RecordingPipeline::start`]).
pub fn create_monitor(enabled: bool) -> Result<Arc<dyn AudioMonitor>, MonitorError> {
    if enabled {
        Ok(Arc::new(FfiMonitor::start()?))
    } else {
        Ok(Arc::new(NullMonitor))
    }
}

/// Like [`create_monitor`], but never fails the recording path.
///
/// On create failure logs a warning and returns [`NullMonitor`].
#[must_use]
pub fn create_monitor_or_null(enabled: bool) -> Arc<dyn AudioMonitor> {
    match create_monitor(enabled) {
        Ok(monitor) => monitor,
        Err(err) => {
            log::warn!("audio monitor unavailable; continuing without monitoring: {err}");
            Arc::new(NullMonitor)
        },
    }
}

/// Test double that records every write (used by unit tests).
#[cfg(test)]
#[derive(Debug, Default)]
pub struct RecordingMonitor {
    pub samples: std::sync::Mutex<Vec<Vec<f32>>>,
    pub write_count: std::sync::atomic::AtomicU64,
    pub stop_count: std::sync::atomic::AtomicU64,
    pub fail_writes: AtomicBool,
}

#[cfg(test)]
impl AudioMonitor for RecordingMonitor {
    fn write(
        &self,
        pcm: &[f32],
    ) -> Result<(), MonitorError> {
        if self.fail_writes.load(Ordering::Relaxed) {
            return Err(MonitorError::Internal {
                msg: "injected failure".to_owned(),
            });
        }
        self.write_count.fetch_add(1, Ordering::Relaxed);
        self.samples
            .lock()
            .map_err(|_| MonitorError::Internal {
                msg: "lock poisoned".to_owned(),
            })?
            .push(pcm.to_vec());
        Ok(())
    }

    fn stop(&self) {
        self.stop_count.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_constants_match_spec() {
        assert_eq!(MONITOR_SAMPLE_RATE_HZ, 48_000);
        assert_eq!(MONITOR_CHANNEL_COUNT, 2);
        assert_eq!(MONITOR_BUFFER_FRAMES, 960);
        assert_eq!(
            MONITOR_BUFFER_FRAMES * usize::from(MONITOR_CHANNEL_COUNT),
            1_920
        );
        // 20 ms at 48 kHz.
        assert_eq!(
            MONITOR_BUFFER_FRAMES * 1_000 / MONITOR_SAMPLE_RATE_HZ as usize,
            20
        );
        assert_eq!(MONITOR_BYTES_PER_FRAME, 8);
    }

    #[test]
    fn null_monitor_is_noop() {
        let monitor = NullMonitor;
        monitor.write(&[0.1, -0.1]).expect("write");
        monitor.stop();
        monitor.stop();
    }

    #[test]
    fn create_monitor_disabled_returns_null() {
        let monitor = create_monitor(false).expect("create");
        monitor.write(&[0.0, 0.0]).expect("write");
        monitor.stop();
    }

    #[test]
    fn create_monitor_enabled_uses_ffi_stub() {
        let monitor = create_monitor(true).expect("create");
        monitor
            .write(&[0.1, -0.1, 0.2, -0.2])
            .expect("write");
        monitor.stop();
        // Second stop is a no-op for FfiMonitor.
        monitor.stop();
    }

    #[test]
    fn create_monitor_or_null_never_fails() {
        let monitor = create_monitor_or_null(true);
        monitor.write(&[0.0, 0.0]).expect("write");
        monitor.stop();
    }

    #[test]
    fn recording_monitor_captures_pcm() {
        let monitor = RecordingMonitor::default();
        monitor.write(&[0.5, -0.5]).expect("write");
        monitor.write(&[0.25, -0.25]).expect("write");
        assert_eq!(monitor.write_count.load(Ordering::Relaxed), 2);
        let samples = monitor.samples.lock().expect("lock").clone();
        assert_eq!(samples, vec![vec![0.5, -0.5], vec![0.25, -0.25]]);
        monitor.stop();
        assert_eq!(monitor.stop_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn ffi_monitor_rejects_write_after_stop() {
        let monitor = FfiMonitor::start().expect("start");
        monitor.stop();
        let err = monitor.write(&[0.0, 0.0]).expect_err("stopped");
        assert!(matches!(err, MonitorError::NotRunning));
    }
}
