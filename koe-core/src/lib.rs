//! koe-core — shared pipeline, AEC, codecs, and state.

pub mod aec;
pub mod codec;
pub mod pipeline;
pub mod transcript;

pub use aec::{AcousticEchoCanceller, AecConfig};
pub use pipeline::{
    AudioChunk, AudioMonitor, FORCE_JOIN_BUDGET, FileWriter, MONITOR_BUFFER_FRAMES,
    MONITOR_CHANNEL_COUNT, MONITOR_SAMPLE_RATE_HZ, MonitorError, PipelineConfig, PipelineError,
    PipelineMetrics, PipelineMetricsSnapshot, PipelineState, RecordingPipeline, RecordingState,
    RecordingStatus, SHUTDOWN_BUDGET, ShutdownMode, StopResult, available_disk_space,
    create_monitor, create_monitor_or_null,
};

/// Error returned by [`available_disk_space`] and recording setup.
pub use koe_ffi::RecordingError;
/// Summary payload returned from [`RecordingPipeline::stop`].
pub use koe_ffi::RecordingSummary;

/// Discovery and permission entry points used by `koe-cli` (and GUI).
pub use koe_ffi::{
    AppInfo, Permission, PermissionStatus, check_permission, enumerate_apps,
    install_default_native_provider, native_provider_registered,
};

/// Compile-time feature flags enabled in this `koe-core` build.
#[must_use]
pub fn enabled_features() -> Vec<&'static str> {
    let mut features = Vec::new();
    if cfg!(feature = "aec") {
        features.push("aec");
    }
    if cfg!(feature = "cli") {
        features.push("cli");
    }
    if cfg!(feature = "gui") {
        features.push("gui");
    }
    if cfg!(feature = "ogg") {
        features.push("ogg");
    }
    if cfg!(feature = "screen-audio") {
        features.push("screen-audio");
    }
    if cfg!(feature = "system-audio") {
        features.push("system-audio");
    }
    features
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_features_include_cli_stack() {
        let features = enabled_features();
        assert!(features.contains(&"aec"));
        assert!(features.contains(&"cli"));
        assert!(features.contains(&"ogg"));
        assert!(features.contains(&"screen-audio"));
        assert!(features.contains(&"system-audio"));
        assert!(!features.contains(&"gui"));
    }
}
