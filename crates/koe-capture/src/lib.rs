//! Cross-platform microphone and system audio capture.
//!
//! The crate is a thin facade over `koe-capture-core` and one platform
//! backend. The core is `no_std` with `alloc`; the platform backend uses std.
//!
//! # Platform features
//!
//! Enable exactly one feature for the target you build:
//!
//! | Target | Feature |
//! | --- | --- |
//! | Linux + `PulseAudio` | `linux` |
//! | macOS | `macos` |
//! | Windows | `windows` |
//!
//! A build with no platform feature keeps the full API and reports
//! [`Error::Unsupported`] from every capture call.
//!
//! ```
//! use koe_capture::{Source, StreamSpec};
//!
//! let spec = StreamSpec::new(Source::Microphone, 48_000, 2).expect("valid spec");
//! assert_eq!(spec.channels(), 2);
//! ```

mod platform;

pub use koe_capture_core::{
    CaptureStream, DeviceId, DeviceInfo, Error, MAX_CHANNELS, MAX_SAMPLE_RATE, MIN_SAMPLE_RATE,
    Source, StreamSpec,
};

use koe_capture_core::select;

/// Lists the capture devices the platform reports.
///
/// # Errors
///
/// Returns [`Error::Unsupported`] when no platform feature is enabled, or the
/// backend's error when the audio server cannot be reached.
pub fn devices() -> Result<Vec<DeviceInfo>, Error> {
    platform::devices()
}

/// Returns the default device for `source`.
///
/// # Errors
///
/// Returns [`Error::NoDevice`] when the platform reports no device for the
/// source, and the same errors as [`devices`].
pub fn default_device(source: Source) -> Result<DeviceInfo, Error> {
    let devices = platform::devices()?;
    select(&devices, source, None).cloned()
}

/// Opens the default device for the source named in `spec`.
///
/// # Errors
///
/// Returns the same errors as [`default_device`] and [`open_device`].
pub fn open(spec: &StreamSpec) -> Result<CaptureStream, Error> {
    let devices = platform::devices()?;
    let device = select(&devices, spec.source(), None)?.clone();
    open_device(&device, spec)
}

/// Opens a specific capture device.
///
/// # Errors
///
/// Returns [`Error::WrongSource`] when `device` captures a different source
/// than `spec`, and the backend's error when the stream cannot start.
pub fn open_device(device: &DeviceInfo, spec: &StreamSpec) -> Result<CaptureStream, Error> {
    if device.source() != spec.source() {
        return Err(Error::WrongSource {
            expected: spec.source(),
            actual: device.source(),
        });
    }
    let raw = platform::open(device, *spec)?;
    Ok(CaptureStream::new(raw, device.clone(), *spec))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn device(source: Source) -> DeviceInfo {
        DeviceInfo::new(
            DeviceId::new("test").unwrap(),
            "test device",
            source,
            true,
            48_000,
            2,
        )
    }

    #[test]
    fn open_device_rejects_a_source_mismatch_before_touching_the_platform() {
        let spec = StreamSpec::new(Source::SystemAudio, 48_000, 2).unwrap();
        let error = open_device(&device(Source::Microphone), &spec)
            .err()
            .unwrap();
        assert_eq!(
            error,
            Error::WrongSource {
                expected: Source::SystemAudio,
                actual: Source::Microphone,
            }
        );
    }

    #[cfg(not(any(feature = "linux", feature = "macos", feature = "windows")))]
    #[test]
    fn a_build_without_a_platform_feature_reports_unsupported() {
        assert_eq!(devices(), Err(Error::Unsupported));
        assert_eq!(default_device(Source::Microphone), Err(Error::Unsupported));
        let spec = StreamSpec::new(Source::Microphone, 48_000, 2).unwrap();
        assert_eq!(open(&spec).err(), Some(Error::Unsupported));
        assert_eq!(
            open_device(&device(Source::Microphone), &spec).err(),
            Some(Error::Unsupported)
        );
    }
}
