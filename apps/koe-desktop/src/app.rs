//! The eframe application: tray, recorder events, window, and HUD.

use std::time::{Duration, Instant};

use egui::{Context, ViewportCommand};

use crate::engine::{self, Completion, EngineEvent};
use crate::hud;
use crate::repaint::Repaint;
use crate::settings::{self, Settings};
use crate::state::{Action, Event, Phase, Sources};
use crate::tray::{Tray, TrayAction, TrayState, TrayStatus};
use crate::ui::{self, Intent};

/// The desktop application.
pub struct KoeApp {
    phase: Phase,
    settings: Settings,
    session: Option<engine::Session>,
    levels: [f32; 2],
    tray: Option<Tray>,
    tray_error: Option<String>,
    last_tray: Option<TrayState>,
    window_visible: bool,
    notice: Option<String>,
    quitting: bool,
    quit_deadline: Option<Instant>,
}

impl KoeApp {
    /// Builds the app and mounts the tray icon.
    #[must_use]
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut app = Self {
            phase: Phase::Idle,
            settings: Settings::default(),
            session: None,
            levels: [0.0; 2],
            tray: None,
            tray_error: None,
            last_tray: None,
            window_visible: true,
            notice: None,
            quitting: false,
            quit_deadline: None,
        };
        let state = app.tray_state();
        match Tray::new(repaint(&cc.egui_ctx), &state) {
            Ok(tray) => {
                app.tray = Some(tray);
                app.last_tray = Some(state);
            }
            Err(error) => {
                app.tray_error = Some(error);
            }
        }
        app
    }

    fn poll_tray(&mut self, ctx: &Context) {
        let actions = self.tray.as_ref().map(Tray::poll).unwrap_or_default();
        for action in actions {
            match action {
                TrayAction::Start => self.start_recording(ctx),
                TrayAction::Stop => self.stop_recording(ctx),
                TrayAction::Show => self.show_window(ctx),
                TrayAction::OpenFolder => self.open_folder(ctx),
                TrayAction::Quit => self.quit(ctx),
            }
        }
    }

    fn poll_engine(&mut self, ctx: &Context) {
        let mut events = Vec::new();
        if let Some(session) = &self.session {
            while let Some(event) = session.poll() {
                events.push(event);
            }
        }
        for event in events {
            match event {
                EngineEvent::Level { source, level } => self.set_level(source, level),
                EngineEvent::Warning(message) => self.notice = Some(message),
                EngineEvent::Opened(source) => self.apply(Event::Opened(source), ctx),
                EngineEvent::Lost { source, message } => {
                    self.apply(Event::Lost { source, message }, ctx);
                }
                EngineEvent::Finished(completion) => {
                    self.session = None;
                    self.levels = [0.0; 2];
                    self.apply(Event::Finished(completion), ctx);
                    if !self.quitting && matches!(&self.phase, Phase::Done(done) if done.failed()) {
                        self.show_window(ctx);
                    }
                }
            }
        }
    }

    fn set_level(&mut self, source: koe_capture::Source, level: f32) {
        let slot = &mut self.levels[Sources::index(source)];
        *slot = if level > *slot { level } else { *slot * 0.82 };
    }

    fn apply(&mut self, event: Event, ctx: &Context) {
        let action = self.phase.reduce(event, Instant::now());
        if let Some(action) = action {
            self.run(action, ctx);
        }
    }

    fn run(&mut self, action: Action, ctx: &Context) {
        match action {
            Action::Start(plan) => {
                let output = plan.output.clone();
                match engine::Session::start(plan, repaint(ctx)) {
                    Ok(session) => {
                        self.session = Some(session);
                        self.levels = [0.0; 2];
                    }
                    Err(error) => {
                        self.session = None;
                        let completion = Box::new(Completion {
                            output,
                            summary: None,
                            failure: Some(error),
                        });
                        let action = self
                            .phase
                            .reduce(Event::Finished(completion), Instant::now());
                        if let Some(Action::Delete(path)) = action {
                            let _ = std::fs::remove_file(path);
                        }
                        self.show_window(ctx);
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

    fn start_recording(&mut self, ctx: &Context) {
        if !self.phase.can_start() {
            return;
        }
        self.notice = None;
        let plan = match self.settings.plan() {
            Ok(plan) => plan,
            Err(error) => {
                self.notice = Some(error.to_string());
                self.show_window(ctx);
                return;
            }
        };
        let action = self.phase.reduce(Event::Start(plan), Instant::now());
        if let Some(action) = action {
            self.run(action, ctx);
        }
    }

    fn stop_recording(&mut self, ctx: &Context) {
        let action = self.phase.reduce(Event::Stop, Instant::now());
        if let Some(action) = action {
            self.run(action, ctx);
        }
    }

    fn open_folder(&mut self, ctx: &Context) {
        if let Err(error) = settings::open_directory(&self.settings.directory) {
            self.notice = Some(error.to_string());
            self.show_window(ctx);
        }
    }

    fn show_window(&mut self, ctx: &Context) {
        self.window_visible = true;
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
    }

    fn hide_window(&mut self, ctx: &Context) {
        self.window_visible = false;
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
    }

    fn quit(&mut self, ctx: &Context) {
        self.quitting = true;
        if let Some(session) = &self.session {
            session.stop();
            self.quit_deadline = Some(Instant::now() + Duration::from_secs(2));
            ctx.request_repaint_after(Duration::from_millis(100));
        } else {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }

    fn sync_tray(&mut self, ctx: &Context) {
        let state = self.tray_state();
        if self.last_tray.as_ref() == Some(&state) {
            return;
        }
        let result = self.tray.as_mut().map(|tray| tray.sync(&state));
        match result {
            Some(Ok(())) => self.last_tray = Some(state),
            Some(Err(error)) => {
                self.tray = None;
                self.tray_error = Some(error);
                self.notice = Some("The menu bar icon stopped working.".to_owned());
                self.show_window(ctx);
            }
            None => {}
        }
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

    fn hud_state(&self) -> Option<hud::HudState> {
        let phase = match &self.phase {
            Phase::Arming { .. } => hud::HudPhase::Starting,
            Phase::Recording(_) => hud::HudPhase::Recording,
            Phase::Stopping { .. } => hud::HudPhase::Saving,
            Phase::Idle | Phase::Done(_) => return None,
        };
        let live = self.phase.live();
        Some(hud::HudState {
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

    fn handle_intent(&mut self, intent: Intent, ctx: &Context) {
        match intent {
            Intent::Start => self.start_recording(ctx),
            Intent::Stop => self.stop_recording(ctx),
            Intent::Hide => self.hide_window(ctx),
            Intent::OpenFolder => self.open_folder(ctx),
            Intent::Quit => self.quit(ctx),
            Intent::Dismiss => {
                self.phase.reduce(Event::Dismiss, Instant::now());
            }
        }
    }
}

fn repaint(ctx: &Context) -> Repaint {
    let ctx = ctx.clone();
    crate::repaint::repaint(move || ctx.request_repaint())
}

impl eframe::App for KoeApp {
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.poll_tray(ctx);
        self.poll_engine(ctx);
        self.sync_tray(ctx);
        if self.quitting {
            let deadline_passed = self
                .quit_deadline
                .is_some_and(|deadline| Instant::now() >= deadline);
            if self.session.is_none() || deadline_passed {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            return;
        }
        if self.phase.is_active() {
            if !self.window_visible {
                ctx.send_viewport_cmd(ViewportCommand::RequestPaintWhileHidden);
            }
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|input| input.viewport().close_requested())
            && !self.quitting
            && self.tray.is_some()
        {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.hide_window(&ctx);
        }
        let intents = ui::draw(
            ui,
            ui::MainView {
                phase: &self.phase,
                settings: &mut self.settings,
                levels: self.levels,
                tray_available: self.tray.is_some(),
                tray_error: self.tray_error.as_deref(),
                notice: self.notice.as_deref(),
            },
        );
        for intent in intents {
            self.handle_intent(intent, &ctx);
        }
        if let Some(state) = self.hud_state() {
            hud::draw(&ctx, &state);
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }
}
