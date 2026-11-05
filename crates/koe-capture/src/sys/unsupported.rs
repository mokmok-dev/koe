//! Fallback used when no platform feature matches the target.

use crate::config::StreamSpec;
use crate::device::DeviceInfo;
use crate::error::{Error, Result};
use crate::stream::RawStream;

pub const fn devices() -> Result<Vec<DeviceInfo>> {
    Err(Error::Unsupported)
}

pub fn open(_device: &DeviceInfo, _spec: StreamSpec) -> Result<Box<dyn RawStream>> {
    Err(Error::Unsupported)
}
