//! Errors crossing the recording boundary.

use alloc::boxed::Box;
use core::fmt;

use koe_capture_core::Source;

/// Something that prevented a recording operation from completing.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The requested recording format is outside the supported range.
    InvalidSpec {
        /// Requested sample rate in hertz.
        sample_rate: u32,
        /// Requested channel count.
        channels: u16,
    },
    /// A sample buffer does not contain whole frames.
    MisalignedBuffer {
        /// Buffer length in samples.
        len: usize,
        /// Channel count the buffer must be a multiple of.
        channels: usize,
    },
    /// A source gain is outside the accepted range.
    InvalidGain {
        /// Source whose gain was rejected.
        source: Source,
    },
    /// An Opus packet is empty or carries no samples.
    InvalidPacket,
    /// A packet needs more Ogg lacing segments than one page can describe.
    PacketTooLarge {
        /// Packet length in bytes.
        bytes: usize,
    },
    /// A container or recording invariant was violated.
    ContainerInvariant {
        /// Operation that broke the invariant.
        operation: &'static str,
    },
    /// A file operation failed.
    Io {
        /// Operation that failed.
        operation: &'static str,
        /// Native error code, or zero when the platform has none.
        code: i32,
    },
    /// The format cannot carry the requested sample rate.
    UnsupportedRate {
        /// Requested sample rate in hertz.
        rate: u32,
    },
    /// The sample data outgrew what the container's size field can describe.
    RecordingTooLarge {
        /// Bytes the data chunk would hold.
        bytes: u64,
        /// Most bytes the size field can describe.
        max: u64,
    },
    /// A source delivers a channel layout the recording cannot mix.
    UnsupportedChannels {
        /// Source that delivers the layout.
        source: Source,
        /// Channel count the source delivers.
        channels: u16,
    },
    /// The source has no registered stream.
    SourceNotRegistered(Source),
    /// The source is registered with a different format.
    SourceAlreadyRegistered(Source),
    /// The source has already ended.
    SourceEnded(Source),
    /// The source's queue cannot accept another write.
    InputOverflow(Source),
    /// A codec call failed.
    Encode {
        /// Operation that failed.
        operation: &'static str,
        /// Native error code.
        code: i32,
    },
    /// The path extension names no supported format.
    UnknownFormat {
        /// Extension that was not recognized, without the dot.
        extension: Box<str>,
    },
    /// A capture stream delivers a different rate than the recording.
    RateMismatch {
        /// Rate the recording is written at.
        expected: u32,
        /// Rate the capture stream delivers.
        actual: u32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSpec {
                sample_rate,
                channels,
            } => write!(
                formatter,
                "unsupported recording spec: {sample_rate} Hz, {channels} channels"
            ),
            Self::MisalignedBuffer { len, channels } => write!(
                formatter,
                "buffer length {len} is not a multiple of {channels} channels"
            ),
            Self::InvalidGain { source } => {
                write!(formatter, "the {source} gain is outside the accepted range")
            }
            Self::InvalidPacket => {
                formatter.write_str("the Opus packet is empty or carries no samples")
            }
            Self::PacketTooLarge { bytes } => write!(
                formatter,
                "a {bytes}-byte packet needs more Ogg segments than a page holds"
            ),
            Self::ContainerInvariant { operation } => {
                write!(
                    formatter,
                    "recording `{operation}` broke a stream invariant"
                )
            }
            Self::Io { operation, code } => {
                write!(formatter, "file `{operation}` failed with code {code}")
            }
            Self::UnsupportedRate { rate } => {
                write!(formatter, "the recording format cannot carry {rate} Hz")
            }
            Self::RecordingTooLarge { bytes, max } => write!(
                formatter,
                "the recording needs {bytes} bytes of sample data; the container holds at most {max}"
            ),
            Self::UnsupportedChannels { source, channels } => write!(
                formatter,
                "{source} delivers {channels} channels; only mono and stereo can be mixed"
            ),
            Self::SourceNotRegistered(source) => {
                write!(formatter, "no {source} source is registered")
            }
            Self::SourceAlreadyRegistered(source) => {
                write!(
                    formatter,
                    "{source} is already registered with another stream format"
                )
            }
            Self::SourceEnded(source) => write!(formatter, "{source} has ended"),
            Self::InputOverflow(source) => write!(formatter, "the {source} queue is full"),
            Self::Encode { operation, code } => {
                write!(
                    formatter,
                    "Opus encoder `{operation}` failed with code {code}"
                )
            }
            Self::UnknownFormat { extension } => {
                write!(formatter, "no recording format matches `.{extension}`")
            }
            Self::RateMismatch { expected, actual } => write!(
                formatter,
                "capture stream delivers {actual} Hz; the recording is {expected} Hz"
            ),
        }
    }
}

impl core::error::Error for Error {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn display_names_every_variant() {
        let cases = [
            (
                Error::InvalidSpec {
                    sample_rate: 7_999,
                    channels: 0,
                },
                "unsupported recording spec: 7999 Hz, 0 channels",
            ),
            (
                Error::MisalignedBuffer {
                    len: 7,
                    channels: 2,
                },
                "buffer length 7 is not a multiple of 2 channels",
            ),
            (
                Error::InvalidGain {
                    source: Source::SystemAudio,
                },
                "the system audio gain is outside the accepted range",
            ),
            (
                Error::InvalidPacket,
                "the Opus packet is empty or carries no samples",
            ),
            (
                Error::PacketTooLarge { bytes: 65_025 },
                "a 65025-byte packet needs more Ogg segments than a page holds",
            ),
            (
                Error::ContainerInvariant {
                    operation: "finish",
                },
                "recording `finish` broke a stream invariant",
            ),
            (
                Error::Io {
                    operation: "create",
                    code: 13,
                },
                "file `create` failed with code 13",
            ),
            (
                Error::UnsupportedRate { rate: 44_100 },
                "the recording format cannot carry 44100 Hz",
            ),
            (
                Error::RecordingTooLarge {
                    bytes: 4_294_967_260,
                    max: 4_294_967_259,
                },
                "the recording needs 4294967260 bytes of sample data; the container holds at most 4294967259",
            ),
            (
                Error::UnsupportedChannels {
                    source: Source::Microphone,
                    channels: 6,
                },
                "microphone delivers 6 channels; only mono and stereo can be mixed",
            ),
            (
                Error::SourceNotRegistered(Source::Microphone),
                "no microphone source is registered",
            ),
            (
                Error::SourceAlreadyRegistered(Source::SystemAudio),
                "system audio is already registered with another stream format",
            ),
            (
                Error::SourceEnded(Source::Microphone),
                "microphone has ended",
            ),
            (
                Error::InputOverflow(Source::SystemAudio),
                "the system audio queue is full",
            ),
            (
                Error::Encode {
                    operation: "encode",
                    code: -1,
                },
                "Opus encoder `encode` failed with code -1",
            ),
            (
                Error::UnknownFormat {
                    extension: "wav".into(),
                },
                "no recording format matches `.wav`",
            ),
            (
                Error::RateMismatch {
                    expected: 48_000,
                    actual: 44_100,
                },
                "capture stream delivers 44100 Hz; the recording is 48000 Hz",
            ),
        ];

        for (error, text) in cases {
            assert_eq!(error.to_string(), text);
        }
    }
}
