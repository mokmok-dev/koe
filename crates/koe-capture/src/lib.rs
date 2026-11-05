//! Cross-platform microphone and system audio capture.
//!
//! The crate talks to the operating system's audio API directly. There is no
//! runtime dependency and no helper process.
//!
//! # Platform features
//!
//! Enable exactly the feature for the target you build:
//!
//! | Target | Feature |
//! | --- | --- |
//! | Linux + `PulseAudio` | `linux` |
//! | macOS | `macos` |
//! | Windows | `windows` |
//!
//! A build with no platform feature keeps the full API and the property-tested
//! core, and reports [`Error::Unsupported`] from every capture call. This is
//! the configuration used for mutation testing.
//!
//! ```
//! use koe_capture::{Source, StreamSpec};
//!
//! let spec = StreamSpec::new(Source::Microphone, 48_000, 2).expect("valid spec");
//! assert_eq!(spec.channels(), 2);
//! ```

mod config;
mod convert;
mod device;
mod error;
mod queue;
mod stream;
mod sys;

pub use config::{MAX_CHANNELS, MAX_SAMPLE_RATE, MIN_SAMPLE_RATE, StreamSpec};
pub use device::{DeviceId, DeviceInfo, Source};
pub use error::{Error, Result};
pub use stream::CaptureStream;

/// Lists the capture devices the platform reports.
///
/// # Errors
///
/// Returns [`Error::Unsupported`] when no platform feature matches the target,
/// or the backend's error when the audio server cannot be reached.
#[allow(clippy::missing_const_for_fn)]
pub fn devices() -> Result<Vec<DeviceInfo>> {
    sys::devices()
}

/// Returns the default device for `source`.
///
/// # Errors
///
/// Returns [`Error::NoDevice`] when the platform reports no device for the
/// source, and the same errors as [`devices`].
pub fn default_device(source: Source) -> Result<DeviceInfo> {
    let devices = sys::devices()?;
    device::select(&devices, source, None).cloned()
}

/// Opens the default device for the source named in `spec`.
///
/// # Errors
///
/// Returns the same errors as [`default_device`] and [`open_device`].
pub fn open(spec: &StreamSpec) -> Result<CaptureStream> {
    let devices = sys::devices()?;
    let device = device::select(&devices, spec.source(), None)?.clone();
    open_device(&device, spec)
}

/// Opens a specific capture device.
///
/// # Errors
///
/// Returns [`Error::WrongSource`] when `device` captures a different source
/// than `spec`, and the backend's error when the stream cannot start.
pub fn open_device(device: &DeviceInfo, spec: &StreamSpec) -> Result<CaptureStream> {
    if device.source() != spec.source() {
        return Err(Error::WrongSource {
            expected: spec.source(),
            actual: device.source(),
        });
    }
    let raw = sys::open(device, *spec)?;
    Ok(CaptureStream::new(raw, device.clone(), *spec))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::float_cmp,
        clippy::cast_precision_loss,
        clippy::needless_range_loop
    )]

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

    #[cfg(not(any(
        all(feature = "linux", target_os = "linux"),
        all(feature = "macos", target_os = "macos"),
        all(feature = "windows", target_os = "windows")
    )))]
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
