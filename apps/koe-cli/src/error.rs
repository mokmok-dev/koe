//! The application's error type.

use std::fmt;

use koe_capture::Error as CaptureError;
use koe_record::Error as RecordError;
use koe_transcribe::Error as TranscribeError;

use crate::wav;

/// Something the command could not do.
#[derive(Debug)]
pub enum AppError {
    /// The arguments ask for something the tool cannot do.
    Usage(String),
    /// A capture device operation failed.
    Capture(CaptureError),
    /// A recording operation failed.
    Record(RecordError),
    /// A speech-to-text operation failed.
    Transcribe(TranscribeError),
    /// The audio file is not a WAV this build reads.
    Wav(wav::Error),
    /// The recorder was finished before this operation ran.
    RecorderFinished,
}

impl AppError {
    /// The process exit code this error asks for.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_)
            | Self::Record(
                RecordError::InvalidSpec { .. }
                | RecordError::UnsupportedRate { .. }
                | RecordError::UnknownFormat { .. },
            ) => 2,
            _ => 1,
        }
    }

    /// A usage error carrying a message for the user.
    #[must_use]
    pub fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => formatter.write_str(message),
            Self::Capture(error) => write!(formatter, "capture failed: {error}"),
            Self::Record(error) => write!(formatter, "recording failed: {error}"),
            Self::Transcribe(error) => write!(formatter, "transcription failed: {error}"),
            Self::Wav(error) => write!(formatter, "audio file: {error}"),
            Self::RecorderFinished => formatter.write_str("the recorder was already finished"),
        }
    }
}

impl From<CaptureError> for AppError {
    fn from(error: CaptureError) -> Self {
        Self::Capture(error)
    }
}

impl From<RecordError> for AppError {
    fn from(error: RecordError) -> Self {
        Self::Record(error)
    }
}

impl From<TranscribeError> for AppError {
    fn from(error: TranscribeError) -> Self {
        Self::Transcribe(error)
    }
}

impl From<wav::Error> for AppError {
    fn from(error: wav::Error) -> Self {
        Self::Wav(error)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn usage_errors_exit_two_and_runtime_errors_exit_one() {
        assert_eq!(AppError::usage("bad flag").exit_code(), 2);
        assert_eq!(AppError::RecorderFinished.exit_code(), 1);
        assert_eq!(AppError::Capture(CaptureError::Unsupported).exit_code(), 1);
        assert_eq!(
            AppError::Record(RecordError::UnsupportedRate { rate: 44_100 }).exit_code(),
            2
        );
        assert_eq!(
            AppError::Record(RecordError::InvalidSpec {
                sample_rate: 0,
                channels: 0
            })
            .exit_code(),
            2
        );
        assert_eq!(
            AppError::Record(RecordError::UnknownFormat {
                extension: "mp3".into()
            })
            .exit_code(),
            2
        );
        assert_eq!(AppError::Record(RecordError::InvalidPacket).exit_code(), 1);
    }

    #[test]
    fn every_error_names_its_cause() {
        let cases = [
            (AppError::usage("bad flag"), "bad flag".to_owned()),
            (
                AppError::Capture(CaptureError::Unsupported),
                "capture failed: no audio capture backend is enabled".to_owned(),
            ),
            (
                AppError::Record(RecordError::UnknownFormat {
                    extension: "mp3".into(),
                }),
                "recording failed: no recording format matches `.mp3`".to_owned(),
            ),
            (
                AppError::Transcribe(TranscribeError::Unsupported),
                "transcription failed: no speech-to-text backend is enabled".to_owned(),
            ),
            (
                AppError::Wav(wav::Error::NotWave),
                "audio file: not a RIFF/WAVE file".to_owned(),
            ),
            (
                AppError::RecorderFinished,
                "the recorder was already finished".to_owned(),
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn domain_errors_convert_to_their_variant() {
        assert!(matches!(
            AppError::from(CaptureError::Unsupported),
            AppError::Capture(CaptureError::Unsupported)
        ));
        assert!(matches!(
            AppError::from(RecordError::InvalidPacket),
            AppError::Record(RecordError::InvalidPacket)
        ));
        assert!(matches!(
            AppError::from(TranscribeError::Unsupported),
            AppError::Transcribe(TranscribeError::Unsupported)
        ));
        assert!(matches!(
            AppError::from(wav::Error::NotWave),
            AppError::Wav(wav::Error::NotWave)
        ));
    }
}
