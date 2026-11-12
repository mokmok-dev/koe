//! The GPUI shell: window, overlay HUD, and the menu bar item.
//!
//! GPUI owns the application loop and the tray is `tray-icon`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use gpui::ActivationPolicy;
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Context, Entity, QuitMode, WindowBounds,
    WindowDecorations, WindowId, WindowOptions, px, size,
};
use gpui_kit::component::{Theme, ThemeMode, TitleBar};
use koe_session_store::{Error as StoreError, Transcripts};

use crate::dictation;
use crate::dictation_control::{DictationControl, Press};
use crate::dictation_worker;
use crate::engine::{self, Completion, EngineEvent, TranscriptionPlan};
use crate::hud::{self, HudState};
use crate::overlay;
use crate::settings::{self, Settings, Take};
use crate::state::{Action, Event, Phase, Sources};
use crate::transcript::{self, MeetingTranscript, Speaker};
use crate::tray::{Tray, TrayAction, TrayState, TrayStatus};
use crate::ui::MainView;

/// How long a refused hotkey press stays in the HUD.
///
/// The window notice and the tray tooltip are the lasting copies; the HUD
/// only needs to hold the sentence long enough to read it.
const REFUSAL_HOLD: Duration = Duration::from_secs(5);

/// How often the shell looks at a running dictation.
///
/// The dictation thread publishes a new transcript once per 20 ms audio block,
/// and the speech models report tentative phrases between blocks. A tick
/// slower than that samples the transcript instead of following it, which
/// reads as words arriving a beat late.
const DICTATION_TICK: Duration = Duration::from_millis(20);

/// How often the shell looks at a running take or a dictation whose transcript
/// nothing draws.
///
/// The take HUD's timer and badge move once a second, and `KOE_HUD=off` leaves
/// the transcript with no reader, so neither needs the dictation tick.
const ACTIVE_TICK: Duration = Duration::from_millis(100);

/// Which of the window's three screens is showing.
///
/// The settings page selected inside gpui-kit's `Settings` is not part of this:
/// GPUI drops an element's keyed state once that element stops rendering, so
/// the settings screen opens on its first page after a visit to another screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Screen {
    /// The take controls and the transcript of both sources as a message thread.
    #[default]
    Meeting,
    /// The take status, the start and stop control, and the last take.
    Recorder,
    /// The dictation settings, the sources, the format, and the folder.
    Settings,
}

/// The shell's state, shared by the window, the tray, and the tick loop.
#[allow(
    clippy::struct_excessive_bools,
    reason = "the shell holds independent pending window requests and view flags; each flag is its own state machine"
)]
pub struct Shell {
    pub(crate) phase: Phase,
    pub(crate) settings: Settings,
    pub(crate) screen: Screen,
    pub(crate) sidebar_collapsed: bool,
    pub(crate) session: Option<engine::Session>,
    pub(crate) take: Option<Take>,
    pub(crate) transcript: MeetingTranscript,
    pub(crate) transcript_pending: bool,
    pub(crate) levels: [f32; 2],
    pub(crate) tray: Option<Tray>,
    pub(crate) tray_error: Option<String>,
    pub(crate) notice: Option<String>,
    pub(crate) dictation_control: DictationControl,
    last_tray: Option<TrayState>,
    quitting: bool,
    quit_deadline: Option<Instant>,
    window: Option<AnyWindowHandle>,
    hud: Option<AnyWindowHandle>,
    hud_view: Option<Entity<overlay::HudView>>,
    last_hud: Option<HudState>,
    hud_enabled: bool,
    /// A refused hotkey press shown in the HUD, and when it stops showing.
    refusal: Option<(String, Instant)>,
    pending_show: bool,
    pending_quit: bool,
    pending_hud_close: Option<AnyWindowHandle>,
}

/// What the tick loop must run after the shell lease is released.
pub struct Tick {
    /// Time until the next tick.
    pub delay: Duration,
    /// Whether to open or focus the main window.
    pub show: bool,
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
        let (dictation_control, warning) = DictationControl::load();
        Self {
            phase: Phase::Idle,
            settings: Settings::default(),
            screen: Screen::Meeting,
            sidebar_collapsed: false,
            session: None,
            take: None,
            transcript: MeetingTranscript::default(),
            transcript_pending: false,
            levels: [0.0; 2],
            tray: None,
            tray_error: None,
            notice: warning,
            dictation_control,
            last_tray: None,
            quitting: false,
            quit_deadline: None,
            window: None,
            hud: None,
            hud_view: None,
            last_hud: None,
            hud_enabled: hud_enabled(),
            refusal: None,
            pending_show: false,
            pending_quit: false,
            pending_hud_close: None,
        }
    }

    /// Runs one tick and reports the window work to run after this lease ends.
    pub fn tick(&mut self, cx: &mut Context<Self>) -> Tick {
        let mut changed = false;
        changed |= self.ensure_tray();
        changed |= self.ensure_dictation();
        let actions = self.tray.as_ref().map(Tray::poll).unwrap_or_default();
        for action in actions {
            if std::env::var_os("KOE_TRAY_DEBUG").is_some() {
                eprintln!("koe app: tray {action:?}");
            }
            self.handle_tray(action, cx);
            changed = true;
        }
        let presses = self.dictation_control.presses();
        for _ in 0..presses {
            // A press makes the previous refusal stale, whether or not this
            // one is also refused.
            self.refusal = None;
            match self.dictation_control.toggle(self.phase.is_active()) {
                Some(Press::Refused(message)) => {
                    self.refusal = Some((message.clone(), Instant::now() + REFUSAL_HOLD));
                    self.notice = Some(message);
                    self.show_window();
                }
                Some(Press::Failed(message)) => {
                    self.notice = Some(message);
                    self.show_window();
                }
                None => {}
            }
        }
        changed |= presses > 0;
        changed |= self.poll_engine();
        if let Some(message) = self.dictation_control.poll() {
            self.notice = Some(message);
            changed = true;
        }
        changed |= self.sync_tray();
        changed |= self.sync_hud(cx);
        if changed {
            cx.notify();
        }

        let mut tick = Tick {
            delay: Duration::from_millis(250),
            show: false,
            quit: false,
            hud_close: self.pending_hud_close.take(),
        };
        if self.quitting {
            let deadline_passed = self
                .quit_deadline
                .is_some_and(|deadline| Instant::now() >= deadline);
            if (self.session.is_none() && self.dictation_control.session.is_none())
                || deadline_passed
            {
                tick.quit = true;
            }
            tick.delay = ACTIVE_TICK;
        } else if self.phase.is_active() {
            tick.delay = ACTIVE_TICK;
        } else if self.dictation_control.dictation.is_active() {
            // Only the HUD reads the transcript; with `KOE_HUD=off` the slower
            // tick keeps the tray and the state machine current for less work.
            tick.delay = if self.hud_enabled {
                DICTATION_TICK
            } else {
                ACTIVE_TICK
            };
        } else if self.dictation_control.hotkeys.is_some() {
            // A press waits in the queue until the next tick. A dictation
            // start is where the lost audio is speech, so the idle tick that
            // polls for presses is five times faster than the tray's.
            tick.delay = Duration::from_millis(50);
        }
        tick.show = self.pending_show;
        tick.quit |= self.pending_quit;
        self.pending_show = false;
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

    /// Mounts the dictation side: the binding and its failure banner.
    fn ensure_dictation(&mut self) -> bool {
        let message = self.dictation_control.mount();
        if let Some(message) = message {
            self.notice = Some(message);
            self.show_window();
            return true;
        }
        false
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
                EngineEvent::Transcript { source, at, text } => {
                    if self.transcript_pending {
                        if self.transcript.replace(Speaker::of(source), at, &text) {
                            self.transcript_pending = false;
                        }
                    } else {
                        self.transcript.push(Speaker::of(source), at, &text);
                    }
                }
                EngineEvent::Draft { source, at, text } => {
                    self.transcript.draft(Speaker::of(source), at, &text);
                }
                EngineEvent::Opened(source) => self.apply(Event::Opened(source)),
                EngineEvent::Lost { source, message } => {
                    self.notice = Some(message.clone());
                    self.apply(Event::Lost { source, message });
                }
                EngineEvent::Finished(completion) => {
                    self.session = None;
                    self.levels = [0.0; 2];
                    self.save_transcripts(&completion.transcripts);
                    // A recorder that never created its file left nothing to
                    // keep; a partial take keeps its session.
                    if completion.summary.is_none()
                        && !completion.output.is_file()
                        && let Some(message) = self.discard_take()
                    {
                        self.notice = Some(message);
                    }
                    self.apply(Event::Finished(completion));
                    self.take = None;
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
                        // A thread that never started recorded nothing.
                        let removal = self.discard_take();
                        self.notice = Some(removal.map_or_else(
                            || error.to_string(),
                            |message| format!("{error}; {message}"),
                        ));
                        let completion = Box::new(Completion {
                            output,
                            summary: None,
                            failure: Some(error),
                            transcripts: Transcripts::new(),
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
            Action::Delete => {
                let removal = self.discard_take();
                self.phase.set_session_removed(removal.is_none());
                if let Some(message) = removal {
                    self.notice = Some(message);
                }
            }
            Action::Pause => {
                if let Some(session) = &self.session {
                    session.pause();
                }
            }
            Action::Resume => {
                if let Some(session) = &self.session {
                    session.resume();
                }
            }
        }
    }

    /// Stores the take's transcripts in its session.
    fn save_transcripts(&mut self, transcripts: &Transcripts) {
        let Some(take) = self.take.as_mut() else {
            return;
        };
        match take.meeting.save_transcripts(transcripts) {
            Ok(()) => {}
            Err(StoreError::TextNotWritten { source }) => {
                self.notice = Some(format!(
                    "the transcript text file was not written: {source}"
                ));
            }
            Err(error) => self.notice = Some(format!("the transcript was not saved: {error}")),
        }
    }

    /// Removes the whole session of a take that recorded nothing.
    ///
    /// Returns the message to show when the removal failed; the caller decides
    /// whether it stands alone or joins another error.
    fn discard_take(&mut self) -> Option<String> {
        let take = self.take.take()?;
        match take.store.discard(take.meeting.id()) {
            Ok(()) => None,
            Err(error) => Some(format!(
                "the session at {} was not removed: {error}",
                take.meeting.directory().display()
            )),
        }
    }

    /// Starts a take from the window or the tray.
    pub fn start_recording(&mut self, cx: &mut Context<Self>) {
        if !self.phase.can_start() {
            return;
        }
        if let Err(message) = dictation::meeting_start_allowed(&self.dictation_control.dictation) {
            self.notice = Some(message.to_owned());
            self.show_window();
            cx.notify();
            return;
        }
        self.notice = None;
        let (take, mut plan) = match self.settings.plan() {
            Ok(planned) => planned,
            Err(error) => {
                self.notice = Some(error.to_string());
                self.show_window();
                return;
            }
        };
        let (model, language) = self.dictation_control.transcription();
        if let Some(model) = model {
            plan.transcription = Some(TranscriptionPlan { model, language });
        } else {
            self.notice = Some(transcript::MODEL_MISSING.to_owned());
        }
        // `Settings::plan` refuses an empty selection, so a start always arms.
        if let Some(action) = self.phase.reduce(Event::Start(plan), Instant::now()) {
            self.take = Some(take);
            // Until this take retains an utterance, the rows on screen belong
            // to an earlier take.
            self.transcript_pending = true;
            self.transcript.clear_drafts();
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

    /// Pauses or resumes the current take from the HUD's button.
    pub fn toggle_recording_pause(&mut self, cx: &mut Context<Self>) {
        let event = if self.phase.is_paused() {
            Event::Resume
        } else {
            Event::Pause
        };
        let action = self.phase.reduce(event, Instant::now());
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

    /// Clears a finished dictation's result.
    pub fn dismiss_dictation(&mut self, cx: &mut Context<Self>) {
        self.notice = None;
        self.dictation_control.dismiss();
        cx.notify();
    }

    /// Switches to the settings screen.
    pub fn show_settings(&mut self, cx: &mut Context<Self>) {
        self.screen = Screen::Settings;
        cx.notify();
    }

    /// Switches to the meeting screen.
    pub fn show_meeting(&mut self, cx: &mut Context<Self>) {
        self.screen = Screen::Meeting;
        cx.notify();
    }

    /// Switches to the recorder screen.
    pub fn show_recorder(&mut self, cx: &mut Context<Self>) {
        self.screen = Screen::Recorder;
        cx.notify();
    }

    /// Collapses the sidebar to its icon strip, or expands it again.
    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        cx.notify();
    }

    /// Commits the hotkey field's draft on Enter.
    ///
    /// The binding is committed here rather than while the user types,
    /// because every valid intermediate prefix of a binding is a real
    /// shortcut: typing toward `Ctrl+Shift+Space` passes through `Ctrl+S`,
    /// which a live rebind would grab system-wide.
    pub fn apply_hotkey(&mut self, draft: &str, cx: &mut Context<Self>) {
        self.notice = self.dictation_control.apply_hotkey(draft);
        cx.notify();
    }

    /// Persists the model and language settings.
    pub fn apply_dictation_settings(
        &mut self,
        model: Option<PathBuf>,
        language: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = self.dictation_control.apply_settings(model, language) {
            self.notice = Some(error.to_string());
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

    /// Stops the take and the dictation, waits up to two seconds for their
    /// work, then quits.
    pub fn quit(&mut self, cx: &mut Context<Self>) {
        self.quitting = true;
        // A dictation stops and commits like a take: both get the same
        // bounded wait for their text or their file.
        self.dictation_control.stop();
        if let Some(session) = &self.session {
            session.stop();
        }
        if self.session.is_some() || self.dictation_control.session.is_some() {
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
            TrayAction::CancelDictation => {
                if self.dictation_control.cancel() {
                    self.notice = None;
                }
            }
            // The menu item opens the window on its settings screen.
            TrayAction::Show => self.show_window(),
            TrayAction::ShowSettings => {
                self.screen = Screen::Settings;
                self.show_window();
            }
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
        let now = Instant::now();
        if self
            .refusal
            .as_ref()
            .is_some_and(|(message, until)| hud::held_refusal(message, *until, now).is_none())
        {
            self.refusal = None;
        }
        if !self.hud_enabled {
            return false;
        }
        let Some(next) = self.hud_state(now) else {
            if let Some(handle) = self.hud.take() {
                self.pending_hud_close = Some(handle);
            }
            self.hud_view = None;
            return self.last_hud.take().is_some();
        };
        if self.last_hud.as_ref() == Some(&next) {
            return false;
        }
        // The take card and the transcript card differ in size and position,
        // and the window is placed when it opens, so a category change swaps
        // the window instead of moving it.
        if self
            .last_hud
            .as_ref()
            .is_some_and(|open| open.is_take() != next.is_take())
        {
            self.pending_hud_close = self.hud.take();
            self.hud_view = None;
        }
        if self.hud.is_none() {
            let shell = cx.weak_entity();
            let Some((handle, view)) = overlay::open(cx, next.clone(), shell) else {
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
            if let Some(handle) = self.hud {
                overlay::set_interactive(cx, handle, next.pause_label().is_some());
            }
        }
        self.last_hud = Some(next);
        true
    }

    fn tray_state(&self) -> TrayState {
        let active = self.dictation_control.dictation.is_active();
        let status = if active {
            TrayStatus::Dictating
        } else {
            match &self.phase {
                Phase::Idle => TrayStatus::Ready,
                Phase::Arming { .. } => TrayStatus::Starting,
                Phase::Recording(_) => TrayStatus::Recording,
                Phase::Paused(_) => TrayStatus::Paused,
                Phase::Stopping { .. } => TrayStatus::Saving,
                Phase::Done(done) if done.failed() => TrayStatus::Failed,
                Phase::Done(_) => TrayStatus::Saved,
            }
        };
        let elapsed = if active {
            self.dictation_control.dictation.elapsed(Instant::now())
        } else {
            self.phase.elapsed(Instant::now())
        };
        TrayState {
            status,
            elapsed: elapsed.map(|elapsed| Duration::from_secs(elapsed.as_secs())),
            detail: self
                .notice
                .clone()
                .or_else(|| self.phase.warning().map(str::to_owned)),
            can_start: self.phase.can_start() && !self.settings.sources.is_empty() && !active,
            can_stop: self.phase.can_stop(),
            can_cancel: active,
        }
    }

    fn hud_state(&self, now: Instant) -> Option<HudState> {
        let transcript = self
            .dictation_control
            .session
            .as_ref()
            .map_or_else(String::new, dictation_worker::Session::transcript);
        let refusal = self
            .refusal
            .as_ref()
            .and_then(|(message, until)| hud::held_refusal(message, *until, now));
        HudState::for_frame(
            &self.dictation_control.dictation,
            &transcript,
            &self.phase,
            refusal,
            now,
        )
    }
}

fn hud_enabled() -> bool {
    std::env::var_os("KOE_HUD").is_none_or(|value| value != "off")
}

fn main_window_options(cx: &App) -> WindowOptions {
    let mut options = TitleBar::window_options();
    // The Settings screen stacks two sidebars: 200px app sidebar, a 180px
    // settings page list, and 44px of content padding. The default keeps the
    // page panel above the 480px where setting rows sit label beside field;
    // the minimum leaves that panel 366px, where rows stack instead.
    options.window_bounds = Some(WindowBounds::centered(size(px(920.0), px(680.0)), cx));
    options.window_min_size = Some(size(px(760.0), px(560.0)));
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
    match gpui_kit::open_window(options, cx, move |window, cx| {
        cx.new(move |cx| MainView::new(view, window, cx))
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::time::{Duration, Instant};

    use koe_record::Summary;
    use koe_session_store::{Store, Transcripts};

    use super::*;
    use crate::state::Live;

    /// A shell holding one take in a fresh store under `root`, and the plan the
    /// engine would have run.
    fn shell_with_take(root: &std::path::Path) -> (Shell, engine::SessionPlan) {
        let mut shell = Shell::new();
        shell.settings.directory = root.to_path_buf();
        let (take, plan) = shell.settings.plan().unwrap();
        shell.take = Some(take);
        shell.phase = Phase::Recording(Live::new(Instant::now()));
        (shell, plan)
    }

    fn empty_take(plan: &engine::SessionPlan) -> Box<Completion> {
        Box::new(Completion {
            output: plan.output.clone(),
            summary: Some(Summary {
                frames: 0,
                clipped_samples: 0,
                duration: Duration::from_secs(1),
            }),
            failure: None,
            transcripts: Transcripts::new(),
        })
    }

    #[test]
    fn an_empty_take_records_that_its_session_was_removed() {
        let root = tempfile::tempdir().unwrap();
        let (mut shell, plan) = shell_with_take(root.path());

        shell.apply(Event::Finished(empty_take(&plan)));

        assert!(matches!(&shell.phase, Phase::Done(done) if done.session_removed));
        assert!(shell.take.is_none());
        assert_eq!(Store::open(root.path()).unwrap().list().unwrap(), []);
    }

    #[cfg(unix)]
    #[test]
    fn an_empty_take_that_cannot_be_removed_names_its_session() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let (mut shell, plan) = shell_with_take(root.path());
        let directory = shell
            .take
            .as_ref()
            .unwrap()
            .meeting
            .directory()
            .to_path_buf();

        let original = fs::metadata(root.path()).unwrap().permissions().mode();
        let mut read_only = fs::metadata(root.path()).unwrap().permissions();
        read_only.set_mode(0o555);
        fs::set_permissions(root.path(), read_only).unwrap();

        shell.apply(Event::Finished(empty_take(&plan)));

        let mut restored = fs::metadata(root.path()).unwrap().permissions();
        restored.set_mode(original);
        fs::set_permissions(root.path(), restored).unwrap();

        assert!(matches!(&shell.phase, Phase::Done(done) if !done.session_removed));
        let notice = shell.notice.as_deref().unwrap();
        assert!(notice.contains(&directory.display().to_string()));
        assert!(directory.exists());
    }
}
