//! User settings and the paths a take writes.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use global_hotkey::hotkey::HotKey;
use koe_record::Format;
use koe_transcribe::Model;

use crate::engine::SessionPlan;
use crate::error::DesktopError;
use crate::hotkey;
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

/// The dictation settings the app persists between launches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DictationSettings {
    /// The binding that starts and stops a dictation.
    pub hotkey: HotKey,
    /// The GGUF model file a dictation loads.
    pub model: Option<PathBuf>,
    /// The BCP 47 language a dictation requests, for example `ja`.
    pub language: Option<String>,
}

impl Default for DictationSettings {
    fn default() -> Self {
        Self {
            hotkey: hotkey::default_hotkey(),
            model: None,
            language: None,
        }
    }
}

impl DictationSettings {
    /// Reads the settings file, falling back field by field.
    ///
    /// A missing file is the first launch. A field the file names but the
    /// app cannot use keeps its default, and the second value says so, so a
    /// hand-edited file never fails silently.
    #[must_use]
    pub fn load(path: &Path) -> (Self, Option<String>) {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return (Self::default(), None);
            }
            Err(error) => {
                return (
                    Self::default(),
                    Some(format!("reading {} failed: {error}", path.display())),
                );
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut hotkey = None;
        let mut model = None;
        let mut language = None;
        let mut recognized = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "hotkey" => {
                    recognized = true;
                    hotkey = Some(hotkey::parse(value));
                }
                "model" => {
                    recognized = true;
                    model = value_or_none(value).map(PathBuf::from);
                }
                "language" => {
                    recognized = true;
                    language = value_or_none(value).map(str::to_owned);
                }
                _ => {}
            }
        }
        let mut settings = Self {
            model,
            language,
            ..Self::default()
        };
        let mut warning = None;
        match hotkey {
            Some(Ok(binding)) => settings.hotkey = binding,
            Some(Err(error)) => {
                warning = Some(format!(
                    "the saved hotkey cannot be used ({error}); using {}",
                    hotkey::display(&settings.hotkey)
                ));
            }
            None => {}
        }
        if !recognized {
            warning.get_or_insert_with(|| {
                format!(
                    "no settings were recognized in {}; using defaults",
                    path.display()
                )
            });
        }
        (settings, warning)
    }

    /// Writes the settings file, replacing whatever was there.
    ///
    /// The file is written whole under a `.partial` name, synced, and renamed
    /// over the target, so a crash cannot leave half a file behind.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopError::Io`] when the file cannot be written,
    /// synced, or renamed.
    pub fn save(&self, path: &Path) -> Result<(), DesktopError> {
        use std::fmt::Write as _;
        use std::io::Write as _;
        let directory = path
            .parent()
            .filter(|directory| !directory.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(directory)
            .map_err(|error| DesktopError::io("create", directory, error))?;
        let mut text = String::from("# koe desktop settings\n");
        let _ = writeln!(text, "hotkey = {}", hotkey::display(&self.hotkey));
        let _ = writeln!(
            text,
            "model = {}",
            self.model
                .as_ref()
                .map_or_else(String::new, |model| model.display().to_string())
        );
        let _ = writeln!(
            text,
            "language = {}",
            self.language.as_deref().unwrap_or_default()
        );
        let partial = partial_path(path);
        let mut file = fs::File::create(&partial)
            .map_err(|error| DesktopError::io("create", &partial, error))?;
        file.write_all(text.as_bytes())
            .map_err(|error| DesktopError::io("write", &partial, error))?;
        file.sync_all()
            .map_err(|error| DesktopError::io("sync", &partial, error))?;
        drop(file);
        if let Err(error) = fs::rename(&partial, path) {
            // A rename that failed leaves no half file behind.
            let _ = fs::remove_file(&partial);
            return Err(DesktopError::io("rename", path, error));
        }
        sync_directory(directory);
        Ok(())
    }
}

/// The value a settings field holds, with its whitespace trimmed: nothing
/// for an empty one.
#[must_use]
pub fn value_or_none(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

/// The `.partial` sibling a write lands in first.
fn partial_path(path: &Path) -> PathBuf {
    let name = path.file_name().map_or_else(
        || std::ffi::OsString::from("settings.conf"),
        |name| {
            let mut name = name.to_os_string();
            name.push(".partial");
            name
        },
    );
    path.with_file_name(name)
}

/// Makes a rename inside `directory` durable. A failure is ignored: the
/// file is already renamed and readable, and some filesystems reject a
/// directory sync.
#[cfg(unix)]
fn sync_directory(directory: &Path) {
    if let Ok(file) = fs::File::open(directory) {
        let _ = file.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) {}

/// The model a dictation runs with, if the platform has one.
///
/// macOS falls back to the system speech model; Linux and Windows need a
/// GGUF path, so [`None`](Option::None) means dictation refuses to start.
#[must_use]
pub fn dictation_model(settings: &DictationSettings) -> Option<Model> {
    let model = settings
        .model
        .as_ref()
        .and_then(|path| path.to_str())
        .map(Model::gguf);
    #[cfg(target_os = "macos")]
    {
        model.or_else(|| Some(Model::system_speech()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        model
    }
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

/// Name of the settings file inside the settings directory.
pub const SETTINGS_FILE: &str = "settings.conf";

/// Where the settings file lives on a fresh install.
#[must_use]
pub fn default_settings_path() -> PathBuf {
    let directory = platform_settings_directory();
    directory.map_or_else(
        || PathBuf::from(SETTINGS_FILE),
        |directory| directory.join(SETTINGS_FILE),
    )
}

#[cfg(target_os = "windows")]
fn platform_settings_directory() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|directory| PathBuf::from(directory).join("koe"))
}

#[cfg(target_os = "macos")]
fn platform_settings_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|directory| PathBuf::from(directory).join("Library/Application Support/koe"))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_settings_directory() -> Option<PathBuf> {
    if let Some(directory) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(directory).join("koe"));
    }
    std::env::var_os("HOME").map(|directory| PathBuf::from(directory).join(".config/koe"))
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

    fn dictation() -> DictationSettings {
        DictationSettings {
            hotkey: hotkey::parse("Alt+F7").unwrap(),
            model: Some(PathBuf::from("/models/tiny.gguf")),
            language: Some("ja".to_owned()),
        }
    }

    #[test]
    fn the_dictation_defaults_are_the_default_binding_and_no_model() {
        let settings = DictationSettings::default();
        assert_eq!(settings.hotkey, hotkey::default_hotkey());
        assert_eq!(settings.model, None);
        assert_eq!(settings.language, None);
    }

    #[test]
    fn a_saved_settings_file_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("koe").join(SETTINGS_FILE);
        dictation().save(&path).unwrap();
        let (loaded, warning) = DictationSettings::load(&path);
        assert_eq!(warning, None);
        assert_eq!(loaded, dictation());
    }

    #[test]
    fn a_saved_file_holds_one_recognized_line_per_field() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(SETTINGS_FILE);
        dictation().save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# koe desktop settings\n\
             hotkey = Alt+F7\n\
             model = /models/tiny.gguf\n\
             language = ja\n"
        );
        assert!(
            !root
                .path()
                .join(format!("{SETTINGS_FILE}.partial"))
                .exists()
        );
    }

    #[test]
    fn saving_twice_leaves_no_partial_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(SETTINGS_FILE);
        dictation().save(&path).unwrap();
        DictationSettings::default().save(&path).unwrap();
        let (loaded, _) = DictationSettings::load(&path);
        assert_eq!(loaded, DictationSettings::default());
        assert!(
            !root
                .path()
                .join(format!("{SETTINGS_FILE}.partial"))
                .exists()
        );
    }

    #[test]
    fn a_missing_file_is_the_first_launch() {
        let root = tempfile::tempdir().unwrap();
        let (loaded, warning) = DictationSettings::load(&root.path().join(SETTINGS_FILE));
        assert_eq!(loaded, DictationSettings::default());
        assert_eq!(warning, None);
    }

    #[test]
    fn a_saved_file_round_trips_without_a_model_or_language() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(SETTINGS_FILE);
        DictationSettings {
            model: None,
            language: None,
            ..dictation()
        }
        .save(&path)
        .unwrap();
        let (loaded, warning) = DictationSettings::load(&path);
        assert_eq!(warning, None);
        assert_eq!(loaded.hotkey, dictation().hotkey);
        assert_eq!(loaded.model, None);
        assert_eq!(loaded.language, None);
    }

    #[test]
    fn an_unusable_saved_hotkey_falls_back_to_the_default_with_a_warning() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(SETTINGS_FILE);
        let text = "# koe desktop settings\nhotkey = Ctrl+Nope\n";
        fs::write(&path, text).unwrap();
        let (loaded, warning) = DictationSettings::load(&path);
        assert_eq!(loaded.hotkey, hotkey::default_hotkey());
        assert!(
            warning
                .as_deref()
                .is_some_and(|warning| warning.contains("Nope"))
        );
    }

    #[test]
    fn a_file_without_a_recognized_setting_warns() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(SETTINGS_FILE);
        fs::write(&path, "this file holds nothing usable\n").unwrap();
        let (loaded, warning) = DictationSettings::load(&path);
        assert_eq!(loaded, DictationSettings::default());
        assert!(
            warning
                .as_deref()
                .is_some_and(|warning| warning.contains("no settings were recognized"))
        );
    }

    #[test]
    fn an_empty_settings_file_warns_instead_of_resetting_silently() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(SETTINGS_FILE);
        fs::write(&path, "").unwrap();
        let (loaded, warning) = DictationSettings::load(&path);
        assert_eq!(loaded, DictationSettings::default());
        assert!(warning.is_some());
    }

    #[test]
    fn comments_and_unknown_keys_are_ignored() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(SETTINGS_FILE);
        let text = "# a hand-written file\ntheme = dark\nhotkey = Ctrl+F9\nhotkey = Alt+D\n";
        fs::write(&path, text).unwrap();
        let (loaded, warning) = DictationSettings::load(&path);
        assert_eq!(warning, None);
        assert_eq!(
            loaded.hotkey,
            hotkey::parse("Alt+D").unwrap(),
            "the last hotkey line wins"
        );
        assert_eq!(loaded.model, None);
        assert_eq!(loaded.language, None);
    }

    #[test]
    fn values_are_trimmed_and_keep_their_equals_sign() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(SETTINGS_FILE);
        let text = "  model =  /models/a=b=1.gguf  \nlanguage =   ja  \n";
        fs::write(&path, text).unwrap();
        let (loaded, _) = DictationSettings::load(&path);
        assert_eq!(loaded.model, Some(PathBuf::from("/models/a=b=1.gguf")));
        assert_eq!(loaded.language, Some("ja".to_owned()));
    }

    #[test]
    fn empty_values_clear_their_field() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(SETTINGS_FILE);
        dictation().save(&path).unwrap();
        let text = "hotkey = Alt+F7\nmodel = \nlanguage = \n";
        fs::write(&path, text).unwrap();
        let (loaded, _) = DictationSettings::load(&path);
        assert_eq!(loaded.hotkey, dictation().hotkey);
        assert_eq!(loaded.model, None);
        assert_eq!(loaded.language, None);
    }

    #[test]
    fn saving_over_an_unwritable_path_fails_with_an_io_error() {
        let root = tempfile::tempdir().unwrap();
        let blocked = root.path().join("blocked");
        fs::write(&blocked, b"").unwrap();
        let path = blocked.join(SETTINGS_FILE);
        assert!(matches!(
            dictation().save(&path),
            Err(DesktopError::Io { .. })
        ));
    }

    #[test]
    fn a_failed_rename_leaves_no_partial_file_behind() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(SETTINGS_FILE);
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            dictation().save(&path),
            Err(DesktopError::Io { .. })
        ));
        assert!(
            !root
                .path()
                .join(format!("{SETTINGS_FILE}.partial"))
                .exists()
        );
    }

    #[test]
    fn a_model_is_resolved_for_the_platform() {
        assert_eq!(
            dictation_model(&dictation()),
            Some(Model::gguf("/models/tiny.gguf"))
        );
        let resolved = dictation_model(&DictationSettings::default());
        #[cfg(target_os = "macos")]
        assert_eq!(resolved, Some(Model::system_speech()));
        #[cfg(not(target_os = "macos"))]
        assert_eq!(resolved, None);
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_model_path_cannot_be_a_model() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let settings = DictationSettings {
            model: Some(PathBuf::from(OsString::from_vec(vec![0xFF]))),
            ..DictationSettings::default()
        };
        assert_eq!(dictation_model(&settings), None);
    }
}
