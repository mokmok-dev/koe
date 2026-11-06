//! The Rune overlay that shows what a take is doing.

use std::time::Duration;

use rune::tok::{self, type_};
use rune::{Color, Cx, El, TextStyle, col, el, row, text};

const CARD: Color = Color {
    r: 0.086,
    g: 0.086,
    b: 0.102,
    a: 0.95,
};
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

/// What the take is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HudPhase {
    /// Devices are opening.
    Starting,
    /// Audio is being written.
    Recording,
    /// The file is closing.
    Saving,
}

/// Everything the HUD reads for one frame.
#[derive(Clone, Debug, PartialEq)]
pub struct HudState {
    /// Stage of the take.
    pub phase: HudPhase,
    /// Time since the take started.
    pub elapsed: Duration,
    /// Whether each source is capturing, microphone first.
    pub live: [bool; 2],
    /// Peak per source, clamped when drawn.
    pub levels: [f32; 2],
    /// A problem worth showing while the take continues.
    pub warning: Option<String>,
}

impl Default for HudState {
    fn default() -> Self {
        Self {
            phase: HudPhase::Starting,
            elapsed: Duration::ZERO,
            live: [false; 2],
            levels: [0.0; 2],
            warning: None,
        }
    }
}

/// The status word beside the dot.
#[must_use]
pub const fn badge(phase: HudPhase) -> &'static str {
    match phase {
        HudPhase::Starting => "STARTING",
        HudPhase::Recording => "REC",
        HudPhase::Saving => "SAVING",
    }
}

/// The elapsed time as `MM:SS`, or `H:MM:SS` from an hour.
#[must_use]
pub fn elapsed_text(elapsed: Duration) -> String {
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

/// Draws the HUD in its overlay.
#[must_use]
pub fn view(state: &HudState, _: &mut Cx<HudState>) -> El {
    let theme = tok::theme(true);
    let colour = phase_colour(state.phase);
    let mut bars = col().w_full().gap(3.0);
    for (live, level) in state.live.iter().zip(state.levels.iter()) {
        let width = (level.clamp(0.0, 1.0) * 280.0).max(0.0);
        bars = bars.child(
            row()
                .w_full()
                .h(5.0)
                .radius(3.0)
                .fill(theme.control_track)
                .child(el().w(width).h(5.0).radius(3.0).fill(if *live {
                    GREEN
                } else {
                    theme.control_off
                })),
        );
    }
    let mut card = col()
        .w_full()
        .h_full()
        .padding(14.0)
        .gap(6.0)
        .radius(18.0)
        .fill(CARD)
        .child(
            row()
                .w_full()
                .items_center()
                .gap(8.0)
                .child(el().size(10.0, 10.0).radius(5.0).fill(colour))
                .child(text(badge(state.phase)).font(type_::LABEL).color(colour))
                .child(el().grow(1.0))
                .child(
                    text(elapsed_text(state.elapsed))
                        .font(type_::TITLE)
                        .color(Color::WHITE),
                ),
        )
        .child(bars);
    if let Some(warning) = &state.warning {
        card = card.child(
            text(warning.as_str())
                .font(TextStyle::new(10.0, 400.0))
                .color(AMBER)
                .truncate(),
        );
    }
    col().w_full().h_full().child(card)
}

const fn phase_colour(phase: HudPhase) -> Color {
    match phase {
        HudPhase::Starting => AMBER,
        HudPhase::Recording => RED,
        HudPhase::Saving => BLUE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_phase_has_its_own_badge() {
        assert_eq!(badge(HudPhase::Starting), "STARTING");
        assert_eq!(badge(HudPhase::Recording), "REC");
        assert_eq!(badge(HudPhase::Saving), "SAVING");
    }

    #[test]
    fn time_switches_to_hours_at_an_hour() {
        assert_eq!(elapsed_text(Duration::ZERO), "00:00");
        assert_eq!(elapsed_text(Duration::from_secs(61)), "01:01");
        assert_eq!(elapsed_text(Duration::from_secs(3_599)), "59:59");
        assert_eq!(elapsed_text(Duration::from_secs(3_600)), "1:00:00");
    }
}
