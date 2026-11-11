//! The main window, built from gpui-kit components.

use std::time::Instant;

use gpui::{
    App, Context, Entity, FontWeight, IntoElement, ParentElement, Render, Styled as _, Window, div,
    px, rgb,
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, TitleBar,
    alert::Alert,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    group_box::{GroupBox, GroupBoxVariants as _},
    h_flex,
    progress::Progress,
    scroll::ScrollableElement as _,
    v_flex,
};
use koe_capture::Source;
use koe_record::Format;

use crate::app::Shell;
use crate::hud;
use crate::state::{Done, Phase, Sources};

const LIVE: u32 = 0x007E_E787;
const IDLE: u32 = 0x005A_5A62;
const RED: u32 = 0x00FF_5A5F;

const AMBER: u32 = 0x00F0_B429;
const BLUE: u32 = 0x0058_A6FF;

/// The main window's root view.
pub struct MainView {
    shell: Entity<Shell>,
}

impl MainView {
    /// Builds the view and re-renders it whenever the shell changes.
    pub fn new(shell: Entity<Shell>, cx: &mut Context<Self>) -> Self {
        cx.observe(&shell, |_, _, cx| cx.notify()).detach();
        Self { shell }
    }
}

impl Render for MainView {
    #[allow(
        clippy::too_many_lines,
        reason = "the window is one element tree; splitting it scatters the layout"
    )]
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = self.shell.clone();
        let shell = entity.read(cx);
        let theme = cx.theme();

        let phase = &shell.phase;
        let active = phase.is_active();
        let can_start = phase.can_start();
        let can_stop = phase.can_stop();
        let elapsed = phase.elapsed(Instant::now());
        let live = phase.live();
        let levels = shell.levels;
        let detail = detail(phase);
        let tone = phase_tone(phase);
        let badge = phase.label().to_uppercase();

        let mic = shell.settings.sources.contains(Source::Microphone);
        let system = shell.settings.sources.contains(Source::SystemAudio);
        let format = shell.settings.format;
        let directory = shell.settings.directory.display().to_string();
        let notice = shell.notice.clone();
        let tray_available = shell.tray.is_some();
        let tray_error = shell.tray_error.clone();
        let done = match phase {
            Phase::Done(done) => Some(done_view(done)),
            _ => None,
        };

        let (primary_label, primary_enabled, primary_stop) = if can_stop {
            ("Stop recording", true, true)
        } else if can_start {
            ("Start recording", !shell.settings.sources.is_empty(), false)
        } else {
            ("Saving...", false, false)
        };

        let start_shell = entity.clone();
        let on_primary = move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            start_shell.update(cx, |shell, cx| {
                if primary_stop {
                    shell.stop_recording(cx);
                } else {
                    shell.start_recording(cx);
                }
            });
        };

        let mic_shell = entity.clone();
        let on_mic = move |checked: &bool, _: &mut Window, cx: &mut App| {
            let checked = *checked;
            mic_shell.update(cx, |shell, cx| {
                shell.settings.sources = if checked {
                    shell.settings.sources.with(Source::Microphone)
                } else {
                    shell.settings.sources.without(Source::Microphone)
                };
                cx.notify();
            });
        };

        let system_shell = entity.clone();
        let on_system = move |checked: &bool, _: &mut Window, cx: &mut App| {
            let checked = *checked;
            system_shell.update(cx, |shell, cx| {
                shell.settings.sources = if checked {
                    shell.settings.sources.with(Source::SystemAudio)
                } else {
                    shell.settings.sources.without(Source::SystemAudio)
                };
                cx.notify();
            });
        };

        let format_shell = entity.clone();
        let on_ogg = move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            format_shell.update(cx, |shell, cx| {
                shell.settings.format = Format::Ogg;
                cx.notify();
            });
        };
        let format_shell = entity.clone();
        let on_wav = move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            format_shell.update(cx, |shell, cx| {
                shell.settings.format = Format::Wav;
                cx.notify();
            });
        };

        let open_shell = entity.clone();
        let on_open = move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            open_shell.update(cx, Shell::open_folder);
        };

        let hide_shell = entity.clone();
        let on_hide = move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            hide_shell.update(cx, Shell::hide_window);
        };

        let quit_shell = entity.clone();
        let on_quit = move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            quit_shell.update(cx, Shell::quit);
        };

        let dismiss_shell = entity.clone();
        let on_dismiss = move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            dismiss_shell.update(cx, Shell::dismiss);
        };

        let primary = Button::new("primary")
            .label(primary_label)
            .disabled(!primary_enabled)
            .w_full()
            .h(px(44.0))
            .on_click(on_primary);
        let primary = if primary_stop {
            primary.danger()
        } else {
            primary.primary()
        };

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                TitleBar::new().child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .child(
                            div()
                                .text_lg()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("koe"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("private recording"),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(tone)
                                .child(badge),
                        ),
                ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scrollbar()
                    .p(px(22.0))
                    .gap(px(14.0))
                    .child(
                        GroupBox::new().outline().child(
                            v_flex()
                                .gap(px(10.0))
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap(px(10.0))
                                        .child(
                                            div()
                                                .size(px(14.0))
                                                .rounded_full()
                                                .bg(tone),
                                        )
                                        .child(
                                            v_flex()
                                                .flex_1()
                                                .child(
                                                    div()
                                                        .text_base()
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .child(phase.label()),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(theme.muted_foreground)
                                                        .child(detail),
                                                ),
                                        )
                                        .children(elapsed.map(|elapsed| {
                                            div()
                                                .text_2xl()
                                                .font_family("monospace")
                                                .child(hud::elapsed_text(elapsed))
                                        })),
                                )
                                .children(active.then(|| {
                                    v_flex()
                                        .gap_2()
                                        .child(meter("Mic", Source::Microphone, levels, live))
                                        .child(meter("System", Source::SystemAudio, levels, live))
                                })),
                        ),
                    )
                    .child(primary)
                    .child(
                        v_flex()
                            .gap_2()
                            .child(section("Sources", cx))
                            .child(
                                Checkbox::new("source-microphone")
                                    .label("Microphone")
                                    .checked(mic)
                                    .disabled(!can_start)
                                    .on_change(on_mic),
                            )
                            .child(
                                Checkbox::new("source-system")
                                    .label("System audio")
                                    .checked(system)
                                    .disabled(!can_start)
                                    .on_change(on_system),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_2()
                            .child(section("Format", cx))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        if format == Format::Ogg {
                                            Button::new("format-ogg").primary()
                                        } else {
                                            Button::new("format-ogg").ghost()
                                        }
                                        .label("Ogg Opus")
                                        .small()
                                        .disabled(!can_start)
                                        .on_click(on_ogg),
                                    )
                                    .child(
                                        if format == Format::Wav {
                                            Button::new("format-wav").primary()
                                        } else {
                                            Button::new("format-wav").ghost()
                                        }
                                        .label("WAV")
                                        .small()
                                        .disabled(!can_start)
                                        .on_click(on_wav),
                                    ),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_2()
                            .child(section("Recordings", cx))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.0))
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_sm()
                                            .font_family("monospace")
                                            .text_color(theme.muted_foreground)
                                            .child(directory),
                                    )
                                    .child(
                                        Button::new("open-folder")
                                            .label("Open")
                                            .small()
                                            .on_click(on_open),
                                    ),
                            ),
                    )
                    .children(notice.map(|notice| {
                        Alert::warning("notice", notice).banner().into_any_element()
                    }))
                    .children((!tray_available).then(|| {
                        v_flex()
                            .gap_1()
                            .child(
                                Alert::warning(
                                    "tray-missing",
                                    "No menu bar icon. Keep this window open to control recordings.",
                                )
                                .banner(),
                            )
                            .children(tray_error.map(|error| {
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(error)
                            }))
                            .into_any_element()
                    }))
                    .children(done.map(|done| done_card(done, on_dismiss, cx))),
            )
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .px(px(22.0))
                    .py_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .gap_2()
                            .children(tray_available.then(|| {
                                Button::new("hide")
                                    .label("Hide to menu bar")
                                    .small()
                                    .on_click(on_hide)
                            }))
                            .child(
                                Button::new("quit")
                                    .label("Quit")
                                    .small()
                                    .ghost()
                                    .on_click(on_quit),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("koe"),
                    ),
            )
    }
}

fn section(title: &str, cx: &Context<MainView>) -> impl IntoElement {
    div()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(cx.theme().muted_foreground)
        .child(title.to_owned())
}

fn meter(name: &str, source: Source, levels: [f32; 2], live: Sources) -> impl IntoElement {
    let index = Sources::index(source);
    let level = levels[index];
    let level = if level.is_finite() {
        level.clamp(0.0, 1.0)
    } else {
        0.0
    };
    h_flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(56.0))
                .text_xs()
                .text_color(rgb(IDLE))
                .child(name.to_owned()),
        )
        .child(
            Progress::new(format!("meter-{index}"))
                .value(level * 100.0)
                .color(rgb(if live.contains(source) { LIVE } else { IDLE }))
                .with_size(px(6.0))
                .flex_1(),
        )
}

struct DoneView {
    output: String,
    failed: bool,
    empty: bool,
    summary: Option<String>,
    headline: Option<String>,
    failure: Option<String>,
    warning: Option<String>,
}

fn done_view(done: &Done) -> DoneView {
    DoneView {
        output: done.output.display().to_string(),
        failed: done.failed(),
        empty: done.summary.is_some_and(|summary| summary.frames == 0),
        summary: done.summary.map(|summary| {
            format!(
                "{} · {} frames",
                hud::elapsed_text(summary.duration),
                summary.frames
            )
        }),
        headline: done
            .failure
            .as_ref()
            .map(|failure| failure.headline().to_owned()),
        failure: done.failure.as_ref().map(ToString::to_string),
        warning: done.warning.clone(),
    }
}

fn done_card(
    done: DoneView,
    on_dismiss: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    cx: &Context<MainView>,
) -> impl IntoElement {
    let theme = cx.theme();
    let colour = if done.failed { rgb(RED) } else { rgb(LIVE) };
    GroupBox::new().outline().child(
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colour)
                    .child(if done.failed {
                        "Take failed"
                    } else {
                        "Last take"
                    }),
            )
            .children(done.empty.then(|| {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("No audio was written, so the empty file was removed.")
            }))
            .children((!done.empty).then(|| {
                div()
                    .text_xs()
                    .font_family("monospace")
                    .text_color(theme.muted_foreground)
                    .child(done.output)
            }))
            .children(done.summary.map(|summary| div().text_xs().child(summary)))
            .children(
                done.headline
                    .map(|headline| div().text_xs().text_color(rgb(RED)).child(headline)),
            )
            .children(done.failure.map(|failure| {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(failure)
            }))
            .children(
                done.warning
                    .map(|warning| div().text_xs().text_color(rgb(AMBER)).child(warning)),
            )
            .child(
                Button::new("dismiss")
                    .label("Dismiss")
                    .small()
                    .on_click(on_dismiss),
            ),
    )
}

fn phase_tone(phase: &Phase) -> gpui::Rgba {
    match phase {
        Phase::Idle => rgb(IDLE),
        Phase::Arming { .. } => rgb(AMBER),
        Phase::Recording(_) => rgb(RED),
        Phase::Stopping { .. } => rgb(BLUE),
        Phase::Done(done) if done.failed() => rgb(RED),
        Phase::Done(_) => rgb(LIVE),
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
