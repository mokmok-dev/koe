//! Recording format validation.

use crate::error::Error;

/// Lowest sample rate a recording accepts.
pub const MIN_SAMPLE_RATE: u32 = 8_000;

/// Highest sample rate a recording accepts.
pub const MAX_SAMPLE_RATE: u32 = 384_000;

/// Output channel layout of a single-file recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Channels {
    /// One channel.
    Mono,
    /// Two channels, left then right.
    Stereo,
}

impl Channels {
    /// Samples one frame carries.
    #[must_use]
    pub const fn count(self) -> u16 {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
        }
    }

    /// Reads a layout back from a channel count.
    #[must_use]
    pub const fn from_count(count: u16) -> Option<Self> {
        match count {
            1 => Some(Self::Mono),
            2 => Some(Self::Stereo),
            _ => None,
        }
    }
}

/// A validated recording format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordingSpec {
    sample_rate: u32,
    channels: Channels,
}

impl RecordingSpec {
    /// Validates a sample rate and channel layout.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidSpec`] when the rate is outside
    /// [`MIN_SAMPLE_RATE`]..=[`MAX_SAMPLE_RATE`].
    pub const fn new(sample_rate: u32, channels: Channels) -> Result<Self, Error> {
        if sample_rate >= MIN_SAMPLE_RATE && sample_rate <= MAX_SAMPLE_RATE {
            Ok(Self {
                sample_rate,
                channels,
            })
        } else {
            Err(Error::InvalidSpec {
                sample_rate,
                channels: channels.count(),
            })
        }
    }

    /// Sample rate in hertz.
    #[must_use]
    pub const fn sample_rate(self) -> u32 {
        self.sample_rate
    }

    /// Output channel layout.
    #[must_use]
    pub const fn channels(self) -> Channels {
        self.channels
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn channel_counts_round_trip() {
        assert_eq!(Channels::Mono.count(), 1);
        assert_eq!(Channels::Stereo.count(), 2);
        assert_eq!(Channels::from_count(1), Some(Channels::Mono));
        assert_eq!(Channels::from_count(2), Some(Channels::Stereo));
    }

    #[test]
    fn channel_counts_outside_the_layout_are_rejected() {
        for count in [0, 3, 6, u16::MAX] {
            assert_eq!(
                Channels::from_count(count),
                None,
                "channel count {count} must map to no layout"
            );
        }
    }

    #[test]
    fn accepts_the_supported_range() {
        for rate in [MIN_SAMPLE_RATE, 44_100, 48_000, MAX_SAMPLE_RATE] {
            for channels in [Channels::Mono, Channels::Stereo] {
                let spec = RecordingSpec::new(rate, channels).unwrap();
                assert_eq!(spec.sample_rate(), rate);
                assert_eq!(spec.channels(), channels);
            }
        }
    }

    #[test]
    fn rejects_rates_below_the_minimum() {
        assert_eq!(
            RecordingSpec::new(MIN_SAMPLE_RATE - 1, Channels::Stereo),
            Err(Error::InvalidSpec {
                sample_rate: MIN_SAMPLE_RATE - 1,
                channels: 2,
            })
        );
    }

    #[test]
    fn rejects_rates_above_the_maximum() {
        assert_eq!(
            RecordingSpec::new(MAX_SAMPLE_RATE + 1, Channels::Mono),
            Err(Error::InvalidSpec {
                sample_rate: MAX_SAMPLE_RATE + 1,
                channels: 1,
            })
        );
    }

    #[test]
    fn rejects_zero() {
        assert_eq!(
            RecordingSpec::new(0, Channels::Mono),
            Err(Error::InvalidSpec {
                sample_rate: 0,
                channels: 1,
            })
        );
    }
}
