//! The main window.

use std::time::Duration;

use egui::{
    Align, Color32, CornerRadius, Frame, Label, Layout, Margin, RichText, ScrollArea, Stroke,
};
use koe_capture::Source;
use koe_record::Format;

use crate::settings::Settings;
use crate::state::{Phase, Sources};

const BG: Color32 = Color32::from_rgb(16, 16, 20);
const CARD: Color32 = Color32::from_rgb(28, 28, 34);
const BORDER: Color32 = Color32::from_rgb(46, 46, 54);
const TRACK: Color32 = Color32::from_rgb(44, 44, 52);
const TEXT: Color32 = Color32::from_rgb(232, 232, 236);
const MUTED: Color32 = Color32::from_rgb(140, 140, 150);
const RED: Color32 = Color32::from_rgb(255, 90, 95);
const GREEN: Color32 = Color32::from_rgb(126, 231, 135);
const AMBER: Color32 = Color32::from_rgb(240, 180, 41);
const BLUE: Color32 = Color32::from_rgb(88, 166, 255);

/// A command the window asks the app to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
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

/// Everything the window reads and writes.
pub struct MainView<'a> {
    /// Current take.
    pub phase: &'a Phase,
    /// Editable settings.
    pub settings: &'a mut Settings,
    /// Peak level per source.
    pub levels: [f32; 2],
    /// Whether the tray icon exists.
    pub tray_available: bool,
    /// Why the tray icon failed.
    pub tray_error: Option<&'a str>,
    /// A warning from the last action.
    pub notice: Option<&'a str>,
}

/// Draws the window and returns the commands the user asked for.
pub fn draw(ui: &mut egui::Ui, view: MainView<'_>) -> Vec<Intent> {
    let MainView {
        phase,
        settings,
        levels,
        tray_available,
        tray_error,
        notice,
    } = view;
    let mut intents = Vec::new();
    ui.painter()
        .rect_filled(ui.max_rect(), CornerRadius::ZERO, BG);
    Frame::NONE.inner_margin(Margin::same(22)).show(ui, |ui| {
        egui::Panel::bottom("koe-footer")
            .frame(Frame::NONE)
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);
                footer(ui, tray_available, &mut intents);
            });
        egui::CentralPanel::default()
            .frame(Frame::NONE)
            .show(ui, |ui| {
                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        header(ui, phase);
                        ui.add_space(14.0);
                        status_card(ui, phase, levels);
                        ui.add_space(14.0);
                        primary_button(ui, phase, settings.sources, &mut intents);
                        ui.add_space(14.0);
                        sources(ui, settings, phase);
                        ui.add_space(6.0);
                        format_choice(ui, settings, phase);
                        ui.add_space(6.0);
                        folder(ui, settings, &mut intents);
                        if let Some(notice) = notice {
                            ui.add_space(12.0);
                            banner(ui, notice, AMBER);
                        }
                        if !tray_available {
                            ui.add_space(12.0);
                            banner(
                                ui,
                                "No menu bar icon. Keep this window open to control recordings.",
                                AMBER,
                            );
                            if let Some(error) = tray_error {
                                ui.label(RichText::new(error).size(10.0).color(MUTED));
                            }
                        }
                        if let Phase::Done(done) = phase {
                            ui.add_space(12.0);
                            finished(ui, done, &mut intents);
                        }
                    });
            });
    });
    intents
}

fn header(ui: &mut egui::Ui, phase: &Phase) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("koe").size(30.0).strong().color(TEXT));
        ui.add_space(8.0);
        ui.label(RichText::new("private recording").size(12.0).color(MUTED));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (colour, text) = phase_style(phase);
            Frame::NONE
                .fill(colour.gamma_multiply(0.18))
                .corner_radius(10)
                .inner_margin(Margin::symmetric(10, 4))
                .show(ui, |ui| {
                    ui.label(RichText::new(text).size(11.0).strong().color(colour));
                });
        });
    });
}

fn status_card(ui: &mut egui::Ui, phase: &Phase, levels: [f32; 2]) {
    Frame::NONE
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(16)
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (colour, _) = phase_style(phase);
                let (dot, _response) =
                    ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), 7.0, colour);
                ui.vertical(|ui| {
                    ui.label(RichText::new(phase.label()).size(17.0).strong().color(TEXT));
                    ui.label(RichText::new(detail(phase)).size(12.0).color(MUTED));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(elapsed) = phase.elapsed(std::time::Instant::now()) {
                        ui.label(
                            RichText::new(clock(elapsed))
                                .monospace()
                                .size(24.0)
                                .color(TEXT),
                        );
                    }
                });
            });
            if phase.is_active() {
                ui.add_space(10.0);
                meters(ui, levels, phase.live());
            }
        });
}

fn meters(ui: &mut egui::Ui, levels: [f32; 2], live: Sources) {
    for (source, name) in [(Source::Microphone, "Mic"), (Source::SystemAudio, "System")] {
        let running = live.contains(source);
        let level = levels[Sources::index(source)].clamp(0.0, 1.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(name).size(10.0).color(MUTED));
            let width = (ui.available_width() - 4.0).max(20.0);
            let (rect, _response) =
                ui.allocate_exact_size(egui::vec2(width, 6.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 3, TRACK);
            if running {
                let filled =
                    egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * level, 6.0));
                ui.painter().rect_filled(filled, 3, GREEN);
            }
        });
        ui.add_space(2.0);
    }
}

fn primary_button(ui: &mut egui::Ui, phase: &Phase, sources: Sources, intents: &mut Vec<Intent>) {
    let (label, intent, enabled, fill, text) = if phase.can_stop() {
        ("Stop recording", Intent::Stop, true, RED, Color32::WHITE)
    } else if phase.can_start() {
        (
            "Start recording",
            Intent::Start,
            !sources.is_empty(),
            Color32::from_rgb(238, 238, 242),
            Color32::BLACK,
        )
    } else {
        (
            "Saving...",
            Intent::Stop,
            false,
            Color32::from_rgb(70, 70, 78),
            MUTED,
        )
    };
    let button = egui::Button::new(RichText::new(label).size(15.0).strong().color(text))
        .fill(fill)
        .corner_radius(12)
        .min_size(egui::vec2(ui.available_width(), 44.0));
    if ui.add_enabled(enabled, button).clicked() {
        intents.push(intent);
    }
}

fn sources(ui: &mut egui::Ui, settings: &mut Settings, phase: &Phase) {
    ui.label(RichText::new("Sources").size(12.0).color(MUTED));
    let editable = phase.can_start();
    let mut selected = settings.sources;
    for (source, label) in [
        (Source::Microphone, "Microphone"),
        (Source::SystemAudio, "System audio"),
    ] {
        let mut checked = selected.contains(source);
        if ui
            .add_enabled(
                editable,
                egui::Checkbox::new(&mut checked, RichText::new(label).size(13.0)),
            )
            .changed()
        {
            selected = if checked {
                selected.with(source)
            } else {
                selected.without(source)
            };
        }
    }
    settings.sources = selected;
}

fn format_choice(ui: &mut egui::Ui, settings: &mut Settings, phase: &Phase) {
    ui.label(RichText::new("Format").size(12.0).color(MUTED));
    ui.add_enabled_ui(phase.can_start(), |ui| {
        ui.horizontal(|ui| {
            for (format, label) in [(Format::Ogg, "Ogg Opus"), (Format::Wav, "WAV")] {
                if ui
                    .selectable_label(settings.format == format, label)
                    .clicked()
                {
                    settings.format = format;
                }
            }
        });
    });
}

fn folder(ui: &mut egui::Ui, settings: &Settings, intents: &mut Vec<Intent>) {
    ui.label(RichText::new("Recordings").size(12.0).color(MUTED));
    ui.horizontal(|ui| {
        ui.add(
            Label::new(
                RichText::new(settings.directory.display().to_string())
                    .monospace()
                    .size(11.0)
                    .color(MUTED),
            )
            .truncate(),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Open").clicked() {
                intents.push(Intent::OpenFolder);
            }
        });
    });
}

fn finished(ui: &mut egui::Ui, done: &crate::state::Done, intents: &mut Vec<Intent>) {
    Frame::NONE
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(12)
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let (colour, title) = if done.failed() {
                (RED, "Take failed")
            } else {
                (GREEN, "Last take")
            };
            ui.label(RichText::new(title).size(13.0).strong().color(colour));
            let empty = done.summary.is_some_and(|summary| summary.frames == 0);
            if empty {
                ui.label(
                    RichText::new("No audio was written, so the empty file was removed.")
                        .size(11.0)
                        .color(MUTED),
                );
            } else {
                ui.label(
                    RichText::new(done.output.display().to_string())
                        .monospace()
                        .size(10.0)
                        .color(MUTED),
                );
                if let Some(summary) = done.summary {
                    ui.label(
                        RichText::new(format!(
                            "{} · {} frames",
                            clock(summary.duration),
                            summary.frames
                        ))
                        .size(11.0)
                        .color(TEXT),
                    );
                }
            }
            if let Some(failure) = &done.failure {
                ui.label(RichText::new(failure.headline()).size(11.0).color(RED));
                ui.label(RichText::new(failure.to_string()).size(10.0).color(MUTED));
            }
            if let Some(warning) = &done.warning {
                ui.label(RichText::new(warning).size(10.0).color(AMBER));
            }
            if ui.button("Dismiss").clicked() {
                intents.push(Intent::Dismiss);
            }
        });
}

fn footer(ui: &mut egui::Ui, tray_available: bool, intents: &mut Vec<Intent>) {
    ui.horizontal(|ui| {
        if tray_available && ui.button("Hide to menu bar").clicked() {
            intents.push(Intent::Hide);
        }
        if ui.button("Quit").clicked() {
            intents.push(Intent::Quit);
        }
    });
}

fn banner(ui: &mut egui::Ui, text: &str, colour: Color32) {
    Frame::NONE
        .fill(colour.gamma_multiply(0.16))
        .stroke(Stroke::new(1.0, colour.gamma_multiply(0.4)))
        .corner_radius(8)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.0).color(TEXT));
        });
}

fn phase_style(phase: &Phase) -> (Color32, &'static str) {
    match phase {
        Phase::Idle => (MUTED, "READY"),
        Phase::Arming { .. } => (AMBER, "STARTING"),
        Phase::Recording(_) => (RED, "REC"),
        Phase::Stopping { .. } => (BLUE, "SAVING"),
        Phase::Done(done) if done.failed() => (RED, "FAILED"),
        Phase::Done(_) => (GREEN, "SAVED"),
    }
}

fn detail(phase: &Phase) -> String {
    match phase {
        Phase::Idle => "ready when you are".to_owned(),
        Phase::Arming { .. } => "opening sources".to_owned(),
        Phase::Recording(live) => format!("{} source(s) live", live.live.len()),
        Phase::Stopping { .. } => "finishing the file".to_owned(),
        Phase::Done(done) if done.failed() => done.failure.as_ref().map_or_else(
            || "the take did not complete".to_owned(),
            |failure| failure.headline().to_owned(),
        ),
        Phase::Done(_) => "saved to your recordings folder".to_owned(),
    }
}

fn clock(duration: Duration) -> String {
    let total = duration.as_secs();
    if total >= 3_600 {
        format!(
            "{}:{:02}:{:02}",
            total / 3_600,
            (total % 3_600) / 60,
            total % 60
        )
    } else {
        format!("{:02}:{:02}", total / 60, total % 60)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_clock_shows_minutes_and_hours() {
        assert_eq!(clock(Duration::ZERO), "00:00");
        assert_eq!(clock(Duration::from_secs(65)), "01:05");
        assert_eq!(clock(Duration::from_secs(3_661)), "1:01:01");
    }
}
