//! The GPUI shell: window, overlay HUD, and the menu bar item.
//!
//! GPUI owns the application loop and the tray is `tray-icon`.

use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use gpui::ActivationPolicy;
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Context, Entity, QuitMode, WindowBounds,
    WindowDecorations, WindowId, WindowOptions, px, size,
};
use gpui_kit::component::{Theme, ThemeMode, TitleBar};

use crate::engine::{self, Completion, EngineEvent};
use crate::hud::{HudPhase, HudState};
use crate::overlay;
use crate::settings::{self, Settings};
use crate::state::{Action, Event, Phase, Sources};
use crate::tray::{Tray, TrayAction, TrayState, TrayStatus};
use crate::ui::MainView;

/// The shell's state, shared by the window, the tray, and the tick loop.
#[allow(
    clippy::struct_excessive_bools,
    reason = "the window requests are independent flags consumed once per tick"
)]
pub struct Shell {
    pub(crate) phase: Phase,
    pub(crate) settings: Settings,
    pub(crate) session: Option<engine::Session>,
    pub(crate) levels: [f32; 2],
    pub(crate) tray: Option<Tray>,
    pub(crate) tray_error: Option<String>,
    pub(crate) notice: Option<String>,
    last_tray: Option<TrayState>,
    quitting: bool,
    quit_deadline: Option<Instant>,
    window: Option<AnyWindowHandle>,
    hud: Option<AnyWindowHandle>,
    hud_view: Option<Entity<overlay::HudView>>,
    last_hud: Option<HudState>,
    hud_enabled: bool,
    pending_show: bool,
    pending_hide: bool,
    pending_quit: bool,
    pending_hud_close: Option<AnyWindowHandle>,
}

/// What the tick loop must run after the shell lease is released.
pub struct Tick {
    /// Time until the next tick.
    pub delay: Duration,
    /// Whether to open or focus the main window.
    pub show: bool,
    /// Whether to close the main window.
    pub hide: bool,
    /// Whether to exit the application.
    pub quit: bool,
    /// The HUD window to close once the shell lease is released.
    pub hud_close: Option<AnyWindowHandle>,
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

impl Shell {
    /// Builds the shell with the tray unmounted; the first tick mounts it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            phase: Phase::Idle,
            settings: Settings::default(),
            session: None,
            levels: [0.0; 2],
            tray: None,
            tray_error: None,
            notice: None,
            last_tray: None,
            quitting: false,
            quit_deadline: None,
            window: None,
            hud: None,
            hud_view: None,
            last_hud: None,
            hud_enabled: hud_enabled(),
            pending_show: false,
            pending_hide: false,
            pending_quit: false,
            pending_hud_close: None,
        }
    }

    /// Runs one tick and reports the window work to run after this lease ends.
    pub fn tick(&mut self, cx: &mut Context<Self>) -> Tick {
        let mut changed = false;
        changed |= self.ensure_tray();
        let actions = self.tray.as_ref().map(Tray::poll).unwrap_or_default();
        for action in actions {
            if std::env::var_os("KOE_TRAY_DEBUG").is_some() {
                eprintln!("koe app: tray {action:?}");
            }
            self.handle_tray(action, cx);
            changed = true;
        }
        changed |= self.poll_engine();
        changed |= self.sync_tray();
        changed |= self.sync_hud(cx);
        if changed {
            cx.notify();
        }

        let mut tick = Tick {
            delay: Duration::from_millis(250),
            show: false,
            hide: false,
            quit: false,
            hud_close: self.pending_hud_close.take(),
        };
        if self.quitting {
            let deadline_passed = self
                .quit_deadline
                .is_some_and(|deadline| Instant::now() >= deadline);
            if self.session.is_none() || deadline_passed {
                tick.quit = true;
            }
            tick.delay = Duration::from_millis(100);
        } else if self.phase.is_active() {
            tick.delay = Duration::from_millis(100);
        }
        tick.show = self.pending_show;
        tick.hide = self.pending_hide;
        tick.quit |= self.pending_quit;
        self.pending_show = false;
        self.pending_hide = false;
        self.pending_quit = false;
        tick
    }

    /// Clears the window handle when the platform closed a window.
    pub fn window_closed(&mut self, id: WindowId, cx: &mut Context<Self>) {
        if self.window.is_some_and(|handle| handle.window_id() == id) {
            self.window = None;
            if self.tray.is_none() && !self.quitting {
                self.pending_quit = true;
            }
        }
        if self.hud.is_some_and(|handle| handle.window_id() == id) {
            self.hud = None;
            self.hud_view = None;
            self.last_hud = None;
        }
        cx.notify();
    }

    fn ensure_tray(&mut self) -> bool {
        if self.tray.is_some() || self.tray_error.is_some() {
            return false;
        }
        match Tray::new(&self.tray_state()) {
            Ok(tray) => {
                self.tray = Some(tray);
                self.last_tray = Some(self.tray_state());
                true
            }
            Err(error) => {
                self.tray_error = Some(error);
                self.show_window();
                true
            }
        }
    }

    fn poll_engine(&mut self) -> bool {
        let mut events = Vec::new();
        if let Some(session) = &self.session {
            while let Some(event) = session.poll() {
                events.push(event);
            }
        }
        let changed = !events.is_empty();
        for event in events {
            match event {
                EngineEvent::Level { source, level } => {
                    let slot = &mut self.levels[Sources::index(source)];
                    *slot = if level > *slot { level } else { *slot * 0.82 };
                }
                EngineEvent::Warning(message) => self.notice = Some(message),
                EngineEvent::Opened(source) => self.apply(Event::Opened(source)),
                EngineEvent::Lost { source, message } => {
                    self.apply(Event::Lost { source, message });
                }
                EngineEvent::Finished(completion) => {
                    self.session = None;
                    self.levels = [0.0; 2];
                    self.apply(Event::Finished(completion));
                    if !self.quitting && matches!(&self.phase, Phase::Done(done) if done.failed()) {
                        self.show_window();
                    }
                }
            }
        }
        changed
    }

    fn apply(&mut self, event: Event) {
        let action = self.phase.reduce(event, Instant::now());
        if let Some(action) = action {
            self.run(action);
        }
    }

    fn run(&mut self, action: Action) {
        match action {
            Action::Start(plan) => {
                let output = plan.output.clone();
                match engine::Session::start(plan) {
                    Ok(session) => {
                        self.session = Some(session);
                        self.levels = [0.0; 2];
                    }
                    Err(error) => {
                        self.session = None;
                        self.notice = Some(error.to_string());
                        let completion = Box::new(Completion {
                            output,
                            summary: None,
                            failure: Some(error),
                        });
                        let action = self
                            .phase
                            .reduce(Event::Finished(completion), Instant::now());
                        if let Some(action) = action {
                            self.run(action);
                        }
                        self.show_window();
                    }
                }
            }
            Action::Stop => {
                if let Some(session) = &self.session {
                    session.stop();
                }
            }
            Action::Delete(path) => {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// Starts a take from the window or the tray.
    pub fn start_recording(&mut self, cx: &mut Context<Self>) {
        if !self.phase.can_start() {
            return;
        }
        self.notice = None;
        let plan = match self.settings.plan() {
            Ok(plan) => plan,
            Err(error) => {
                self.notice = Some(error.to_string());
                self.show_window();
                return;
            }
        };
        let action = self.phase.reduce(Event::Start(plan), Instant::now());
        if let Some(action) = action {
            self.run(action);
        }
        cx.notify();
    }

    /// Stops the current take from the window or the tray.
    pub fn stop_recording(&mut self, cx: &mut Context<Self>) {
        let action = self.phase.reduce(Event::Stop, Instant::now());
        if let Some(action) = action {
            self.run(action);
        }
        cx.notify();
    }

    /// Clears a finished take.
    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        let action = self.phase.reduce(Event::Dismiss, Instant::now());
        if let Some(action) = action {
            self.run(action);
        }
        cx.notify();
    }

    /// Opens the recordings folder.
    pub fn open_folder(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = settings::open_directory(&self.settings.directory) {
            self.notice = Some(error.to_string());
            self.show_window();
        }
        cx.notify();
    }

    /// Shows the window, or asks the tick loop to open it again.
    pub const fn show_window(&mut self) {
        self.pending_show = true;
    }

    /// Hides the window and keeps the tray icon.
    pub fn hide_window(&mut self, cx: &mut Context<Self>) {
        if self.window.is_some() {
            self.pending_hide = true;
        }
        cx.notify();
    }

    /// Stops the take, waits up to two seconds for the file, then quits.
    pub fn quit(&mut self, cx: &mut Context<Self>) {
        self.quitting = true;
        if let Some(session) = &self.session {
            session.stop();
            self.quit_deadline = Some(Instant::now() + Duration::from_secs(2));
        } else {
            self.pending_quit = true;
        }
        cx.notify();
    }

    fn handle_tray(&mut self, action: TrayAction, cx: &mut Context<Self>) {
        match action {
            TrayAction::Start => self.start_recording(cx),
            TrayAction::Stop => self.stop_recording(cx),
            TrayAction::Show => self.show_window(),
            TrayAction::OpenFolder => self.open_folder(cx),
            TrayAction::Quit => self.quit(cx),
        }
    }

    fn sync_tray(&mut self) -> bool {
        let next = self.tray_state();
        if self.last_tray.as_ref() == Some(&next) {
            return false;
        }
        let result = self.tray.as_mut().map(|tray| tray.sync(&next));
        match result {
            Some(Ok(())) => {
                self.last_tray = Some(next);
                false
            }
            Some(Err(error)) => {
                self.tray = None;
                self.tray_error = Some(error);
                self.notice = Some("The menu bar icon stopped working.".to_owned());
                self.show_window();
                true
            }
            None => false,
        }
    }

    fn sync_hud(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.hud_enabled {
            return false;
        }
        if !self.phase.is_active() {
            if let Some(handle) = self.hud.take() {
                self.pending_hud_close = Some(handle);
            }
            self.hud_view = None;
            return self.last_hud.take().is_some();
        }
        let Some(next) = self.hud_state() else {
            return false;
        };
        if self.last_hud.as_ref() == Some(&next) {
            return false;
        }
        if self.hud.is_none() {
            let Some((handle, view)) = overlay::open(cx, next.clone()) else {
                return false;
            };
            self.hud = Some(handle);
            self.hud_view = Some(view);
        } else if let Some(view) = &self.hud_view {
            let update = next.clone();
            view.update(cx, move |view, cx| {
                view.set_state(update);
                cx.notify();
            });
        }
        self.last_hud = Some(next);
        true
    }

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

fn hud_enabled() -> bool {
    std::env::var_os("KOE_HUD").is_none_or(|value| value != "off")
}

fn main_window_options(cx: &App) -> WindowOptions {
    let mut options = TitleBar::window_options();
    options.window_bounds = Some(WindowBounds::centered(size(px(440.0), px(640.0)), cx));
    options.window_min_size = Some(size(px(420.0), px(560.0)));
    options.window_decorations = Some(WindowDecorations::Client);
    options.app_id = Some("koe".to_owned());
    options
}

/// Opens or focuses the main window. Runs without a shell lease because
/// opening a window draws the view, and that view reads the shell.
fn open_main_window(cx: &mut App, shell: &Entity<Shell>) {
    let existing = shell.read(cx).window;
    if let Some(handle) = existing
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        cx.activate(true);
        return;
    }
    let options = main_window_options(cx);
    let view = shell.clone();
    match gpui_kit::open_window(options, cx, move |_, cx| {
        cx.new(move |cx| MainView::new(view, cx))
    }) {
        Ok((handle, _)) => {
            shell.update(cx, |shell, _| shell.window = Some(handle));
            cx.activate(true);
        }
        Err(error) => {
            shell.update(cx, |shell, _| {
                shell.notice = Some(format!("Could not open the window: {error}"));
            });
        }
    }
}

/// Closes the main window without a shell lease.
fn hide_main_window(cx: &mut App, shell: &Entity<Shell>) {
    let existing = shell.read(cx).window;
    if let Some(handle) = existing {
        let _ = handle.update(cx, |_, window, _| window.remove_window());
        shell.update(cx, |shell, _| shell.window = None);
    }
}

/// Runs the GPUI shell.
pub fn run() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .with_quit_mode(QuitMode::Explicit)
        .run(|cx| {
            gpui_kit::init(cx);
            Theme::change(ThemeMode::Dark, None, cx);
            #[cfg(target_os = "macos")]
            cx.set_activation_policy(ActivationPolicy::Accessory);

            let shell = cx.new(|_| Shell::new());
            cx.on_window_closed({
                let shell = shell.clone();
                move |cx, id| {
                    let shell = shell.clone();
                    // A window can close while its owner is being updated, so
                    // run the bookkeeping after that update finishes.
                    cx.defer(move |cx| {
                        shell.update(cx, |shell, cx| shell.window_closed(id, cx));
                    });
                }
            })
            .detach();

            #[cfg(not(target_os = "macos"))]
            open_main_window(cx, &shell);

            cx.spawn(async move |cx: &mut AsyncApp| {
                loop {
                    let tick = cx.update(|cx| shell.update(cx, Shell::tick));
                    if tick.show {
                        cx.update(|cx| open_main_window(cx, &shell));
                    }
                    if tick.hide {
                        cx.update(|cx| hide_main_window(cx, &shell));
                    }
                    if let Some(handle) = tick.hud_close {
                        cx.update(|cx| overlay::close(cx, handle));
                    }
                    if tick.quit {
                        cx.update(|cx| cx.quit());
                    }
                    cx.background_executor().timer(tick.delay).await;
                }
            })
            .detach();
        });
}
