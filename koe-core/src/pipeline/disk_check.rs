//! Pre-recording disk space validation.

use std::path::Path;

use koe_ffi::{OutputFormat, RecordingError};

/// Minimum free space required when no duration estimate is available.
const MIN_FREE_BYTES: u64 = 100 * 1024 * 1024;

/// Estimated bytes per hour by output format (from task 43).
const fn estimated_bytes_per_hour(format: &OutputFormat) -> u64 {
    match format {
        OutputFormat::Ogg { .. } => 42 * 1024 * 1024,
        OutputFormat::Flac { .. } => 210 * 1024 * 1024,
        OutputFormat::Wav { .. } => 2_100 * 1024 * 1024,
    }
}

/// Returns available bytes on the volume containing `path`.
fn available_space(path: &Path) -> Result<u64, RecordingError> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let check_path = parent.unwrap_or_else(|| Path::new("."));
    fs2::available_space(check_path).map_err(|err| RecordingError::Internal {
        msg: format!("disk space check failed: {err}"),
    })
}

/// Validates that the output volume has enough free space for recording.
///
/// # Errors
///
/// Returns [`RecordingError::InsufficientDiskSpace`] when free space is too low.
pub fn check_disk_space(
    output_path: &Path,
    format: &OutputFormat,
    estimated_duration_hours: Option<f64>,
) -> Result<(), RecordingError> {
    let available = available_space(output_path)?;
    let needed = estimated_duration_hours.map_or(MIN_FREE_BYTES, |hours| {
        #[allow(clippy::cast_precision_loss)]
        let estimate = hours * estimated_bytes_per_hour(format) as f64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let bytes = estimate as u64;
        bytes
    });

    if available < needed {
        return Err(RecordingError::InsufficientDiskSpace { needed, available });
    }

    if available < needed.saturating_mul(2) {
        log::warn!(
            "Low disk space: {available} bytes available, {needed} bytes estimated needed"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_impossibly_large_requirement() {
        let tmp = std::env::temp_dir().join("koe-disk-check-test.ogg");
        let err = check_disk_space(
            &tmp,
            &OutputFormat::Wav { bits_per_sample: 32 },
            Some(1_000_000.0),
        )
        .expect_err("should fail on insufficient space");
        assert!(matches!(
            err,
            RecordingError::InsufficientDiskSpace { .. }
        ));
    }
}
