//! The Rune main window.

use std::time::{Duration, Instant};

use koe_capture::Source;
use koe_record::Format;
use rune::tok::{self, type_};
use rune::{Color, Cx, El, TextStyle, col, el, row, text, text_input};

use crate::dictation::Dictation;
use crate::hotkey;
use crate::rune_app::{AppState, Screen, UiIntent};
use crate::settings;
use crate::state::{Phase, Sources};

const RED: Color = Color {
    r: 1.0,
    g: 0.353,
    b: 0.373,
    a: 1.0,
};
const GREEN: Color = Color {
    r: 0.494,
    g: 0.906,
    b: 0.529,
    a: 1.0,
};
const AMBER: Color = Color {
    r: 0.941,
    g: 0.706,
    b: 0.161,
    a: 1.0,
};
const BLUE: Color = Color {
    r: 0.345,
    g: 0.651,
    b: 1.0,
    a: 1.0,
};

/// Draws the window.
#[must_use]
pub fn view(state: &AppState, _: &mut Cx<AppState>) -> El {
    let theme = tok::theme(true);
    let mut content = col()
        .w_full()
        .h_full()
        .padding(22.0)
        .gap(12.0)
        .fill(theme.bg)
        .child(header(state, &theme));
    match state.screen {
        Screen::Recorder => {
            content = content
                .child(status_card(state, &theme))
                .child(primary_button(state, &theme));
        }
        Screen::Settings => {
            content = content
                .child(dictation(state, &theme))
                .child(sources(state, &theme))
                .child(format_choice(state, &theme))
                .child(folder(state, &theme));
        }
    }
    if let Some(notice) = &state.notice {
        content = content.child(banner(notice.as_str(), AMBER));
    }
    if state.tray.is_none()
        && let Some(error) = &state.tray_error
    {
        content = content.child(banner(
            "No menu bar icon. Keep this window open to control recordings.",
            AMBER,
        ));
        content = content.child(
            text(error.as_str())
                .font(TextStyle::new(10.0, 400.0))
                .color(theme.text_secondary)
                .truncate(),
        );
    }
    if state.dictation_control.hotkeys.is_none()
        && let Some(error) = &state.dictation_control.hotkey_error
    {
        content = content.child(banner(
            "No global hotkey. Dictation needs a desktop that lets apps register shortcuts; on Linux that means X11. Press Enter in the Hotkey field to retry.",
            AMBER,
        ));
        content = content.child(
            text(error.as_str())
                .font(TextStyle::new(10.0, 400.0))
                .color(theme.text_secondary)
                .truncate(),
        );
    }
    if let Phase::Done(done) = &state.phase {
        content = content.child(finished(done, &theme));
    }
    content = content.child(el().grow(1.0)).child(footer(state, &theme));
    content
}

fn header(state: &AppState, theme: &tok::Theme) -> El {
    let (colour, label) = phase_style(&state.phase);
    row()
        .w_full()
        .items_center()
        .gap(8.0)
        .child(
            text("koe")
                .font(TextStyle::new(28.0, 700.0))
                .color(theme.text),
        )
        .child(
            text("private recording")
                .font(type_::LABEL)
                .color(theme.text_secondary),
        )
        .child(el().grow(1.0))
        .child(screen_button(state, theme))
        .child(
            el().padding(8.0)
                .radius(10.0)
                .fill(colour.alpha(0.16))
                .child(text(label).font(type_::SECTION).color(colour)),
        )
}

/// The header button that flips the window between the two screens.
fn screen_button(state: &AppState, theme: &tok::Theme) -> El {
    let (label, intent) = if state.screen == Screen::Settings {
        ("Back", UiIntent::ShowRecorder)
    } else {
        ("Settings", UiIntent::ShowSettings)
    };
    el().padding(8.0)
        .radius(8.0)
        .fill(theme.control_track)
        .role("button")
        .label(label)
        .focusable(true)
        .child(text(label).font(type_::LABEL).color(theme.text))
        .on_press(move |s: &mut AppState| s.pending = Some(intent))
}

fn status_card(state: &AppState, theme: &tok::Theme) -> El {
    let (colour, _) = phase_style(&state.phase);
    let mut row_content = row()
        .w_full()
        .items_center()
        .gap(12.0)
        .child(el().size(14.0, 14.0).radius(7.0).fill(colour))
        .child(
            col()
                .gap(2.0)
                .child(
                    text(state.phase.label())
                        .font(type_::TITLE)
                        .color(theme.text),
                )
                .child(
                    text(detail(&state.phase))
                        .font(type_::LABEL)
                        .color(theme.text_secondary),
                ),
        )
        .child(el().grow(1.0));
    if let Some(elapsed) = state.phase.elapsed(Instant::now()) {
        row_content = row_content.child(
            text(clock(elapsed))
                .font(TextStyle::new(22.0, 500.0))
                .color(theme.text),
        );
    }
    let mut card = col()
        .w_full()
        .padding(16.0)
        .radius(16.0)
        .fill(theme.card)
        .border(1.0, theme.card_border)
        .child(row_content);
    if state.phase.is_active() {
        card = card.child(meters(state, theme));
    }
    card
}

fn meters(state: &AppState, theme: &tok::Theme) -> El {
    let live = state.phase.live();
    let mut bars = col().w_full().gap(4.0);
    for (source, label) in [(Source::Microphone, "Mic"), (Source::SystemAudio, "System")] {
        let width = (state.levels[Sources::index(source)].clamp(0.0, 1.0) * 300.0).max(0.0);
        bars =
            bars.child(
                row()
                    .w_full()
                    .items_center()
                    .gap(8.0)
                    .child(
                        text(label)
                            .font(TextStyle::new(10.0, 500.0))
                            .color(theme.text_secondary),
                    )
                    .child(
                        row()
                            .grow(1.0)
                            .h(6.0)
                            .radius(3.0)
                            .fill(theme.control_track)
                            .child(el().w(width).h(6.0).radius(3.0).fill(
                                if live.contains(source) {
                                    GREEN
                                } else {
                                    theme.control_off
                                },
                            )),
                    ),
            );
    }
    bars
}

fn primary_button(state: &AppState, theme: &tok::Theme) -> El {
    let (label, intent, enabled, fill, text_colour) = if state.phase.can_stop() {
        ("Stop recording", UiIntent::Stop, true, RED, Color::WHITE)
    } else if state.phase.can_start() {
        (
            "Start recording",
            UiIntent::Start,
            !state.settings.sources.is_empty(),
            Color::WHITE,
            Color::BLACK,
        )
    } else {
        (
            "Saving...",
            UiIntent::Stop,
            false,
            theme.control_off,
            theme.text_secondary,
        )
    };
    let button = el()
        .w_full()
        .h(44.0)
        .radius(12.0)
        .fill(fill)
        .items_center()
        .justify_center()
        .role("button")
        .label(label)
        .focusable(true)
        .child(text(label).font(type_::TITLE).color(text_colour));
    if enabled {
        button.on_press(move |s: &mut AppState| s.pending = Some(intent))
    } else {
        button.opacity(0.5).disabled(true)
    }
}

fn sources(state: &AppState, theme: &tok::Theme) -> El {
    let editable = state.phase.can_start();
    let mut column = col().w_full().gap(6.0).child(
        text("Sources")
            .font(type_::SECTION)
            .color(theme.text_secondary),
    );
    for (source, label) in [
        (Source::Microphone, "Microphone"),
        (Source::SystemAudio, "System audio"),
    ] {
        let selected = state.settings.sources.contains(source);
        let mut checkbox = el()
            .size(16.0, 16.0)
            .radius(4.0)
            .items_center()
            .justify_center()
            .fill(if selected {
                tok::accent(0, true)
            } else {
                theme.control_track
            })
            .border(1.0, theme.control_off);
        if selected {
            checkbox = checkbox.child(el().size(6.0, 6.0).radius(1.0).fill(Color::WHITE));
        }
        let mut row_content = row()
            .w_full()
            .items_center()
            .gap(10.0)
            .padding(8.0)
            .radius(8.0)
            .fill(theme.card)
            .role("checkbox")
            .label(label)
            .checked(selected)
            .focusable(true)
            .child(checkbox)
            .child(text(label).font(type_::BODY).color(theme.text));
        if editable {
            row_content = row_content.on_press(move |s: &mut AppState| {
                s.settings.sources = if s.settings.sources.contains(source) {
                    s.settings.sources.without(source)
                } else {
                    s.settings.sources.with(source)
                };
            });
        } else {
            row_content = row_content.opacity(0.5).disabled(true);
        }
        column = column.child(row_content);
    }
    column
}

fn format_choice(state: &AppState, theme: &tok::Theme) -> El {
    let editable = state.phase.can_start();
    let mut row_content = row().w_full().items_center().gap(8.0).child(
        text("Format")
            .font(type_::SECTION)
            .color(theme.text_secondary),
    );
    for (format, label) in [(Format::Ogg, "Ogg Opus"), (Format::Wav, "WAV")] {
        let selected = state.settings.format == format;
        let mut choice = el()
            .padding(8.0)
            .radius(8.0)
            .fill(if selected {
                tok::accent(0, true).alpha(0.22)
            } else {
                theme.card
            })
            .child(text(label).font(type_::LABEL).color(if selected {
                theme.text
            } else {
                theme.text_secondary
            }));
        if editable {
            choice = choice.on_press(move |s: &mut AppState| s.settings.format = format);
        } else {
            choice = choice.opacity(0.5).disabled(true);
        }
        row_content = row_content.child(choice);
    }
    row_content
}

fn dictation(state: &AppState, theme: &tok::Theme) -> El {
    let mut column = col().w_full().gap(6.0).child(
        text("Dictation")
            .font(type_::SECTION)
            .color(theme.text_secondary),
    );
    column = column.child(
        text(dictation_status(state).as_str())
            .font(type_::BODY)
            .color(theme.text),
    );
    column = column.child(field_row(
        theme,
        "Hotkey",
        text_input(&state.hotkey_draft, TextStyle::new(13.0, 400.0))
            .single_line()
            .placeholder("Ctrl+Shift+Space", theme.text_secondary)
            .w_full()
            .on_edit(|s: &mut AppState| &mut s.hotkey_draft)
            .on_submit(|s: &mut AppState, _| s.pending = Some(UiIntent::ApplyHotkey)),
    ));
    let draft = state.hotkey_draft.text.trim();
    if let Some(draft) = settings::value_or_none(draft) {
        match hotkey::parse(draft) {
            Err(error) => {
                column = column.child(
                    text(error.to_string().as_str())
                        .font(type_::LABEL)
                        .color(RED)
                        .truncate(),
                );
            }
            Ok(hotkey) if hotkey != state.dictation_control.settings.hotkey => {
                column = column.child(
                    text("Press Enter to apply this binding")
                        .font(type_::LABEL)
                        .color(theme.text_secondary),
                );
            }
            Ok(_) => {}
        }
    }
    column = column.child(field_row(
        theme,
        "Model",
        text_input(&state.model_draft, TextStyle::new(13.0, 400.0))
            .single_line()
            .placeholder("/path/to/model.gguf", theme.text_secondary)
            .w_full()
            .on_edit(|s: &mut AppState| &mut s.model_draft),
    ));
    column = column.child(
        text(model_hint(state).as_str())
            .font(type_::LABEL)
            .color(theme.text_secondary),
    );
    column = column.child(field_row(
        theme,
        "Language",
        text_input(&state.language_draft, TextStyle::new(13.0, 400.0))
            .single_line()
            .placeholder("ja", theme.text_secondary)
            .w_full()
            .on_edit(|s: &mut AppState| &mut s.language_draft),
    ));
    if let Dictation::Done(_) = &state.dictation_control.dictation {
        column = column.child(
            el().padding(8.0)
                .radius(8.0)
                .fill(theme.control_track)
                .role("button")
                .label("Clear dictation")
                .focusable(true)
                .child(text("Clear").font(type_::LABEL).color(theme.text))
                .on_press(|s: &mut AppState| s.pending = Some(UiIntent::DismissDictation)),
        );
    }
    column
}

/// One settings field: a label and the input beside it.
fn field_row(theme: &tok::Theme, label: &str, input: El) -> El {
    row()
        .w_full()
        .items_center()
        .gap(8.0)
        .child(text(label).font(type_::LABEL).color(theme.text_secondary))
        .child(input)
}

/// The status sentence the dictation section shows.
fn dictation_status(state: &AppState) -> String {
    let binding = hotkey::display(&state.dictation_control.settings.hotkey);
    match &state.dictation_control.dictation {
        Dictation::Idle => format!("Press {binding} anywhere to dictate"),
        Dictation::Listening { .. } => {
            let elapsed = state
                .dictation_control
                .dictation
                .elapsed(Instant::now())
                .map_or_else(String::new, crate::hud::elapsed_text);
            format!("Dictating {elapsed} — press {binding} to stop")
        }
        Dictation::Stopping { .. } => "Transcribing — the text follows".to_owned(),
        Dictation::Done(done) if done.failed() => done.failure.as_ref().map_or_else(
            || "Dictation heard nothing".to_owned(),
            |failure| format!("{}: {failure}", failure.headline()),
        ),
        Dictation::Done(done) => format!("Typed {}", done.text),
    }
}

/// What the model field should explain on this platform.
fn model_hint(state: &AppState) -> String {
    #[cfg(target_os = "macos")]
    {
        if settings::value_or_none(&state.model_draft.text).is_none() {
            return "Leave the model empty to use the system speech model".to_owned();
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        if settings::dictation_model(&state.dictation_control.settings).is_none() {
            return "No transcription model set: dictation will refuse to start".to_owned();
        }
    }
    "The GGUF file a dictation transcribes with".to_owned()
}

fn folder(state: &AppState, theme: &tok::Theme) -> El {
    let path = state.settings.directory.display().to_string();
    row()
        .w_full()
        .items_center()
        .gap(8.0)
        .child(
            text("Recordings")
                .font(type_::SECTION)
                .color(theme.text_secondary),
        )
        .child(
            text(path.as_str())
                .font(TextStyle::new(11.0, 400.0))
                .color(theme.text_secondary)
                .max_w(250.0)
                .truncate(),
        )
        .child(el().grow(1.0))
        .child(
            el().padding(8.0)
                .radius(8.0)
                .fill(theme.card)
                .border(1.0, theme.card_border)
                .role("button")
                .label("Open recordings folder")
                .focusable(true)
                .child(text("Open").font(type_::LABEL).color(theme.text))
                .on_press(|s: &mut AppState| s.pending = Some(UiIntent::OpenFolder)),
        )
}

fn finished(done: &crate::state::Done, theme: &tok::Theme) -> El {
    let failed = done.failed();
    let mut column = col()
        .w_full()
        .gap(6.0)
        .padding(12.0)
        .radius(12.0)
        .fill(theme.card)
        .border(1.0, theme.card_border)
        .child(
            text(if failed { "Take failed" } else { "Last take" })
                .font(type_::TITLE)
                .color(if failed { RED } else { GREEN }),
        );
    let empty = done.summary.is_some_and(|summary| summary.frames == 0);
    if empty {
        column = column.child(
            text("No audio was written, so the empty file was removed.")
                .font(type_::LABEL)
                .color(theme.text_secondary),
        );
    } else {
        column = column
            .child(
                text(done.output.display().to_string().as_str())
                    .font(TextStyle::new(10.0, 400.0))
                    .color(theme.text_secondary)
                    .truncate(),
            )
            .child(
                text(
                    done.summary
                        .map_or_else(
                            || "recording did not complete".to_owned(),
                            |summary| {
                                format!("{} · {} frames", clock(summary.duration), summary.frames)
                            },
                        )
                        .as_str(),
                )
                .font(type_::LABEL)
                .color(theme.text),
            );
    }
    if let Some(failure) = &done.failure {
        column = column
            .child(text(failure.headline()).font(type_::LABEL).color(RED))
            .child(
                text(failure.to_string().as_str())
                    .font(TextStyle::new(10.0, 400.0))
                    .color(theme.text_secondary)
                    .truncate(),
            );
    }
    if let Some(warning) = &done.warning {
        column = column.child(
            text(warning.as_str())
                .font(TextStyle::new(10.0, 400.0))
                .color(AMBER)
                .truncate(),
        );
    }
    column.child(
        el().padding(8.0)
            .radius(8.0)
            .fill(theme.control_track)
            .role("button")
            .label("Dismiss")
            .focusable(true)
            .child(text("Dismiss").font(type_::LABEL).color(theme.text))
            .on_press(|s: &mut AppState| s.pending = Some(UiIntent::Dismiss)),
    )
}

fn footer(state: &AppState, theme: &tok::Theme) -> El {
    let mut row_content = row().w_full().items_center().gap(8.0);
    if state.tray.is_some() {
        row_content = row_content.child(
            el().padding(8.0)
                .radius(8.0)
                .fill(theme.card)
                .border(1.0, theme.card_border)
                .role("button")
                .label("Hide to menu bar")
                .focusable(true)
                .child(
                    text("Hide to menu bar")
                        .font(type_::LABEL)
                        .color(theme.text),
                )
                .on_press(|s: &mut AppState| s.pending = Some(UiIntent::Hide)),
        );
    }
    row_content.child(
        el().padding(8.0)
            .radius(8.0)
            .fill(theme.card)
            .border(1.0, theme.card_border)
            .role("button")
            .label("Quit")
            .focusable(true)
            .child(text("Quit").font(type_::LABEL).color(theme.text))
            .on_press(|s: &mut AppState| s.pending = Some(UiIntent::Quit)),
    )
}

fn banner(value: &str, colour: Color) -> El {
    el().w_full()
        .padding(10.0)
        .radius(8.0)
        .fill(colour.alpha(0.16))
        .border(1.0, colour.alpha(0.4))
        .child(text(value).font(type_::LABEL).color(colour))
}

fn phase_style(phase: &Phase) -> (Color, &'static str) {
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

const MUTED: Color = Color {
    r: 0.55,
    g: 0.55,
    b: 0.6,
    a: 1.0,
};
