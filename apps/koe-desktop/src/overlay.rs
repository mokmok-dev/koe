//! The always-on-top overlay window that shows what a take is doing.

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, FontWeight, IntoElement,
    ParentElement, Pixels, Render, Styled as _, Window, WindowBackgroundAppearance, WindowBounds,
    WindowDecorations, WindowKind, WindowOptions, div, point, px, relative, rgb, rgba, size,
};

use crate::hud::{EDGE_MARGIN, HUD_HEIGHT, HUD_WIDTH, HudPhase, HudState, badge, elapsed_text};

const TRACK: u32 = 0x002C_2C34;
const LIVE: u32 = 0x007E_E787;
const IDLE: u32 = 0x005A_5A62;
const AMBER: u32 = 0x00F0_B429;

/// The HUD view drawn inside the overlay window.
pub struct HudView {
    state: HudState,
}

impl HudView {
    /// Replaces the values the next frame draws.
    pub fn set_state(&mut self, state: HudState) {
        self.state = state;
    }
}

/// Opens the HUD on the primary display, or returns `None` when the backend
/// reports no display.
pub fn open(cx: &mut App, state: HudState) -> Option<(AnyWindowHandle, Entity<HudView>)> {
    let bounds = bounds(cx)?;
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
    let view = cx.new(|_| HudView { state });
    let built = view.clone();
    let handle = cx.open_window(options, move |_, _| built).ok()?;
    // Wayland honours an empty input region; X11 and macOS ignore it.
    let _ = handle.update(cx, |_, window, _| {
        let empty: [Bounds<Pixels>; 0] = [];
        window.set_input_region(Some(&empty));
    });
    Some((handle.into(), view))
}

/// Closes the HUD window.
pub fn close(cx: &mut App, handle: AnyWindowHandle) {
    let _ = handle.update(cx, |_, window, _| window.remove_window());
}

const fn phase_color(phase: HudPhase) -> u32 {
    match phase {
        HudPhase::Starting => AMBER,
        HudPhase::Recording => 0x00FF_5A5F,
        HudPhase::Saving => 0x0058_A6FF,
    }
}

fn bounds(cx: &App) -> Option<Bounds<Pixels>> {
    let display = cx.primary_display()?;
    let screen = display.visible_bounds();
    let width = px(HUD_WIDTH);
    let height = px(HUD_HEIGHT);
    let x = screen.origin.x + (screen.size.width - width) / 2.0;
    let y = screen.origin.y + screen.size.height - height - px(EDGE_MARGIN);
    Some(Bounds {
        origin: point(x, y),
        size: size(width, height),
    })
}

impl Render for HudView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let state = &self.state;
        let color = phase_color(state.phase);
        let level = |index: usize| {
            let value = state.levels[index];
            // A device that fails mid-take can report NaN; draw it as silence.
            if value.is_finite() {
                value.clamp(0.0, 1.0)
            } else {
                0.0
            }
        };
        let meter = |index: usize, top: f32| {
            div()
                .absolute()
                .left(px(EDGE_MARGIN))
                .top(px(top))
                .w(px(EDGE_MARGIN.mul_add(-2.0, HUD_WIDTH)))
                .h(px(6.0))
                .rounded(px(3.0))
                .bg(rgb(TRACK))
                .child(
                    div()
                        .h_full()
                        .w(relative(level(index)))
                        .rounded(px(3.0))
                        .bg(rgb(if state.live[index] { LIVE } else { IDLE })),
                )
        };

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
            .child(meter(0, 48.0))
            .child(meter(1, 58.0))
            .children(state.warning.as_ref().map(|warning| {
                div()
                    .absolute()
                    .left(px(EDGE_MARGIN))
                    .bottom(px(6.0))
                    .w(px(EDGE_MARGIN.mul_add(-2.0, HUD_WIDTH)))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(11.0))
                    .text_color(rgb(AMBER))
                    .child(warning.clone())
            }))
    }
}
