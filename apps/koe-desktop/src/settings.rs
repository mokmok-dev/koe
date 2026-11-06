//! User settings and the paths a take writes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use koe_record::Format;

use crate::engine::SessionPlan;
use crate::error::DesktopError;
use crate::state::Sources;

/// What the app remembers between takes.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Folder that holds takes.
    pub directory: PathBuf,
    /// Container the next take writes.
    pub format: Format,
    /// Sources the next take records.
    pub sources: Sources,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            directory: default_directory(),
            format: Format::Ogg,
            sources: Sources::MEETING,
        }
    }
}

impl Settings {
    /// Builds the plan for one take.
    ///
    /// # Errors
    ///
    /// Returns the file error when the folder cannot be created, and the
    /// source error when the selection is empty.
    pub fn plan(&self) -> Result<SessionPlan, DesktopError> {
        fs::create_dir_all(&self.directory)
            .map_err(|error| DesktopError::io("create", &self.directory, error))?;
        let output = self.next_output_path()?;
        SessionPlan::meeting(output, self.sources)
    }

    /// Picks a file name that does not exist yet.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopError::OutputExists`] when the folder already holds
    /// one hundred takes from the same second.
    pub fn next_output_path(&self) -> Result<PathBuf, DesktopError> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let extension = self.format.extension();
        for attempt in 0..100_u32 {
            let name = if attempt == 0 {
                format!("koe-{stamp}.{extension}")
            } else {
                format!("koe-{stamp}-{attempt}.{extension}")
            };
            let path = self.directory.join(name);
            if !path.exists() {
                return Ok(path);
            }
        }
        Err(DesktopError::OutputExists {
            path: self.directory.join(format!("koe-{stamp}.{extension}")),
        })
    }
}

/// Folder a fresh install writes to.
#[must_use]
pub fn default_directory() -> PathBuf {
    if let Some(directory) = std::env::var_os("KOE_RECORDINGS_DIR") {
        return PathBuf::from(directory);
    }
    if let Some(directory) = platform_directory() {
        return directory;
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/koe/recordings");
    }
    PathBuf::from("recordings")
}

#[cfg(target_os = "windows")]
fn platform_directory() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|directory| PathBuf::from(directory).join("koe/recordings"))
}

#[cfg(target_os = "macos")]
fn platform_directory() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|directory| {
        PathBuf::from(directory).join("Library/Application Support/koe/recordings")
    })
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_directory() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(|directory| PathBuf::from(directory).join("koe/recordings"))
}

/// Opens `path` in the file manager.
///
/// # Errors
///
/// Returns the spawn error when the platform opener is not installed.
pub fn open_directory(path: &Path) -> Result<(), DesktopError> {
    fs::create_dir_all(path).map_err(|error| DesktopError::io("create", path, error))?;
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = Command::new("explorer");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = Command::new("xdg-open");
    command.arg(path);
    command
        .spawn()
        .map(|_child| ())
        .map_err(|error| DesktopError::io("open", path, error))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_defaults_record_both_sources_as_ogg() {
        let settings = Settings::default();
        assert_eq!(settings.sources, Sources::MEETING);
        assert_eq!(settings.format, Format::Ogg);
    }

    #[test]
    fn an_existing_name_is_skipped() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = Settings {
            directory: directory.path().to_path_buf(),
            ..Settings::default()
        };
        let first = settings.next_output_path().unwrap();
        fs::write(&first, b"").unwrap();
        let second = settings.next_output_path().unwrap();
        assert_ne!(first, second);
        assert_eq!(second.extension().unwrap(), "ogg");
        settings.format = Format::Wav;
        let third = settings.next_output_path().unwrap();
        assert_eq!(third.extension().unwrap(), "wav");
    }

    #[test]
    fn a_plan_creates_the_folder_and_names_the_file() {
        let root = tempfile::tempdir().unwrap();
        let settings = Settings {
            directory: root.path().join("takes"),
            ..Settings::default()
        };
        let plan = settings.plan().unwrap();
        assert!(settings.directory.is_dir());
        assert_eq!(plan.output.parent(), Some(settings.directory.as_path()));
        assert_eq!(plan.output.extension().unwrap(), "ogg");
    }

    #[test]
    fn a_plan_without_a_source_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let settings = Settings {
            directory: root.path().to_path_buf(),
            sources: Sources::EMPTY,
            ..Settings::default()
        };
        assert!(matches!(settings.plan(), Err(DesktopError::NoSources)));
    }
}
