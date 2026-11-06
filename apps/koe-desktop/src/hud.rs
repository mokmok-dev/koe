//! The always-on-top overlay that shows what a take is doing.
//!
//! The HUD draws no widgets: every pixel is painted from [`HudState`], so a
//! take in progress never reacts to a click that was meant for the app
//! underneath.

use egui::{
    Align2, Color32, FontFamily, FontId, Pos2, Rect, Vec2, ViewportBuilder, ViewportId, pos2, vec2,
};

/// Size of the HUD window in points.
pub const HUD_SIZE: egui::Vec2 = egui::vec2(340.0, 88.0);

/// Distance the HUD keeps from each monitor edge.
const EDGE_MARGIN: f32 = 24.0;

/// Monitor size used when the backend reports none.
const FALLBACK_MONITOR: Vec2 = vec2(1280.0, 800.0);

const TRACK: Color32 = Color32::from_rgb(44, 44, 52);
const LIVE: Color32 = Color32::from_rgb(126, 231, 135);
const IDLE: Color32 = Color32::from_rgb(90, 90, 98);
const AMBER: Color32 = Color32::from_rgb(240, 180, 41);

/// What the take is doing right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HudPhase {
    /// Devices are opening and the file is being created.
    Starting,
    /// Audio is being written.
    Recording,
    /// The file is being finalized.
    Saving,
}

/// Everything the HUD draws for one frame.
#[derive(Clone, Debug)]
pub struct HudState {
    /// Stage of the take.
    pub phase: HudPhase,
    /// Time since the take started.
    pub elapsed: std::time::Duration,
    /// Whether each source is capturing, in microphone then system order.
    pub live: [bool; 2],
    /// Peak per source, in the same order, clamped to `0.0..=1.0` when drawn.
    pub levels: [f32; 2],
    /// A problem worth telling the user about while the take continues.
    pub warning: Option<String>,
}

/// The status word shown beside the dot.
#[must_use]
pub const fn badge(state: &HudState) -> &'static str {
    match state.phase {
        HudPhase::Starting => "STARTING",
        HudPhase::Recording => "REC",
        HudPhase::Saving => "SAVING",
    }
}

/// The elapsed time as `MM:SS`, or `H:MM:SS` once it reaches an hour.
#[must_use]
pub fn elapsed_text(elapsed: std::time::Duration) -> String {
    let total = elapsed.as_secs();
    let hours = total / 3_600;
    let minutes = total % 3_600 / 60;
    let seconds = total % 60;
    if hours == 0 {
        format!("{minutes:02}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}")
    }
}

/// Shows the HUD, or updates the HUD that is already showing.
pub fn draw(ctx: &egui::Context, state: &HudState) {
    let monitor = ctx
        .input(|i| i.viewport().monitor_size)
        .unwrap_or(FALLBACK_MONITOR);
    let builder = ViewportBuilder::default()
        .with_title("koe")
        .with_inner_size(HUD_SIZE)
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top()
        .with_resizable(false)
        .with_taskbar(false)
        .with_mouse_passthrough(true)
        .with_has_shadow(false)
        .with_position(bottom_center(monitor));
    ctx.show_viewport_immediate(
        ViewportId::from_hash_of("koe-hud"),
        builder,
        |ui, _class| paint(ui, state),
    );
}

/// Paints one frame of the HUD into an immediate viewport.
pub fn paint(ui: &mut egui::Ui, state: &HudState) {
    let card = ui.max_rect();
    let painter = ui.painter().clone();
    painter.rect_filled(card, 18.0, card_fill());

    let color = phase_color(state.phase);
    let dot = pos2(card.left() + EDGE_MARGIN, card.top() + 28.0);
    painter.circle_filled(dot, 8.0, color);
    painter.text(
        pos2(dot.x + 16.0, dot.y),
        Align2::LEFT_CENTER,
        badge(state),
        FontId::new(12.0, FontFamily::Proportional),
        color,
    );
    painter.text(
        pos2(card.right() - EDGE_MARGIN, dot.y),
        Align2::RIGHT_CENTER,
        elapsed_text(state.elapsed),
        FontId::new(24.0, FontFamily::Monospace),
        Color32::WHITE,
    );

    let track_left = card.left() + EDGE_MARGIN;
    let track_width = (card.width() - EDGE_MARGIN - EDGE_MARGIN).max(0.0);
    let mut top = card.top() + 48.0;
    for (live, level) in state.live.iter().zip(state.levels.iter()) {
        let track = Rect::from_min_size(pos2(track_left, top), vec2(track_width, 6.0));
        painter.rect_filled(track, 3.0, TRACK);
        // A device that fails mid-take can report NaN; draw it as silence.
        let level = if level.is_finite() {
            level.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let fill = Rect::from_min_size(track.min, vec2(track_width * level, 6.0));
        painter.rect_filled(fill, 3.0, if *live { LIVE } else { IDLE });
        top += 10.0;
    }

    if let Some(warning) = &state.warning {
        let font = FontId::new(11.0, FontFamily::Proportional);
        let text = shortened(&painter, warning, &font, track_width);
        painter.text(
            pos2(card.left() + EDGE_MARGIN, card.bottom() - 16.0),
            Align2::LEFT_CENTER,
            text,
            font,
            AMBER,
        );
    }
}

/// Card fill, translucent so the desktop shows through.
fn card_fill() -> Color32 {
    Color32::from_rgba_unmultiplied(22, 22, 26, 238)
}

const fn phase_color(phase: HudPhase) -> Color32 {
    match phase {
        HudPhase::Starting => AMBER,
        HudPhase::Recording => Color32::from_rgb(255, 90, 95),
        HudPhase::Saving => Color32::from_rgb(88, 166, 255),
    }
}

fn bottom_center(monitor: Vec2) -> Pos2 {
    let x = (monitor.x - HUD_SIZE.x) * 0.5;
    let y = monitor.y - HUD_SIZE.y;
    let limit = monitor - HUD_SIZE - Vec2::splat(EDGE_MARGIN);
    pos2(
        x.clamp(EDGE_MARGIN, limit.x.max(EDGE_MARGIN)),
        y.clamp(EDGE_MARGIN, limit.y.max(EDGE_MARGIN)),
    )
}

/// Cuts `text` to `width`, ending an overlong line with an ellipsis.
fn shortened(painter: &egui::Painter, text: &str, font: &FontId, width: f32) -> String {
    if width <= 0.0 {
        return String::new();
    }
    let measure = |candidate: String| {
        painter.fonts_mut(|fonts| {
            fonts
                .layout_no_wrap(candidate, font.clone(), Color32::WHITE)
                .size()
                .x
        })
    };
    if measure(text.to_owned()) <= width {
        return text.to_owned();
    }

    let chars: Vec<char> = text.chars().collect();
    let mut low = 0;
    let mut high = chars.len();
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        let mut candidate: String = chars[..mid].iter().collect();
        candidate.push('…');
        if measure(candidate) <= width {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let mut result: String = chars[..low].iter().collect();
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn hud(phase: HudPhase) -> HudState {
        HudState {
            phase,
            elapsed: Duration::ZERO,
            live: [true, false],
            levels: [0.0, 0.0],
            warning: None,
        }
    }

    #[test]
    fn each_phase_has_its_own_badge() {
        assert_eq!(badge(&hud(HudPhase::Starting)), "STARTING");
        assert_eq!(badge(&hud(HudPhase::Recording)), "REC");
        assert_eq!(badge(&hud(HudPhase::Saving)), "SAVING");
    }

    #[test]
    fn times_under_an_hour_show_minutes_and_seconds() {
        assert_eq!(elapsed_text(Duration::from_secs(0)), "00:00");
        assert_eq!(elapsed_text(Duration::from_secs(59)), "00:59");
        assert_eq!(elapsed_text(Duration::from_secs(61)), "01:01");
        assert_eq!(elapsed_text(Duration::from_secs(3_599)), "59:59");
        assert_eq!(elapsed_text(Duration::from_millis(3_599_999)), "59:59");
    }

    #[test]
    fn an_hour_or_more_shows_hours() {
        assert_eq!(elapsed_text(Duration::from_secs(3_600)), "1:00:00");
        assert_eq!(elapsed_text(Duration::from_secs(3_661)), "1:01:01");
        assert_eq!(elapsed_text(Duration::from_secs(86_399)), "23:59:59");
        assert_eq!(elapsed_text(Duration::from_hours(100)), "100:00:00");
    }
}
