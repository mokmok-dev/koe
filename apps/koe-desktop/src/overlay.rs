//! The always-on-top overlay window that shows what a take or a dictation is
//! doing.

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Div, Entity, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement, Pixels, Render,
    StatefulInteractiveElement as _, Styled as _, WeakEntity, Window, WindowBackgroundAppearance,
    WindowBounds, WindowDecorations, WindowKind, WindowOptions, div, point, px, rgb, rgba, size,
};

use crate::app::Shell;
use crate::hud::{
    EDGE_MARGIN, HUD_HEIGHT, HUD_WIDTH, HudPhase, HudState, TAKE_HEIGHT, TAKE_WIDTH, badge,
    elapsed_text,
};

const AMBER: u32 = 0x00F0_B429;
const TRANSCRIPT_TEXT: u32 = 0x00E6_E6EA;
const BUTTON: u32 = 0x003A_3A44;
const BUTTON_HOVER: u32 = 0x004A_4A56;
const BUTTON_ACTIVE: u32 = 0x0058_5866;
/// The tray icon's colour for a dictation.
const DICTATING: u32 = 0x00AF_52DE;

/// Width of the Pause/Resume button in points.
const BUTTON_WIDTH: f32 = 72.0;

/// Height of the Pause/Resume button in points.
const BUTTON_HEIGHT: f32 = 28.0;

/// Distance from the card's top edge to the button.
const BUTTON_TOP: f32 = (TAKE_HEIGHT - BUTTON_HEIGHT) / 2.0;

/// Width of the elapsed-time field in points.
const TIMER_WIDTH: f32 = 72.0;

/// Distance between the card's edge and its content.
const CARD_PADDING: f32 = 10.0;

/// Distance between the card's children.
const ROW_GAP: f32 = 4.0;

/// The Pause/Resume button's rectangle in the card's coordinates.
fn button_rect() -> Bounds<Pixels> {
    Bounds {
        origin: point(px(TAKE_WIDTH - CARD_PADDING - BUTTON_WIDTH), px(BUTTON_TOP)),
        size: size(px(BUTTON_WIDTH), px(BUTTON_HEIGHT)),
    }
}

/// The HUD view drawn inside the overlay window.
pub struct HudView {
    state: HudState,
    shell: WeakEntity<Shell>,
}

impl HudView {
    /// Replaces the values the next frame draws.
    pub fn set_state(&mut self, state: HudState) {
        self.state = state;
    }

    fn take_card(&self) -> Div {
        let state = &self.state;
        let color = phase_color(state.phase);
        let shell = self.shell.clone();
        let button = state.pause_label().map(|label| {
            let shell = shell.clone();
            div()
                .id("hud-pause")
                .flex_shrink_0()
                .w(px(BUTTON_WIDTH))
                .h(px(BUTTON_HEIGHT))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(8.0))
                .bg(rgb(BUTTON))
                .text_size(px(12.0))
                .text_color(rgb(0x00FF_FFFF))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(BUTTON_HOVER)))
                .active(|style| style.bg(rgb(BUTTON_ACTIVE)))
                .child(label)
                .on_click(move |_, _, cx| {
                    if let Some(shell) = shell.upgrade() {
                        shell.update(cx, Shell::toggle_recording_pause);
                    }
                })
        });

        div()
            .size_full()
            .flex()
            .items_center()
            .gap(px(ROW_GAP))
            .px(px(CARD_PADDING))
            .overflow_hidden()
            .rounded(px(14.0))
            .bg(rgba(0x1616_1AEE))
            .child(
                div()
                    .size(px(8.0))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(rgb(color)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(11.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(color))
                    .child(badge(state)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(TIMER_WIDTH))
                    .text_right()
                    .font_family("monospace")
                    .text_size(px(15.0))
                    .text_color(rgb(0x00FF_FFFF))
                    .child(elapsed_text(state.elapsed)),
            )
            .children(button)
    }

    fn transcript_card(&self) -> Div {
        let state = &self.state;
        let color = phase_color(state.phase);
        // A refused press has no transcript, so its reason takes the
        // transcript box rather than a second line beside it.
        let (text, text_color) = state
            .warning
            .as_ref()
            .map_or((state.transcript.as_str(), TRANSCRIPT_TEXT), |warning| {
                (warning.as_str(), AMBER)
            });

        div()
            .size_full()
            .rounded(px(18.0))
            .bg(rgba(0x1616_1AEE))
            .relative()
            .child(
                div()
                    .absolute()
                    .left(px(EDGE_MARGIN))
                    .top(px(20.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().size(px(16.0)).rounded_full().bg(rgb(color)))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(color))
                            .child(badge(state)),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .right(px(EDGE_MARGIN))
                    .top(px(14.0))
                    .font_family("monospace")
                    .text_size(px(24.0))
                    .text_color(rgb(0x00FF_FFFF))
                    .child(elapsed_text(state.elapsed)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(EDGE_MARGIN))
                    .top(px(48.0))
                    .w(px(EDGE_MARGIN.mul_add(-2.0, HUD_WIDTH)))
                    .h(px(32.0))
                    .line_clamp(2)
                    .line_height(px(16.0))
                    .text_size(px(12.0))
                    .text_color(rgb(text_color))
                    .child(text.to_owned()),
            )
    }
}

/// Opens the HUD on the primary display, or returns `None` when the backend
/// reports no display.
pub fn open(
    cx: &mut App,
    state: HudState,
    shell: WeakEntity<Shell>,
) -> Option<(AnyWindowHandle, Entity<HudView>)> {
    let bounds = bounds(cx, &state)?;
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        focus: false,
        show: true,
        kind: WindowKind::PopUp,
        is_resizable: false,
        is_movable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        window_decorations: Some(WindowDecorations::Client),
        app_id: Some("koe-hud".to_owned()),
        ..WindowOptions::default()
    };
    let interactive = state.pause_label().is_some();
    let view = cx.new(|_| HudView { state, shell });
    let built = view.clone();
    let handle = cx.open_window(options, move |_, _| built).ok()?;
    let _ = handle.update(cx, |_, window, _| set_region(window, interactive));
    Some((handle.into(), view))
}

/// Closes the HUD window.
pub fn close(cx: &mut App, handle: AnyWindowHandle) {
    let _ = handle.update(cx, |_, window, _| window.remove_window());
}

/// Lets the overlay's button take pointer input, or makes clicks pass through.
///
/// The region is the button, not the card, so the rest of the card stays
/// click-through on Wayland.
pub fn set_interactive(cx: &mut App, handle: AnyWindowHandle, interactive: bool) {
    let _ = handle.update(cx, |_, window, _| set_region(window, interactive));
}

fn set_region(window: &mut Window, interactive: bool) {
    let empty: [Bounds<Pixels>; 0] = [];
    let button = [button_rect()];
    let region: &[Bounds<Pixels>] = if interactive { &button } else { &empty };
    window.set_input_region(Some(region));
}

const fn phase_color(phase: HudPhase) -> u32 {
    match phase {
        HudPhase::Starting | HudPhase::Paused | HudPhase::Refused => AMBER,
        HudPhase::Recording => 0x00FF_5A5F,
        HudPhase::Saving | HudPhase::Transcribing => 0x0058_A6FF,
        HudPhase::Listening => DICTATING,
    }
}

fn bounds(cx: &App, state: &HudState) -> Option<Bounds<Pixels>> {
    let display = cx.primary_display()?;
    Some(place(state.is_take(), display.visible_bounds()))
}

/// Where the overlay sits on `screen`: the take card in the top-right corner,
/// the transcript card at the bottom centre.
fn place(take: bool, screen: Bounds<Pixels>) -> Bounds<Pixels> {
    let (width, height) = if take {
        (px(TAKE_WIDTH), px(TAKE_HEIGHT))
    } else {
        (px(HUD_WIDTH), px(HUD_HEIGHT))
    };
    let origin = if take {
        point(
            screen.origin.x + screen.size.width - width - px(EDGE_MARGIN),
            screen.origin.y + px(EDGE_MARGIN),
        )
    } else {
        point(
            screen.origin.x + (screen.size.width - width) / 2.0,
            screen.origin.y + screen.size.height - height - px(EDGE_MARGIN),
        )
    };
    Bounds {
        origin,
        size: size(width, height),
    }
}

impl Render for HudView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        if self.state.is_take() {
            self.take_card()
        } else {
            self.transcript_card()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> Bounds<Pixels> {
        Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(1_920.0), px(1_080.0)),
        }
    }

    #[test]
    fn the_take_card_sits_in_the_top_right_corner() {
        assert_eq!(
            place(true, screen()),
            Bounds {
                origin: point(px(1_920.0 - TAKE_WIDTH - EDGE_MARGIN), px(EDGE_MARGIN)),
                size: size(px(TAKE_WIDTH), px(TAKE_HEIGHT)),
            }
        );
    }

    #[test]
    fn the_transcript_card_stays_at_the_bottom_centre() {
        assert_eq!(
            place(false, screen()),
            Bounds {
                origin: point(
                    px((1_920.0 - HUD_WIDTH) / 2.0),
                    px(1_080.0 - HUD_HEIGHT - EDGE_MARGIN),
                ),
                size: size(px(HUD_WIDTH), px(HUD_HEIGHT)),
            }
        );
    }

    #[test]
    fn a_secondary_origin_keeps_both_margins() {
        let screen = Bounds {
            origin: point(px(1_920.0), px(200.0)),
            size: size(px(1_280.0), px(1_024.0)),
        };
        let bounds = place(true, screen);
        assert_eq!(
            bounds.origin,
            point(
                px(1_920.0 + 1_280.0 - TAKE_WIDTH - EDGE_MARGIN),
                px(200.0 + EDGE_MARGIN)
            )
        );
    }

    #[test]
    fn the_interactive_region_is_the_button_alone() {
        let button = button_rect();
        assert_eq!(button.size, size(px(BUTTON_WIDTH), px(BUTTON_HEIGHT)));
        assert_eq!(
            button.origin.x + button.size.width,
            px(TAKE_WIDTH - CARD_PADDING)
        );
        assert_eq!(button.origin.y, px((TAKE_HEIGHT - BUTTON_HEIGHT) / 2.0));
        assert!(button.origin.x >= px(CARD_PADDING));
        assert!(button.origin.y + button.size.height <= px(TAKE_HEIGHT));
    }
}
