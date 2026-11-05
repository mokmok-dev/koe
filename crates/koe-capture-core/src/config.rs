//! Stream format validation.

use crate::Source;
use crate::error::Error;

/// Lowest sample rate the crate accepts.
pub const MIN_SAMPLE_RATE: u32 = 8_000;

/// Highest sample rate the crate accepts.
pub const MAX_SAMPLE_RATE: u32 = 384_000;

/// Highest channel count the crate accepts.
pub const MAX_CHANNELS: u16 = 32;

/// A validated capture request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamSpec {
    source: Source,
    sample_rate: u32,
    channels: u16,
}

impl StreamSpec {
    /// Validates a capture request.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidSpec`] when the rate is outside
    /// [`MIN_SAMPLE_RATE`]..=[`MAX_SAMPLE_RATE`] or the channel count is zero
    /// or above [`MAX_CHANNELS`].
    pub fn new(source: Source, sample_rate: u32, channels: u16) -> Result<Self, Error> {
        if (MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate)
            && (1..=MAX_CHANNELS).contains(&channels)
        {
            Ok(Self {
                source,
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

    /// What the stream records.
    #[must_use]
    pub const fn source(self) -> Source {
        self.source
    }

    /// Requested sample rate in hertz.
    #[must_use]
    pub const fn sample_rate(self) -> u32 {
        self.sample_rate
    }

    /// Requested channel count.
    #[must_use]
    pub const fn channels(self) -> u16 {
        self.channels
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

    #[test]
    fn accepts_the_supported_range() {
        for rate in [MIN_SAMPLE_RATE, 44_100, 48_000, MAX_SAMPLE_RATE] {
            for channels in [1, 2, MAX_CHANNELS] {
                let spec = StreamSpec::new(Source::Microphone, rate, channels).unwrap();
                assert_eq!(spec.source(), Source::Microphone);
                assert_eq!(spec.sample_rate(), rate);
                assert_eq!(spec.channels(), channels);
            }
        }
    }

    #[test]
    fn rejects_rates_below_the_minimum() {
        assert_eq!(
            StreamSpec::new(Source::SystemAudio, MIN_SAMPLE_RATE - 1, 2),
            Err(Error::InvalidSpec {
                sample_rate: MIN_SAMPLE_RATE - 1,
                channels: 2,
            })
        );
    }

    #[test]
    fn rejects_rates_above_the_maximum() {
        assert_eq!(
            StreamSpec::new(Source::SystemAudio, MAX_SAMPLE_RATE + 1, 2),
            Err(Error::InvalidSpec {
                sample_rate: MAX_SAMPLE_RATE + 1,
                channels: 2,
            })
        );
    }

    #[test]
    fn rejects_zero_channels() {
        assert_eq!(
            StreamSpec::new(Source::Microphone, 48_000, 0),
            Err(Error::InvalidSpec {
                sample_rate: 48_000,
                channels: 0,
            })
        );
    }

    #[test]
    fn rejects_channels_above_the_maximum() {
        assert_eq!(
            StreamSpec::new(Source::Microphone, 48_000, MAX_CHANNELS + 1),
            Err(Error::InvalidSpec {
                sample_rate: 48_000,
                channels: MAX_CHANNELS + 1,
            })
        );
    }
}
