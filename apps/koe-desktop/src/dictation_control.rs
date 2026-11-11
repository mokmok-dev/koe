//! The dictation side of a shell.
//!
//! The shell embeds one [`DictationControl`]; mounting the hotkey, driving
//! the machine, and running the worker are written once. What stays with the
//! shell is its own: the notice slot, the window, and the meeting take.

use std::path::{Path, PathBuf};
use std::time::Instant;

use global_hotkey::hotkey::HotKey;

use crate::dictation::{self, Dictation, DictationContext, DictationEvent, DictationOutcome};
use crate::dictation_worker;
use crate::error::DesktopError;
use crate::hotkey::{self, Hotkeys};
use crate::settings::{self, DictationSettings};

/// What a hotkey press left to tell the user.
#[derive(Debug, PartialEq, Eq)]
pub enum Press {
    /// The press was refused; nothing started.
    Refused(String),
    /// The press was accepted, but the dictation thread did not start.
    Failed(String),
}

/// The dictation machine, thread, settings, and hotkey of one shell.
pub struct DictationControl {
    /// The dictation state machine.
    pub dictation: Dictation,
    /// The running dictation thread, if one is active.
    pub session: Option<dictation_worker::Session>,
    /// The settings the dictation uses, persisted as they change.
    pub settings: DictationSettings,
    /// The registered binding, when the desktop accepted one.
    pub hotkeys: Option<Hotkeys>,
    /// Why no binding is registered, when none is.
    pub hotkey_error: Option<String>,
    settings_path: PathBuf,
}

impl DictationControl {
    /// Loads the settings from their default path.
    ///
    /// # Errors
    ///
    /// Never fails: unusable fields fall back to their defaults and the
    /// second value is the warning to show.
    #[must_use]
    pub fn load() -> (Self, Option<String>) {
        Self::load_at(&settings::default_settings_path())
    }

    /// Loads the settings from `path`.
    ///
    /// # Errors
    ///
    /// Never fails: unusable fields fall back to their defaults and the
    /// second value is the warning to show.
    #[must_use]
    pub fn load_at(path: &Path) -> (Self, Option<String>) {
        let (settings, warning) = DictationSettings::load(path);
        (
            Self {
                dictation: Dictation::Idle,
                session: None,
                settings,
                hotkeys: None,
                hotkey_error: None,
                settings_path: path.to_path_buf(),
            },
            warning,
        )
    }

    /// Registers the binding, or keeps the failure for the window banner.
    ///
    /// Called every poll; it works at most once, because a failure latches
    /// until [`Self::apply_hotkey`] retries it.
    ///
    /// # Errors
    ///
    /// Returns the failure to show as a notice, when registration fails.
    pub fn mount(&mut self) -> Option<String> {
        if self.hotkeys.is_some() || self.hotkey_error.is_some() {
            return None;
        }
        match Hotkeys::new(self.settings.hotkey) {
            Ok(hotkeys) => {
                self.hotkeys = Some(hotkeys);
                None
            }
            Err(error) => {
                self.hotkey_error = Some(error);
                Some(
                    "No global hotkey. Dictation needs a desktop that lets apps register shortcuts."
                        .to_owned(),
                )
            }
        }
    }

    /// Hotkey presses waiting to be applied.
    #[must_use]
    pub fn presses(&self) -> usize {
        self.hotkeys.as_ref().map_or(0, Hotkeys::presses)
    }

    /// Applies one hotkey press, given whether a meeting take is active.
    ///
    /// # Errors
    ///
    /// Returns the refusal or the start failure to show, when the press does
    /// not start or stop a dictation. A refusal is the hotkey's own answer and
    /// belongs in the HUD; a failure is the dictation thread's, and the window
    /// notice is enough for it.
    pub fn toggle(&mut self, meeting_active: bool) -> Option<Press> {
        let context = DictationContext {
            meeting_active,
            model: settings::dictation_model(&self.settings),
            language: self
                .settings
                .language
                .as_ref()
                .map(|language| language.as_str().into()),
        };
        let action = self.dictation.toggle(context, Instant::now())?;
        let refused = matches!(&action, dictation::DictationAction::Refuse(_));
        self.run(action).map(|message| {
            if refused {
                Press::Refused(message)
            } else {
                Press::Failed(message)
            }
        })
    }

    /// Takes the dictation thread's report, if it has one.
    ///
    /// A thread that died without reporting becomes a visible failure, so
    /// the machine cannot wait forever on a channel nobody fills.
    ///
    /// # Errors
    ///
    /// Returns the result to show as a notice, when the dictation ends.
    pub fn poll(&mut self) -> Option<String> {
        let report = self.session.as_ref()?.poll();
        let outcome = match report {
            dictation_worker::Report::Pending => return None,
            dictation_worker::Report::Done(outcome) => outcome,
            dictation_worker::Report::Lost => Box::new(DictationOutcome {
                text: String::new(),
                failure: Some(DesktopError::Thread(
                    "the dictation thread ended without reporting".to_owned(),
                )),
            }),
        };
        self.session = None;
        if let Some(action) = self
            .dictation
            .reduce(DictationEvent::Finished(outcome), Instant::now())
        {
            return self.run(action);
        }
        match &self.dictation {
            Dictation::Done(done) if done.failed() => Some(
                done.failure
                    .as_ref()
                    .map_or_else(|| "Dictation heard nothing".to_owned(), ToString::to_string),
            ),
            Dictation::Done(done) => Some(format!("Typed {}", done.text)),
            Dictation::Idle | Dictation::Listening { .. } | Dictation::Stopping { .. } => None,
        }
    }

    /// Discards the running dictation; nothing is inserted.
    ///
    /// Returns whether a dictation was active.
    pub fn cancel(&mut self) -> bool {
        let Some(action) = self.dictation.cancel() else {
            return false;
        };
        let _ = self.run(action);
        true
    }

    /// Stops the dictation so it can commit before the app quits.
    pub fn stop(&mut self) {
        if let Some(session) = &self.session {
            session.stop();
        }
    }

    /// Applies the hotkey field's draft: rebind, or retry the mount.
    ///
    /// The binding is committed here rather than while the user types,
    /// because every valid intermediate prefix of a binding is a real
    /// shortcut: typing toward `Ctrl+Shift+Space` passes through `Ctrl+S`,
    /// which a live rebind would grab system-wide. A draft that does not
    /// parse changes nothing; the field shows the parser's complaint. A
    /// registration that fails changes nothing either: what stays armed is
    /// what stays saved, so the field's hint keeps asking for Enter.
    ///
    /// # Errors
    ///
    /// Returns the failure to show as a notice, when the binding cannot be
    /// registered.
    pub fn apply_hotkey(&mut self, draft: &str) -> Option<String> {
        let Ok(hotkey) = hotkey::parse(draft.trim()) else {
            return None;
        };
        if let Some(hotkeys) = &mut self.hotkeys {
            if hotkeys.rebind(hotkey).is_err() {
                return Some(
                    "the new hotkey could not be registered; the previous binding stays active"
                        .to_owned(),
                );
            }
        } else if let Some(error) = self.remount(hotkey) {
            self.hotkey_error = Some(error.clone());
            return Some(format!("the hotkey could not be registered ({error})"));
        }
        if hotkey == self.settings.hotkey {
            return None;
        }
        self.settings.hotkey = hotkey;
        if let Err(error) = self.settings.save(&self.settings_path) {
            return Some(error.to_string());
        }
        None
    }

    /// Applies the model and language drafts, persisting what changed.
    ///
    /// # Errors
    ///
    /// Returns the save error when the settings file cannot be written.
    pub fn apply_settings(
        &mut self,
        model: Option<PathBuf>,
        language: Option<String>,
    ) -> Result<bool, DesktopError> {
        let mut changed = false;
        if model != self.settings.model {
            self.settings.model = model;
            changed = true;
        }
        if language != self.settings.language {
            self.settings.language = language;
            changed = true;
        }
        if changed {
            self.settings.save(&self.settings_path)?;
        }
        Ok(changed)
    }

    /// Clears the finished dictation's result.
    pub fn dismiss(&mut self) {
        let _ = self
            .dictation
            .reduce(DictationEvent::Dismiss, Instant::now());
    }

    /// Registers `hotkey` as a mount or a retry of a failed one.
    fn remount(&mut self, hotkey: HotKey) -> Option<String> {
        match Hotkeys::new(hotkey) {
            Ok(hotkeys) => {
                self.hotkeys = Some(hotkeys);
                self.hotkey_error = None;
                None
            }
            Err(error) => Some(error),
        }
    }

    /// Runs the work the machine asked for.
    fn run(&mut self, action: dictation::DictationAction) -> Option<String> {
        match action {
            dictation::DictationAction::Start(plan) => {
                match dictation_worker::Session::start(plan) {
                    Ok(session) => {
                        self.session = Some(session);
                        None
                    }
                    Err(error) => {
                        self.session = None;
                        let message = error.to_string();
                        let outcome = Box::new(DictationOutcome {
                            text: String::new(),
                            failure: Some(error),
                        });
                        if let Some(action) = self
                            .dictation
                            .reduce(DictationEvent::Finished(outcome), Instant::now())
                        {
                            return self.run(action);
                        }
                        Some(message)
                    }
                }
            }
            dictation::DictationAction::Stop => {
                self.stop();
                None
            }
            dictation::DictationAction::Cancel => {
                if let Some(session) = &self.session {
                    session.cancel();
                }
                None
            }
            dictation::DictationAction::Refuse(message) => Some(message.to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;
    #[cfg(not(target_os = "macos"))]
    use std::time::Duration;

    use super::*;

    fn control(dir: &std::path::Path) -> DictationControl {
        let (control, warning) = DictationControl::load_at(&dir.join(settings::SETTINGS_FILE));
        assert_eq!(warning, None);
        control
    }

    /// Waits until the dictation thread has reported, with a bound.
    #[cfg(not(target_os = "macos"))]
    fn reported(control: &mut DictationControl) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                Instant::now() < deadline,
                "the dictation thread did not report"
            );
            let _ = control.poll();
            if control.session.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    // On macOS the system speech model always resolves, so the no-model
    // refusal this pins cannot happen there.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_press_without_a_model_is_refused_without_a_thread() {
        let root = tempfile::tempdir().unwrap();
        let mut control = control(root.path());
        let message = control.toggle(false).unwrap();
        assert_eq!(message, Press::Refused(dictation::MODEL_MISSING.to_owned()));
        assert!(control.session.is_none());
        assert!(matches!(control.dictation, Dictation::Idle));
    }

    #[test]
    fn a_press_during_a_meeting_is_refused_without_a_thread() {
        let root = tempfile::tempdir().unwrap();
        let mut control = control(root.path());
        control.settings.model = Some(PathBuf::from("tiny.gguf"));
        let message = control.toggle(true).unwrap();
        assert_eq!(
            message,
            Press::Refused(dictation::MEETING_ACTIVE.to_owned())
        );
        assert!(control.session.is_none());
    }

    // The microphone opens before the model loads, and a headless macOS
    // runner never answers the open, so the real thread this exercises
    // cannot report there within a bound.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_cancel_discards_the_dictation_the_thread_captured() {
        let root = tempfile::tempdir().unwrap();
        let mut control = control(root.path());
        control.settings.model = Some(PathBuf::from("tiny.gguf"));
        assert_eq!(control.toggle(false), None);
        assert!(control.session.is_some());
        assert!(control.dictation.is_active());

        assert!(control.cancel());
        assert!(matches!(control.dictation, Dictation::Idle));
        // Whatever the thread captured or failed to capture, it reports and
        // a discarded take surfaces nothing.
        reported(&mut control);
        assert!(matches!(control.dictation, Dictation::Idle));
        assert!(!control.dictation.is_active());
    }

    #[test]
    fn a_cancel_on_an_idle_control_does_nothing() {
        let root = tempfile::tempdir().unwrap();
        let mut control = control(root.path());
        assert!(!control.cancel());
        assert!(matches!(control.dictation, Dictation::Idle));
    }

    #[test]
    fn a_hotkey_draft_that_does_not_parse_changes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let mut control = control(root.path());
        assert_eq!(control.apply_hotkey("Ctrl+Nope"), None);
        assert_eq!(control.settings.hotkey, hotkey::default_hotkey());
        let (loaded, _) = DictationSettings::load(&root.path().join(settings::SETTINGS_FILE));
        assert_eq!(loaded.hotkey, hotkey::default_hotkey());
    }

    #[test]
    fn the_model_and_language_drafts_persist_when_they_change() {
        let root = tempfile::tempdir().unwrap();
        let mut control = control(root.path());
        assert!(control.apply_settings(None, Some("ja".to_owned())).unwrap());
        assert_eq!(control.settings.language, Some("ja".to_owned()));
        // Applying the same values again changes nothing.
        assert!(!control.apply_settings(None, Some("ja".to_owned())).unwrap());
        let changed = control
            .apply_settings(Some(PathBuf::from("tiny.gguf")), Some("ja".to_owned()))
            .unwrap();
        assert!(changed);
        let (loaded, _) = DictationSettings::load(&root.path().join(settings::SETTINGS_FILE));
        assert_eq!(loaded.model, Some(PathBuf::from("tiny.gguf")));
        assert_eq!(loaded.language, Some("ja".to_owned()));
    }

    #[test]
    fn presses_stay_at_zero_without_hotkeys() {
        let root = tempfile::tempdir().unwrap();
        let control = control(root.path());
        assert_eq!(control.presses(), 0);
    }

    #[test]
    fn a_second_mount_does_nothing_after_the_first() {
        let root = tempfile::tempdir().unwrap();
        let mut control = control(root.path());
        let _ = control.mount();
        assert_eq!(control.mount(), None);
    }

    // The microphone opens before the model loads, and a headless macOS
    // runner never answers the open, so the real thread this exercises
    // cannot report there within a bound.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_failed_dictation_surfaces_its_notice_and_dismisses() {
        let root = tempfile::tempdir().unwrap();
        let mut control = control(root.path());
        control.settings.model = Some(PathBuf::from("tiny.gguf"));
        assert_eq!(control.toggle(false), None);
        assert!(control.session.is_some());

        let mut notice = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while control.session.is_some() {
            assert!(
                Instant::now() < deadline,
                "the dictation thread did not report"
            );
            notice = control.poll().or(notice);
            std::thread::sleep(Duration::from_millis(5));
        }
        let notice = notice.unwrap();
        assert!(
            !notice.starts_with("Typed"),
            "a failed dictation reports its failure, not its text: {notice}"
        );
        assert!(
            !notice.starts_with("recorder thread did not start"),
            "the thread reports a real outcome, not a lost one: {notice}"
        );
        assert!(matches!(control.dictation, Dictation::Done(_)));
        control.dismiss();
        assert!(matches!(control.dictation, Dictation::Idle));
    }
}
