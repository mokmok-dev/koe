//! Capture sources, device identifiers, and device selection.

use core::fmt;
use core::str::FromStr;

use crate::error::{Error, Result};

/// What a capture stream records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// A microphone or other capture endpoint.
    Microphone,
    /// The audio the machine is playing.
    SystemAudio,
}

impl Source {
    /// Lowercase name used in diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Microphone => "microphone",
            Self::SystemAudio => "system audio",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable identifier for a capture device.
///
/// The value is opaque to callers and comes from the platform backend. It is
/// non-empty, which keeps an empty id from ever reaching an OS lookup.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId(Box<str>);

impl DeviceId {
    /// Creates an id, rejecting the empty string.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidId`] when `value` is empty.
    pub fn new(value: impl Into<Box<str>>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() {
            Err(Error::InvalidId)
        } else {
            Ok(Self(value))
        }
    }

    /// Borrows the id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for DeviceId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

/// A capture device reported by the platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    id: DeviceId,
    name: Box<str>,
    source: Source,
    is_default: bool,
    sample_rate: u32,
    channels: u16,
}

impl DeviceInfo {
    // Constructed by the cfg-gated platform adapters and by tests.
    #[allow(dead_code)]
    pub fn new(
        id: DeviceId,
        name: impl Into<Box<str>>,
        source: Source,
        is_default: bool,
        sample_rate: u32,
        channels: u16,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            source,
            is_default,
            sample_rate,
            channels,
        }
    }

    /// Stable identifier accepted by [`crate::open_device`].
    #[must_use]
    pub const fn id(&self) -> &DeviceId {
        &self.id
    }

    /// Human-readable device name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What this device records.
    #[must_use]
    pub const fn source(&self) -> Source {
        self.source
    }

    /// Whether the platform treats this device as the default for its source.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        self.is_default
    }

    /// Sample rate the device reports as its native rate.
    #[must_use]
    pub const fn default_sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Channel count the device reports as its native count.
    #[must_use]
    pub const fn channels(&self) -> u16 {
        self.channels
    }
}

/// Picks the device a capture request should open.
///
/// An explicit id is matched by identity, then checked against the requested
/// source. Without an id, the default device for the source wins over the first
/// reported device.
pub fn select<'a>(
    devices: &'a [DeviceInfo],
    source: Source,
    requested: Option<&DeviceId>,
) -> Result<&'a DeviceInfo> {
    let Some(requested) = requested else {
        return devices
            .iter()
            .find(|device| device.source == source && device.is_default)
            .or_else(|| devices.iter().find(|device| device.source == source))
            .ok_or(Error::NoDevice(source));
    };

    let device = devices
        .iter()
        .find(|device| device.id == *requested)
        .ok_or_else(|| Error::DeviceNotFound(requested.clone()))?;
    if device.source == source {
        Ok(device)
    } else {
        Err(Error::WrongSource {
            expected: source,
            actual: device.source,
        })
    }
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

    fn device(id: &str, source: Source, is_default: bool) -> DeviceInfo {
        DeviceInfo::new(
            DeviceId::new(id).unwrap(),
            format!("device {id}"),
            source,
            is_default,
            48_000,
            2,
        )
    }

    #[test]
    fn source_names_are_distinct() {
        assert_eq!(Source::Microphone.as_str(), "microphone");
        assert_eq!(Source::SystemAudio.as_str(), "system audio");
        assert_eq!(Source::Microphone.to_string(), "microphone");
        assert_eq!(Source::SystemAudio.to_string(), "system audio");
    }

    #[test]
    fn empty_device_id_is_rejected() {
        assert_eq!(DeviceId::new(""), Err(Error::InvalidId));
        assert_eq!("".parse::<DeviceId>(), Err(Error::InvalidId));
    }

    #[test]
    fn device_id_round_trips() {
        let id = DeviceId::new("hw:1,0").unwrap();
        assert_eq!(id.as_str(), "hw:1,0");
        assert_eq!(id.to_string(), "hw:1,0");
        assert_eq!("hw:1,0".parse::<DeviceId>(), Ok(id));
    }

    #[test]
    fn device_accessors_expose_construction() {
        let info = device("mic", Source::Microphone, true);
        assert_eq!(info.id().as_str(), "mic");
        assert_eq!(info.name(), "device mic");
        assert_eq!(info.source(), Source::Microphone);
        assert!(info.is_default());
        assert_eq!(info.default_sample_rate(), 48_000);
        assert_eq!(info.channels(), 2);

        let fallback = device("fallback", Source::SystemAudio, false);
        assert!(!fallback.is_default());
    }

    #[test]
    fn explicit_id_wins_even_when_not_default() {
        let devices = [
            device("default", Source::Microphone, true),
            device("chosen", Source::Microphone, false),
        ];
        let chosen = DeviceId::new("chosen").unwrap();
        let selected = select(&devices, Source::Microphone, Some(&chosen)).unwrap();
        assert_eq!(selected.id().as_str(), "chosen");
    }

    #[test]
    fn explicit_id_must_match_the_requested_source() {
        let devices = [device("speakers", Source::SystemAudio, false)];
        let chosen = DeviceId::new("speakers").unwrap();
        let error = select(&devices, Source::Microphone, Some(&chosen)).unwrap_err();
        assert_eq!(
            error,
            Error::WrongSource {
                expected: Source::Microphone,
                actual: Source::SystemAudio,
            }
        );
    }

    #[test]
    fn missing_explicit_id_is_reported() {
        let devices = [device("present", Source::Microphone, true)];
        let chosen = DeviceId::new("absent").unwrap();
        let error = select(&devices, Source::Microphone, Some(&chosen)).unwrap_err();
        assert_eq!(error, Error::DeviceNotFound(chosen));
    }

    #[test]
    fn default_device_is_preferred_within_the_source() {
        let devices = [
            device("first", Source::Microphone, false),
            device("system", Source::SystemAudio, true),
            device("second", Source::Microphone, true),
        ];
        let selected = select(&devices, Source::Microphone, None).unwrap();
        assert_eq!(selected.id().as_str(), "second");
    }

    #[test]
    fn first_device_is_the_fallback_when_no_default_is_marked() {
        let devices = [
            device("first", Source::Microphone, false),
            device("second", Source::Microphone, false),
        ];
        let selected = select(&devices, Source::Microphone, None).unwrap();
        assert_eq!(selected.id().as_str(), "first");
    }

    #[test]
    fn source_without_devices_is_reported() {
        let devices = [device("system", Source::SystemAudio, true)];
        let error = select(&devices, Source::Microphone, None).unwrap_err();
        assert_eq!(error, Error::NoDevice(Source::Microphone));
    }

    #[test]
    fn empty_list_has_no_default() {
        let error = select(&[], Source::SystemAudio, None).unwrap_err();
        assert_eq!(error, Error::NoDevice(Source::SystemAudio));
    }
}
