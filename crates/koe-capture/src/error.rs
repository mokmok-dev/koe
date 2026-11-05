//! Errors crossing the capture boundary.

use std::fmt;

use crate::Source;

/// Something that prevented a capture operation from completing.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// No capture backend is enabled for the current target.
    Unsupported,
    /// The platform reported no device for the requested source.
    NoDevice(Source),
    /// An explicit device id did not match any enumerated device.
    DeviceNotFound(crate::DeviceId),
    /// The device exists but captures a different source.
    WrongSource {
        /// Source the caller asked for.
        expected: Source,
        /// Source the device provides.
        actual: Source,
    },
    /// The requested stream format is outside the supported range.
    InvalidSpec {
        /// Requested sample rate in hertz.
        sample_rate: u32,
        /// Requested channel count.
        channels: u16,
    },
    /// A device id was empty.
    InvalidId,
    /// The capture stream reached its end.
    StreamClosed,
    /// An operating-system audio call failed.
    Backend {
        /// Operation that failed.
        operation: &'static str,
        /// Native error code.
        code: i32,
    },
    /// The audio server did not answer in time.
    Timeout {
        /// Operation that timed out.
        operation: &'static str,
    },
    /// The caller's output buffer cannot hold a single frame.
    BufferTooSmall {
        /// Frames the buffer can hold.
        frames: usize,
    },
    /// The caller's output buffer does not contain whole frames.
    MisalignedBuffer {
        /// Buffer length in samples.
        len: usize,
        /// Channel count the buffer must be a multiple of.
        channels: usize,
    },
    /// A decode target cannot hold every decoded sample.
    OutputTooSmall {
        /// Samples the input contains.
        samples: usize,
        /// Samples the output can hold.
        capacity: usize,
    },
    /// A backend violated the private capture contract.
    BackendInvariant {
        /// Operation that broke the contract.
        operation: &'static str,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => formatter.write_str("no audio capture backend is enabled"),
            Self::NoDevice(source) => write!(formatter, "no {source} device is available"),
            Self::DeviceNotFound(id) => write!(formatter, "device `{id}` was not found"),
            Self::WrongSource { expected, actual } => {
                write!(formatter, "device provides {actual}, expected {expected}")
            }
            Self::InvalidSpec {
                sample_rate,
                channels,
            } => write!(
                formatter,
                "unsupported stream spec: {sample_rate} Hz, {channels} channels"
            ),
            Self::InvalidId => formatter.write_str("device id is empty"),
            Self::StreamClosed => formatter.write_str("capture stream is closed"),
            Self::Backend { operation, code } => {
                write!(
                    formatter,
                    "audio backend `{operation}` failed with code {code}"
                )
            }
            Self::Timeout { operation } => {
                write!(formatter, "audio backend `{operation}` timed out")
            }
            Self::BufferTooSmall { frames } => {
                write!(
                    formatter,
                    "output buffer holds {frames} frames; need at least one"
                )
            }
            Self::MisalignedBuffer { len, channels } => write!(
                formatter,
                "output buffer length {len} is not a multiple of {channels} channels"
            ),
            Self::OutputTooSmall { samples, capacity } => write!(
                formatter,
                "output holds {capacity} samples but the input has {samples}"
            ),
            Self::BackendInvariant { operation } => {
                write!(
                    formatter,
                    "audio backend `{operation}` broke the capture contract"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::float_cmp,
        clippy::cast_precision_loss,
        clippy::needless_range_loop
    )]
    use super::*;
    use crate::DeviceId;

    fn id(value: &str) -> DeviceId {
        DeviceId::new(value).unwrap()
    }

    #[test]
    fn display_names_every_variant() {
        let cases = [
            (Error::Unsupported, "no audio capture backend is enabled"),
            (
                Error::NoDevice(Source::Microphone),
                "no microphone device is available",
            ),
            (
                Error::NoDevice(Source::SystemAudio),
                "no system audio device is available",
            ),
            (
                Error::DeviceNotFound(id("missing")),
                "device `missing` was not found",
            ),
            (
                Error::WrongSource {
                    expected: Source::SystemAudio,
                    actual: Source::Microphone,
                },
                "device provides microphone, expected system audio",
            ),
            (
                Error::InvalidSpec {
                    sample_rate: 7_999,
                    channels: 0,
                },
                "unsupported stream spec: 7999 Hz, 0 channels",
            ),
            (Error::InvalidId, "device id is empty"),
            (Error::StreamClosed, "capture stream is closed"),
            (
                Error::Backend {
                    operation: "open",
                    code: -5,
                },
                "audio backend `open` failed with code -5",
            ),
            (
                Error::Timeout {
                    operation: "connect",
                },
                "audio backend `connect` timed out",
            ),
            (
                Error::BufferTooSmall { frames: 0 },
                "output buffer holds 0 frames; need at least one",
            ),
            (
                Error::MisalignedBuffer {
                    len: 7,
                    channels: 2,
                },
                "output buffer length 7 is not a multiple of 2 channels",
            ),
            (
                Error::OutputTooSmall {
                    samples: 8,
                    capacity: 4,
                },
                "output holds 4 samples but the input has 8",
            ),
            (
                Error::BackendInvariant { operation: "read" },
                "audio backend `read` broke the capture contract",
            ),
        ];

        for (error, text) in cases {
            assert_eq!(error.to_string(), text);
        }
    }
}
