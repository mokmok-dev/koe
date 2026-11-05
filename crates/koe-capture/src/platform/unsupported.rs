//! Fallback used when no platform feature is enabled.

use koe_capture_core::{DeviceInfo, Error, RawStream, StreamSpec};

// Kept non-const so `crate::devices` has one signature in every feature
// configuration.
#[allow(clippy::missing_const_for_fn)]
pub fn devices() -> Result<Vec<DeviceInfo>, Error> {
    Err(Error::Unsupported)
}

pub fn open(_device: &DeviceInfo, _spec: StreamSpec) -> Result<Box<dyn RawStream>, Error> {
    Err(Error::Unsupported)
}
