//! Failures at the desktop boundary.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Something the desktop app could not do.
#[derive(Debug)]
#[non_exhaustive]
pub enum DesktopError {
    /// The platform capture backend refused a call.
    Capture(koe_capture::Error),
    /// The recorder could not write the file.
    Record(koe_record::Error),
    /// The file system refused an operation.
    Io {
        /// Operation that failed.
        operation: &'static str,
        /// Path the operation used.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
    /// The user asked to record without a source.
    NoSources,
    /// The output path already holds a file.
    OutputExists {
        /// Path that already exists.
        path: PathBuf,
    },
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
            Self::Io { .. } => "The recordings folder is not accessible",
            Self::NoSources => "Select at least one source",
            Self::OutputExists { .. } => "The output file already exists",
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
}

impl fmt::Display for DesktopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capture(error) => write!(formatter, "capture failed: {error}"),
            Self::Record(error) => write!(formatter, "recording failed: {error}"),
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} {} failed: {source}", path.display()),
            Self::NoSources => formatter.write_str("select at least one source"),
            Self::OutputExists { path } => {
                write!(formatter, "{} already exists", path.display())
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
            Self::Io { source, .. } => Some(source),
            Self::NoSources | Self::OutputExists { .. } | Self::Tray(_) | Self::Thread(_) => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::io;

    fn samples() -> Vec<DesktopError> {
        vec![
            DesktopError::Capture(koe_capture::Error::Unsupported),
            DesktopError::Record(koe_record::Error::InvalidPacket),
            DesktopError::Io {
                operation: "create",
                path: PathBuf::from("/tmp/koe"),
                source: io::Error::from(io::ErrorKind::PermissionDenied),
            },
            DesktopError::NoSources,
            DesktopError::OutputExists {
                path: PathBuf::from("/tmp/koe.ogg"),
            },
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
                "The recordings folder is not accessible",
                "Select at least one source",
                "The output file already exists",
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
            DesktopError::OutputExists {
                path: PathBuf::from("/tmp/koe.ogg")
            }
            .to_string(),
            "/tmp/koe.ogg already exists"
        );
        assert_eq!(
            DesktopError::Tray("no host".to_owned()).to_string(),
            "menu bar icon unavailable: no host"
        );
        assert_eq!(
            DesktopError::Thread("spawn failed".to_owned()).to_string(),
            "recorder thread did not start: spawn failed"
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
                DesktopError::Capture(_) | DesktopError::Record(_) | DesktopError::Io { .. }
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
    }
}
