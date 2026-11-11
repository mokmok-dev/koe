//! The menu bar icon, its menu, and the actions it reports.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
#[cfg(not(target_os = "macos"))]
use tray_icon::{MouseButton, MouseButtonState};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::icon;
use crate::repaint::Repaint;

/// What the icon and menu show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayStatus {
    /// No take is running.
    Ready,
    /// A take is opening its sources.
    Starting,
    /// At least one source is capturing.
    Recording,
    /// The file is closing.
    Saving,
    /// A take finished cleanly.
    Saved,
    /// A take ended with an error.
    Failed,
    /// A dictation is capturing or transcribing.
    Dictating,
}

/// The value the app hands the tray each frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayState {
    /// What the take is doing.
    pub status: TrayStatus,
    /// Whole seconds to show beside Recording.
    pub elapsed: Option<Duration>,
    /// A short warning or error for the tooltip.
    pub detail: Option<String>,
    /// Whether Start applies.
    pub can_start: bool,
    /// Whether Stop applies.
    pub can_stop: bool,
    /// Whether Cancel dictation applies.
    pub can_cancel: bool,
}

/// A command the user chose in the menu or with a click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    /// Begin a take.
    Start,
    /// Stop the current take.
    Stop,
    /// Discard the running dictation without typing it.
    CancelDictation,
    /// Show the main window.
    Show,
    /// Show the settings screen.
    ShowSettings,
    /// Open the recordings folder.
    OpenFolder,
    /// Exit the app.
    Quit,
}

/// Owns the tray icon and its menu items.
pub struct Tray {
    icon: TrayIcon,
    header: MenuItem,
    start: MenuItem,
    stop: MenuItem,
    cancel: MenuItem,
    last: TrayState,
    actions: Arc<Mutex<VecDeque<TrayAction>>>,
}

impl Tray {
    /// Creates the icon and menu, and routes menu events to the UI repaint callback.
    ///
    /// # Errors
    ///
    /// Returns the platform error when no tray host accepts the icon, which
    /// happens on a desktop without a `StatusNotifier` watcher.
    pub fn new(repaint: Repaint, state: &TrayState) -> Result<Self, String> {
        let actions = Arc::new(Mutex::new(VecDeque::new()));
        let menu_actions = Arc::clone(&actions);
        let menu_repaint = repaint.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            debug(&format!("menu {:?}", event.id.0));
            if let Some(action) = action_for(&event.id)
                && let Ok(mut queue) = menu_actions.lock()
            {
                queue.push_back(action);
            }
            menu_repaint();
        }));
        TrayIconEvent::set_event_handler(Some(move |_event| repaint()));

        let header = MenuItem::with_id("status", label(state), false, None);
        let start = MenuItem::with_id("start", "Start recording", state.can_start, None);
        let stop = MenuItem::with_id("stop", "Stop recording", state.can_stop, None);
        let cancel = MenuItem::with_id("cancel", "Cancel dictation", state.can_cancel, None);
        let show = MenuItem::with_id("show", "Open koe", true, None);
        let settings = MenuItem::with_id("settings", "Settings…", true, None);
        let folder = MenuItem::with_id("folder", "Open recordings folder", true, None);
        let quit = MenuItem::with_id("quit", "Quit koe", true, None);
        let first_separator = PredefinedMenuItem::separator();
        let second_separator = PredefinedMenuItem::separator();
        let third_separator = PredefinedMenuItem::separator();
        let menu = Menu::new();
        for item in [
            &header as &dyn tray_icon::menu::IsMenuItem,
            &first_separator,
            &start,
            &stop,
            &cancel,
            &second_separator,
            &show,
            &settings,
            &folder,
            &third_separator,
            &quit,
        ] {
            menu.append(item).map_err(|error| error.to_string())?;
        }

        let icon = TrayIconBuilder::new()
            .with_tooltip(tooltip(state))
            .with_icon(icon::status_icon(state.status)?)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(true)
            .build()
            .map_err(|error| error.to_string())?;
        debug("installed");

        Ok(Self {
            icon,
            header,
            start,
            stop,
            cancel,
            last: state.clone(),
            actions,
        })
    }

    /// Applies `state` when it differs from the last value applied.
    ///
    /// # Errors
    ///
    /// Returns the platform error when the icon or tooltip cannot be updated.
    pub fn sync(&mut self, state: &TrayState) -> Result<(), String> {
        if state == &self.last {
            return Ok(());
        }
        self.header.set_text(label(state));
        self.start.set_enabled(state.can_start);
        self.stop.set_enabled(state.can_stop);
        self.cancel.set_enabled(state.can_cancel);
        self.icon
            .set_tooltip(Some(tooltip(state)))
            .map_err(|error| error.to_string())?;
        self.icon
            .set_icon(Some(icon::status_icon(state.status)?))
            .map_err(|error| error.to_string())?;
        self.last = state.clone();
        Ok(())
    }

    /// Takes every menu and icon event that arrived.
    #[must_use]
    pub fn poll(&self) -> Vec<TrayAction> {
        let mut actions = Vec::new();
        if let Ok(mut queue) = self.actions.lock() {
            for action in queue.drain(..) {
                push_unique(&mut actions, action);
            }
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if let Some(action) = action_for(&event.id) {
                push_unique(&mut actions, action);
            }
        }
        #[cfg(not(target_os = "macos"))]
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
                | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => push_unique(&mut actions, TrayAction::Show),
                _ => {}
            }
        }
        #[cfg(target_os = "macos")]
        while TrayIconEvent::receiver().try_recv().is_ok() {}
        actions
    }
}

fn push_unique(actions: &mut Vec<TrayAction>, action: TrayAction) {
    if !actions.contains(&action) {
        actions.push(action);
    }
}

fn debug(message: &str) {
    if std::env::var_os("KOE_TRAY_DEBUG").is_some() {
        eprintln!("koe tray: {message}");
    }
}

/// Text the menu header shows.
#[must_use]
pub fn label(state: &TrayState) -> String {
    match state.status {
        TrayStatus::Ready => "Ready".to_owned(),
        TrayStatus::Starting => "Starting...".to_owned(),
        TrayStatus::Recording => state.elapsed.map_or_else(
            || "Recording".to_owned(),
            |elapsed| format!("Recording {}", elapsed_text(elapsed)),
        ),
        TrayStatus::Saving => "Saving...".to_owned(),
        TrayStatus::Saved => "Saved".to_owned(),
        TrayStatus::Failed => "Recording failed".to_owned(),
        TrayStatus::Dictating => state.elapsed.map_or_else(
            || "Dictating".to_owned(),
            |elapsed| format!("Dictating {}", elapsed_text(elapsed)),
        ),
    }
}

/// Text the pointer shows over the icon.
#[must_use]
pub fn tooltip(state: &TrayState) -> String {
    state.detail.as_ref().map_or_else(
        || format!("koe · {}", label(state)),
        |detail| format!("koe · {} · {detail}", label(state)),
    )
}

/// Maps a menu id to the action it names.
#[must_use]
pub fn action_for(id: &MenuId) -> Option<TrayAction> {
    match id.0.as_str() {
        "start" => Some(TrayAction::Start),
        "stop" => Some(TrayAction::Stop),
        "cancel" => Some(TrayAction::CancelDictation),
        "show" => Some(TrayAction::Show),
        "settings" => Some(TrayAction::ShowSettings),
        "folder" => Some(TrayAction::OpenFolder),
        "quit" => Some(TrayAction::Quit),
        _ => None,
    }
}

fn elapsed_text(elapsed: Duration) -> String {
    let total = elapsed.as_secs();
    format!("{:02}:{:02}", total / 60, total % 60)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn state(status: TrayStatus) -> TrayState {
        TrayState {
            status,
            elapsed: None,
            detail: None,
            can_start: true,
            can_stop: false,
            can_cancel: false,
        }
    }

    #[test]
    fn every_phase_has_a_label() {
        assert_eq!(label(&state(TrayStatus::Ready)), "Ready");
        assert_eq!(label(&state(TrayStatus::Starting)), "Starting...");
        assert_eq!(label(&state(TrayStatus::Recording)), "Recording");
        assert_eq!(label(&state(TrayStatus::Saving)), "Saving...");
        assert_eq!(label(&state(TrayStatus::Saved)), "Saved");
        assert_eq!(label(&state(TrayStatus::Failed)), "Recording failed");
        assert_eq!(label(&state(TrayStatus::Dictating)), "Dictating");
    }

    #[test]
    fn dictating_shows_whole_seconds() {
        let mut state = state(TrayStatus::Dictating);
        state.elapsed = Some(Duration::from_secs(30));
        assert_eq!(label(&state), "Dictating 00:30");
        assert_eq!(tooltip(&state), "koe · Dictating 00:30");
    }

    #[test]
    fn recording_shows_whole_seconds() {
        let mut state = state(TrayStatus::Recording);
        state.elapsed = Some(Duration::from_secs(65));
        assert_eq!(label(&state), "Recording 01:05");
        assert_eq!(tooltip(&state), "koe · Recording 01:05");
    }

    #[test]
    fn a_detail_joins_the_tooltip() {
        let mut state = state(TrayStatus::Failed);
        state.detail = Some("no microphone".to_owned());
        assert_eq!(tooltip(&state), "koe · Recording failed · no microphone");
    }

    #[test]
    fn every_menu_id_names_one_action() {
        for (id, expected) in [
            ("start", TrayAction::Start),
            ("stop", TrayAction::Stop),
            ("cancel", TrayAction::CancelDictation),
            ("show", TrayAction::Show),
            ("settings", TrayAction::ShowSettings),
            ("folder", TrayAction::OpenFolder),
            ("quit", TrayAction::Quit),
        ] {
            assert_eq!(action_for(&MenuId::new(id)), Some(expected));
        }
        assert_eq!(action_for(&MenuId::new("status")), None);
        assert_eq!(action_for(&MenuId::new("unknown")), None);
    }
}
