//! The HUD geometry, the take state it draws, and the text it shares with the
//! window.
//!
//! Every HUD pixel comes from [`HudState`], so a take in progress never
//! reacts to a click that was meant for the app underneath.

/// Width of the HUD window in points.
pub const HUD_WIDTH: f32 = 340.0;

/// Height of the HUD window in points.
pub const HUD_HEIGHT: f32 = 88.0;

/// Distance the HUD keeps from each monitor edge.
pub const EDGE_MARGIN: f32 = 24.0;

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
#[derive(Clone, Debug, PartialEq)]
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
