//! Audio format validation.

use crate::error::Error;

/// Lowest sample rate the crate accepts.
pub const MIN_SAMPLE_RATE: u32 = 8_000;

/// Highest sample rate the crate accepts.
pub const MAX_SAMPLE_RATE: u32 = 384_000;

/// Highest channel count the crate accepts.
pub const MAX_CHANNELS: u16 = 32;

/// A validated audio format for a buffer handed to transcription.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioSpec {
    sample_rate: u32,
    channels: u16,
}

impl AudioSpec {
    /// Validates a sample rate and channel count.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidSpec`] when the rate is outside
    /// [`MIN_SAMPLE_RATE`]..=[`MAX_SAMPLE_RATE`] or the channel count is zero
    /// or above [`MAX_CHANNELS`].
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self, Error> {
        if (MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate)
            && (1..=MAX_CHANNELS).contains(&channels)
        {
            Ok(Self {
                sample_rate,
                channels,
            })
        } else {
            Err(Error::InvalidSpec {
                sample_rate,
                channels,
            })
        }
    }

    /// Sample rate in hertz.
    #[must_use]
    pub const fn sample_rate(self) -> u32 {
        self.sample_rate
    }

    /// Channel count.
    #[must_use]
    pub const fn channels(self) -> u16 {
        self.channels
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn accepts_the_supported_range() {
        for rate in [MIN_SAMPLE_RATE, 44_100, 48_000, MAX_SAMPLE_RATE] {
            for channels in [1, 2, MAX_CHANNELS] {
                let spec = AudioSpec::new(rate, channels).unwrap();
                assert_eq!(spec.sample_rate(), rate);
                assert_eq!(spec.channels(), channels);
            }
        }
    }

    #[test]
    fn rejects_rates_below_the_minimum() {
        assert_eq!(
            AudioSpec::new(MIN_SAMPLE_RATE - 1, 2),
            Err(Error::InvalidSpec {
                sample_rate: MIN_SAMPLE_RATE - 1,
                channels: 2,
            })
        );
    }

    #[test]
    fn rejects_rates_above_the_maximum() {
        assert_eq!(
            AudioSpec::new(MAX_SAMPLE_RATE + 1, 2),
            Err(Error::InvalidSpec {
                sample_rate: MAX_SAMPLE_RATE + 1,
                channels: 2,
            })
        );
    }

    #[test]
    fn rejects_zero_channels() {
        assert_eq!(
            AudioSpec::new(48_000, 0),
            Err(Error::InvalidSpec {
                sample_rate: 48_000,
                channels: 0,
            })
        );
    }

    #[test]
    fn rejects_channels_above_the_maximum() {
        assert_eq!(
            AudioSpec::new(48_000, MAX_CHANNELS + 1),
            Err(Error::InvalidSpec {
                sample_rate: 48_000,
                channels: MAX_CHANNELS + 1,
            })
        );
    }
}
