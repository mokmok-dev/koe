//! The Rune shell: window, overlay HUD, and the system tray.
//!
//! Rune owns one winit event loop. The tray is `tray-icon`, not Rune's own
//! tray, so macOS gets a status item while the app keeps a single UI stack.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use rune::{
    Align, App, AppHandle, AppSpec, OverlayHandle, OverlaySpec, RunOptions, ScreenEdge, Tx, Waker,
    WindowSpec,
};

use crate::engine::{self, Completion, EngineEvent};
use crate::repaint::repaint;
use crate::rune_hud::{HudPhase, HudState};
use crate::settings::{self, Settings};
use crate::state::{Action, Event, Phase, Sources};
use crate::tray::{Tray, TrayAction, TrayState, TrayStatus};

thread_local! {
    static APP: RefCell<Option<AppHandle<AppState>>> = const { RefCell::new(None) };
    static HUD: RefCell<Option<OverlayHandle<HudState>>> = const { RefCell::new(None) };
}

/// A command the window or tray asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiIntent {
    /// Begin a take.
    Start,
    /// Stop the current take.
    Stop,
    /// Hide the window and keep the tray icon.
    Hide,
    /// Open the recordings folder.
    OpenFolder,
    /// Exit the app.
    Quit,
    /// Clear the finished take.
    Dismiss,
}

/// The shell's state.
pub struct AppState {
    pub(crate) phase: Phase,
    pub(crate) settings: Settings,
    pub(crate) session: Option<engine::Session>,
    pub(crate) levels: [f32; 2],
    pub(crate) tray: Option<Tray>,
    pub(crate) tray_error: Option<String>,
    pub(crate) notice: Option<String>,
    pub(crate) pending: Option<UiIntent>,
    waker: Option<Waker>,
    last_tray: Option<TrayState>,
    last_hud: Option<HudState>,
    hud_visible: bool,
    hud_enabled: bool,
    quitting: bool,
    quit_deadline: Option<Instant>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            settings: Settings::default(),
            session: None,
            levels: [0.0; 2],
            tray: None,
            tray_error: None,
            notice: None,
            pending: None,
            waker: None,
            last_tray: None,
            last_hud: None,
            hud_visible: false,
            hud_enabled: std::env::var_os("KOE_HUD").is_none_or(|value| value != "off"),
            quitting: false,
            quit_deadline: None,
        }
    }
}

impl AppState {
    fn tray_state(&self) -> TrayState {
        let status = match &self.phase {
            Phase::Idle => TrayStatus::Ready,
            Phase::Arming { .. } => TrayStatus::Starting,
            Phase::Recording(_) => TrayStatus::Recording,
            Phase::Stopping { .. } => TrayStatus::Saving,
            Phase::Done(done) if done.failed() => TrayStatus::Failed,
            Phase::Done(_) => TrayStatus::Saved,
        };
        TrayState {
            status,
            elapsed: self
                .phase
                .elapsed(Instant::now())
                .map(|elapsed| Duration::from_secs(elapsed.as_secs())),
            detail: self
                .notice
                .clone()
                .or_else(|| self.phase.warning().map(str::to_owned)),
            can_start: self.phase.can_start() && !self.settings.sources.is_empty(),
            can_stop: self.phase.can_stop(),
        }
    }

    fn hud_state(&self) -> Option<HudState> {
        let phase = match &self.phase {
            Phase::Arming { .. } => HudPhase::Starting,
            Phase::Recording(_) => HudPhase::Recording,
            Phase::Stopping { .. } => HudPhase::Saving,
            Phase::Idle | Phase::Done(_) => return None,
        };
        let live = self.phase.live();
        Some(HudState {
            phase,
            elapsed: self.phase.elapsed(Instant::now()).unwrap_or_default(),
            live: [
                live.contains(koe_capture::Source::Microphone),
                live.contains(koe_capture::Source::SystemAudio),
            ],
            levels: self.levels,
            warning: self.phase.warning().map(str::to_owned),
        })
    }
}

/// Runs the Rune shell.
pub fn run() {
    rune::run_windows_with_args(RunOptions { accessory: true }, [], |_| {
        let spec = AppSpec {
            setup: Some(|state: &mut AppState, waker| state.waker = Some(waker)),
            poll: Some(poll),
            min_size: Some((420.0, 560.0)),
            ..AppSpec::new(
                "koe",
                (440.0, 640.0),
                AppState::default,
                crate::rune_ui::view,
            )
        };
        let mut app = App::new(spec).keep_running();
        app = app.on_close(|state, tx| {
            if state.tray.is_none() {
                tx.quit();
            }
        });
        APP.with(|slot| *slot.borrow_mut() = Some(app.handle()));
        let hud = app.overlay(
            OverlaySpec {
                name: "koe-hud",
                size: (340.0, 88.0),
                edge: ScreenEdge::Bottom,
                margin: 24.0,
                align: Align::Center,
                inset: 0.0,
                interactive: false,
                view: crate::rune_hud::view,
            },
            HudState::default(),
        );
        HUD.with(|slot| *slot.borrow_mut() = Some(hud));
        vec![WindowSpec::from_app("main", app)]
    });
}

fn poll(state: &mut AppState, _seconds: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    if state.tray.is_none()
        && state.tray_error.is_none()
        && let Some(waker) = state.waker.clone()
    {
        match Tray::new(waker, &state.tray_state()) {
            Ok(tray) => {
                state.tray = Some(tray);
                state.last_tray = Some(state.tray_state());
                changed = true;
            }
            Err(error) => {
                state.tray_error = Some(error);
                changed = true;
            }
        }
    }
    let actions = state.tray.as_ref().map(Tray::poll).unwrap_or_default();
    for action in actions {
        if std::env::var_os("KOE_TRAY_DEBUG").is_some() {
            eprintln!("koe app: tray {action:?}");
        }
        handle_tray(state, action, tx);
        changed = true;
    }
    changed |= poll_engine(state);
    if let Some(intent) = state.pending.take() {
        handle_intent(state, intent, tx);
        changed = true;
    }
    changed |= sync_tray(state);
    changed |= sync_hud(state);
    if state.quitting {
        let deadline_passed = state
            .quit_deadline
            .is_some_and(|deadline| Instant::now() >= deadline);
        if state.session.is_none() || deadline_passed {
            with_app(rune::AppHandle::quit);
        } else {
            tx.wake_after(Duration::from_millis(100));
        }
    } else if state.phase.is_active() {
        tx.wake_after(Duration::from_millis(100));
        changed = true;
    } else {
        // A menu pick can arrive while the shell sleeps; keep a tick that drains it.
        tx.wake_after(Duration::from_millis(250));
    }
    changed
}

fn handle_tray(state: &mut AppState, action: TrayAction, tx: &mut Tx) {
    match action {
        TrayAction::Start => start_recording(state),
        TrayAction::Stop => stop_recording(state),
        TrayAction::Show => show_window(),
        TrayAction::OpenFolder => open_folder(state),
        TrayAction::Quit => quit(state, tx),
    }
}

fn handle_intent(state: &mut AppState, intent: UiIntent, tx: &mut Tx) {
    match intent {
        UiIntent::Start => start_recording(state),
        UiIntent::Stop => stop_recording(state),
        UiIntent::Hide => hide_window(),
        UiIntent::OpenFolder => open_folder(state),
        UiIntent::Quit => quit(state, tx),
        UiIntent::Dismiss => {
            let action = state.phase.reduce(Event::Dismiss, Instant::now());
            if let Some(action) = action {
                run_action(state, action);
            }
        }
    }
}

fn start_recording(state: &mut AppState) {
    if !state.phase.can_start() {
        return;
    }
    state.notice = None;
    let plan = match state.settings.plan() {
        Ok(plan) => plan,
        Err(error) => {
            state.notice = Some(error.to_string());
            show_window();
            return;
        }
    };
    let action = state.phase.reduce(Event::Start(plan), Instant::now());
    if let Some(action) = action {
        run_action(state, action);
    }
}

fn stop_recording(state: &mut AppState) {
    let action = state.phase.reduce(Event::Stop, Instant::now());
    if let Some(action) = action {
        run_action(state, action);
    }
}

fn run_action(state: &mut AppState, action: Action) {
    match action {
        Action::Start(plan) => {
            let output = plan.output.clone();
            let repaint = state.waker.clone().unwrap_or_else(|| repaint(|| {}));
            match engine::Session::start(plan, repaint) {
                Ok(session) => {
                    state.session = Some(session);
                    state.levels = [0.0; 2];
                }
                Err(error) => {
                    state.session = None;
                    state.notice = Some(error.to_string());
                    let completion = Box::new(Completion {
                        output,
                        summary: None,
                        failure: Some(error),
                    });
                    let action = state
                        .phase
                        .reduce(Event::Finished(completion), Instant::now());
                    if let Some(action) = action {
                        run_action(state, action);
                    }
                    show_window();
                }
            }
        }
        Action::Stop => {
            if let Some(session) = &state.session {
                session.stop();
            }
        }
        Action::Delete(path) => {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn poll_engine(state: &mut AppState) -> bool {
    let mut events = Vec::new();
    if let Some(session) = &state.session {
        while let Some(event) = session.poll() {
            events.push(event);
        }
    }
    let changed = !events.is_empty();
    for event in events {
        match event {
            EngineEvent::Level { source, level } => {
                let slot = &mut state.levels[Sources::index(source)];
                *slot = if level > *slot { level } else { *slot * 0.82 };
            }
            EngineEvent::Warning(message) => state.notice = Some(message),
            EngineEvent::Opened(source) => apply(state, Event::Opened(source)),
            EngineEvent::Lost { source, message } => {
                apply(state, Event::Lost { source, message });
            }
            EngineEvent::Finished(completion) => {
                state.session = None;
                state.levels = [0.0; 2];
                apply(state, Event::Finished(completion));
                if matches!(&state.phase, Phase::Done(done) if done.failed()) {
                    show_window();
                }
            }
        }
    }
    changed
}

fn apply(state: &mut AppState, event: Event) {
    let action = state.phase.reduce(event, Instant::now());
    if let Some(action) = action {
        run_action(state, action);
    }
}

fn sync_tray(state: &mut AppState) -> bool {
    let next = state.tray_state();
    if state.last_tray.as_ref() == Some(&next) {
        return false;
    }
    let result = state.tray.as_mut().map(|tray| tray.sync(&next));
    match result {
        Some(Ok(())) => {
            state.last_tray = Some(next);
            false
        }
        Some(Err(error)) => {
            state.tray = None;
            state.tray_error = Some(error);
            state.notice = Some("The menu bar icon stopped working.".to_owned());
            show_window();
            true
        }
        None => false,
    }
}

fn sync_hud(state: &mut AppState) -> bool {
    if !state.hud_enabled {
        return false;
    }
    if !state.phase.is_active() {
        if state.hud_visible {
            with_hud(rune::OverlayHandle::hide);
            state.hud_visible = false;
        }
        state.last_hud = None;
        return false;
    }
    let Some(next) = state.hud_state() else {
        return false;
    };
    if state.last_hud.as_ref() == Some(&next) {
        return false;
    }
    if !state.hud_visible {
        with_hud(rune::OverlayHandle::show);
        state.hud_visible = true;
    }
    let update = next.clone();
    with_hud(|hud| hud.update(move |hud, _tx| *hud = update));
    state.last_hud = Some(next);
    true
}

fn open_folder(state: &mut AppState) {
    if let Err(error) = settings::open_directory(&state.settings.directory) {
        state.notice = Some(error.to_string());
        show_window();
    }
}

fn quit(state: &mut AppState, tx: &mut Tx) {
    state.quitting = true;
    if let Some(session) = &state.session {
        session.stop();
        state.quit_deadline = Some(Instant::now() + Duration::from_secs(2));
        tx.wake_after(Duration::from_millis(100));
    } else {
        with_app(rune::AppHandle::quit);
    }
}

fn show_window() {
    with_app(|app| {
        app.open();
        app.show();
        app.focus();
    });
}

fn hide_window() {
    with_app(rune::AppHandle::hide);
}

fn with_app(f: impl FnOnce(&AppHandle<AppState>)) {
    APP.with(|slot| {
        if let Some(app) = slot.borrow().as_ref() {
            f(app);
        }
    });
}

fn with_hud(f: impl FnOnce(&OverlayHandle<HudState>)) {
    HUD.with(|slot| {
        if let Some(hud) = slot.borrow().as_ref() {
            f(hud);
        }
    });
}
