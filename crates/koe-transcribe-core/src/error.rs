//! Errors crossing the transcription boundary.

use alloc::boxed::Box;
use core::fmt;

/// Something that prevented a transcription operation from completing.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// No transcription backend is enabled for the current target.
    Unsupported,
    /// The platform version lacks an API the requested transcription needs.
    Unavailable {
        /// Capability that is unavailable.
        feature: &'static str,
    },
    /// A model path did not name a readable file.
    ModelNotFound {
        /// Path the caller supplied.
        path: Box<str>,
    },
    /// A model file exists but is not a model this backend can load.
    ModelInvalid {
        /// Path the caller supplied.
        path: Box<str>,
        /// Why the file was rejected.
        reason: &'static str,
    },
    /// The backend needs assets for a language it does not have installed.
    ModelMissing {
        /// Language tag the caller asked for.
        language: Box<str>,
    },
    /// A session option is outside the range the backend accepts.
    OptionsInvalid {
        /// Option that was rejected.
        option: &'static str,
    },
    /// The requested audio format is outside the supported range.
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
    /// A native inference call failed.
    Backend {
        /// Operation that failed.
        operation: &'static str,
        /// Native error code.
        code: i32,
    },
    /// A backend violated the private transcription contract.
    BackendInvariant {
        /// Operation that broke the contract.
        operation: &'static str,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => formatter.write_str("no speech-to-text backend is enabled"),
            Self::Unavailable { feature } => {
                write!(
                    formatter,
                    "{feature} is not available on this platform version"
                )
            }
            Self::ModelNotFound { path } => write!(formatter, "model `{path}` was not found"),
            Self::ModelInvalid { path, reason } => {
                write!(formatter, "model `{path}` is invalid: {reason}")
            }
            Self::ModelMissing { language } => {
                write!(formatter, "no speech model is installed for `{language}`")
            }
            Self::OptionsInvalid { option } => {
                write!(formatter, "session option `{option}` is outside its range")
            }
            Self::InvalidSpec {
                sample_rate,
                channels,
            } => write!(
                formatter,
                "unsupported audio spec: {sample_rate} Hz, {channels} channels"
            ),
            Self::MisalignedBuffer { len, channels } => write!(
                formatter,
                "buffer length {len} is not a multiple of {channels} channels"
            ),
            Self::Backend { operation, code } => {
                write!(
                    formatter,
                    "speech backend `{operation}` failed with code {code}"
                )
            }
            Self::BackendInvariant { operation } => {
                write!(
                    formatter,
                    "speech backend `{operation}` broke the transcription contract"
                )
            }
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
            (Error::Unsupported, "no speech-to-text backend is enabled"),
            (
                Error::Unavailable {
                    feature: "SpeechAnalyzer",
                },
                "SpeechAnalyzer is not available on this platform version",
            ),
            (
                Error::ModelNotFound {
                    path: "/models/missing.gguf".into(),
                },
                "model `/models/missing.gguf` was not found",
            ),
            (
                Error::ModelInvalid {
                    path: "/models/bad.gguf".into(),
                    reason: "unsupported family",
                },
                "model `/models/bad.gguf` is invalid: unsupported family",
            ),
            (
                Error::ModelMissing {
                    language: "ja".into(),
                },
                "no speech model is installed for `ja`",
            ),
            (
                Error::OptionsInvalid { option: "language" },
                "session option `language` is outside its range",
            ),
            (
                Error::InvalidSpec {
                    sample_rate: 7_999,
                    channels: 0,
                },
                "unsupported audio spec: 7999 Hz, 0 channels",
            ),
            (
                Error::MisalignedBuffer {
                    len: 7,
                    channels: 2,
                },
                "buffer length 7 is not a multiple of 2 channels",
            ),
            (
                Error::Backend {
                    operation: "decode",
                    code: -5,
                },
                "speech backend `decode` failed with code -5",
            ),
            (
                Error::BackendInvariant {
                    operation: "session task",
                },
                "speech backend `session task` broke the transcription contract",
            ),
        ];

        for (error, text) in cases {
            assert_eq!(error.to_string(), text);
        }
    }
}
