//! Failures at the desktop boundary.

use std::fmt;
use std::io;
use std::path::PathBuf;

use koe_session_store::Error as StoreError;

/// Something the desktop app could not do.
#[derive(Debug)]
#[non_exhaustive]
pub enum DesktopError {
    /// The platform capture backend refused a call.
    Capture(koe_capture::Error),
    /// The recorder could not write the file.
    Record(koe_record::Error),
    /// The meeting session could not be created, saved, or discarded.
    Store(StoreError),
    /// The file system refused an operation on the recordings folder.
    Io {
        /// Operation that failed.
        operation: &'static str,
        /// Path the operation used.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
    /// The settings file could not be read or written.
    Settings {
        /// Operation that failed.
        operation: &'static str,
        /// Path the operation used.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
    /// The user asked to record without a source.
    NoSources,
    /// The transcription backend refused a call.
    Transcribe(koe_transcribe::Error),
    /// A dictation could not run to typed text.
    Dictation(String),
    /// The menu bar icon could not be created.
    Tray(String),
    /// The recorder thread could not start.
    Thread(String),
}

impl DesktopError {
    /// Short sentence a person can read in the tray or the window.
    #[must_use]
    pub const fn headline(&self) -> &'static str {
        match self {
            Self::Capture(_) => "Audio capture failed",
            Self::Record(_) => "The recording could not be written",
            Self::Store(_) => "The meeting session could not be saved",
            Self::Io { .. } => "The recordings folder is not accessible",
            Self::Settings { .. } => "The settings file could not be saved",
            Self::NoSources => "Select at least one source",
            Self::Transcribe(_) => "Transcription failed",
            Self::Dictation(_) => "Dictation failed",
            Self::Tray(_) => "The menu bar icon is unavailable",
            Self::Thread(_) => "The recorder did not start",
        }
    }

    /// Builds an I/O error with the path it used.
    #[must_use]
    pub fn io(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }

    /// Builds a settings-file error with the path it used.
    #[must_use]
    pub fn settings(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Settings {
            operation,
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for DesktopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capture(error) => write!(formatter, "capture failed: {error}"),
            Self::Record(error) => write!(formatter, "recording failed: {error}"),
            Self::Store(error) => write!(formatter, "meeting session failed: {error}"),
            Self::Io {
                operation,
                path,
                source,
            }
            | Self::Settings {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} {} failed: {source}", path.display()),
            Self::NoSources => formatter.write_str("select at least one source"),
            Self::Transcribe(error) => write!(formatter, "transcription failed: {error}"),
            Self::Dictation(message) => {
                write!(formatter, "dictation failed: {message}")
            }
            Self::Tray(message) => write!(formatter, "menu bar icon unavailable: {message}"),
            Self::Thread(message) => write!(formatter, "recorder thread did not start: {message}"),
        }
    }
}

impl std::error::Error for DesktopError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Capture(error) => Some(error),
            Self::Record(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Io { source, .. } | Self::Settings { source, .. } => Some(source),
            Self::Transcribe(error) => Some(error),
            Self::NoSources | Self::Dictation(_) | Self::Tray(_) | Self::Thread(_) => None,
        }
    }
}

impl From<koe_capture::Error> for DesktopError {
    fn from(error: koe_capture::Error) -> Self {
        Self::Capture(error)
    }
}

impl From<koe_record::Error> for DesktopError {
    fn from(error: koe_record::Error) -> Self {
        Self::Record(error)
    }
}

impl From<StoreError> for DesktopError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::io;

    fn samples() -> Vec<DesktopError> {
        vec![
            DesktopError::Capture(koe_capture::Error::Unsupported),
            DesktopError::Record(koe_record::Error::InvalidPacket),
            DesktopError::Store(StoreError::IdCollision),
            DesktopError::Io {
                operation: "create",
                path: PathBuf::from("/tmp/koe"),
                source: io::Error::from(io::ErrorKind::PermissionDenied),
            },
            DesktopError::Settings {
                operation: "write",
                path: PathBuf::from("/tmp/koe/settings.conf"),
                source: io::Error::from(io::ErrorKind::PermissionDenied),
            },
            DesktopError::NoSources,
            DesktopError::Transcribe(koe_transcribe::Error::Unsupported),
            DesktopError::Dictation("no display".to_owned()),
            DesktopError::Tray("no host".to_owned()),
            DesktopError::Thread("spawn failed".to_owned()),
        ]
    }

    #[test]
    fn every_kind_has_its_own_headline() {
        let headlines: Vec<_> = samples().iter().map(DesktopError::headline).collect();
        assert_eq!(
            headlines,
            [
                "Audio capture failed",
                "The recording could not be written",
                "The meeting session could not be saved",
                "The recordings folder is not accessible",
                "The settings file could not be saved",
                "Select at least one source",
                "Transcription failed",
                "Dictation failed",
                "The menu bar icon is unavailable",
                "The recorder did not start",
            ]
        );
    }

    #[test]
    fn display_names_the_failure_and_its_path() {
        for error in samples() {
            assert_ne!(error.to_string(), "");
        }
        assert_eq!(
            DesktopError::NoSources.to_string(),
            "select at least one source"
        );
        assert_eq!(
            DesktopError::Store(StoreError::IdCollision).to_string(),
            "meeting session failed: every meeting id within 16 milliseconds of the requested time is taken"
        );
        assert_eq!(
            DesktopError::Tray("no host".to_owned()).to_string(),
            "menu bar icon unavailable: no host"
        );
        assert_eq!(
            DesktopError::Thread("spawn failed".to_owned()).to_string(),
            "recorder thread did not start: spawn failed"
        );
        assert_eq!(
            DesktopError::Dictation("no display".to_owned()).to_string(),
            "dictation failed: no display"
        );
        assert_eq!(
            DesktopError::Transcribe(koe_transcribe::Error::Unsupported).to_string(),
            "transcription failed: no speech-to-text backend is enabled"
        );
        let io_error = DesktopError::io(
            "create",
            "/tmp/koe",
            io::Error::from(io::ErrorKind::PermissionDenied),
        );
        assert!(io_error.to_string().contains("create /tmp/koe failed"));
    }

    #[test]
    fn only_wrapped_failures_have_a_source() {
        for error in samples() {
            let expected = matches!(
                error,
                DesktopError::Capture(_)
                    | DesktopError::Record(_)
                    | DesktopError::Store(_)
                    | DesktopError::Io { .. }
                    | DesktopError::Settings { .. }
                    | DesktopError::Transcribe(_)
            );
            assert_eq!(error.source().is_some(), expected);
        }
    }

    #[test]
    fn io_keeps_the_operation_and_path() {
        let error = DesktopError::io(
            "create",
            "/tmp/koe",
            io::Error::from(io::ErrorKind::PermissionDenied),
        );
        assert!(matches!(
            &error,
            DesktopError::Io {
                operation,
                path,
                ..
            } if *operation == "create" && path == &PathBuf::from("/tmp/koe")
        ));
    }

    #[test]
    fn wrapped_errors_keep_their_kind() {
        assert!(matches!(
            DesktopError::from(koe_capture::Error::Unsupported),
            DesktopError::Capture(_)
        ));
        assert!(matches!(
            DesktopError::from(koe_record::Error::InvalidPacket),
            DesktopError::Record(_)
        ));
        assert!(matches!(
            DesktopError::from(StoreError::IdCollision),
            DesktopError::Store(_)
        ));
    }
}
