//! The HUD geometry, the state it draws, and the text it shares with the
//! window.
//!
//! Every HUD pixel comes from [`HudState`], so a take or a dictation in
//! progress never reacts to a click that was meant for the app underneath.

use std::time::{Duration, Instant};

use koe_capture::Source;

use crate::dictation::Dictation;
use crate::state::Phase;

/// Width of the HUD window in points.
pub const HUD_WIDTH: f32 = 340.0;

/// Height of the HUD window in points.
pub const HUD_HEIGHT: f32 = 88.0;

/// Distance the HUD keeps from each monitor edge.
pub const EDGE_MARGIN: f32 = 24.0;

/// Characters of dictation text the transcript box keeps, counted from the
/// end.
///
/// The box holds two lines at 12 points in 292 points of width, which is
/// about 48 full-width glyphs. Keeping more would cut the text at its end,
/// where the newest words are.
const TRANSCRIPT_LIMIT: usize = 44;

/// What the HUD is showing right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HudPhase {
    /// Devices are opening and the file is being created.
    Starting,
    /// Audio is being written.
    Recording,
    /// The file is being finalized.
    Saving,
    /// A dictation is capturing speech.
    Listening,
    /// A dictation's last words are being transcribed.
    Transcribing,
    /// A hotkey press was refused.
    Refused,
}

/// Everything the HUD draws for one frame.
#[derive(Clone, Debug, PartialEq)]
pub struct HudState {
    /// Stage the HUD is drawing.
    pub phase: HudPhase,
    /// Time since the take or the dictation started, in whole seconds.
    ///
    /// Whole seconds are what [`elapsed_text`] draws, so two ticks in the same
    /// whole second derive the same elapsed; a dictation whose text did not
    /// change is then not redrawn, while a take still redraws as its meters
    /// move.
    pub elapsed: Duration,
    /// Whether each source is capturing, in microphone then system order.
    ///
    /// Only a take draws these; a dictation has no per-source widget, so its
    /// values stay at their defaults.
    pub live: [bool; 2],
    /// Peak per source, in the same order, clamped to `0.0..=1.0` when drawn.
    ///
    /// Drawn only for a take, like [`Self::live`].
    pub levels: [f32; 2],
    /// A problem to tell the user about: a take's lost source, or the reason a
    /// dictation was refused.
    pub warning: Option<String>,
    /// The dictation's recognized text, already capped to what the box shows.
    pub transcript: String,
}

impl HudState {
    /// The HUD the overlay draws at `now`.
    ///
    /// What is already running outranks a refused press: a dictation, then a
    /// take, then the refusal. A refusal therefore never replaces a take's
    /// timer or a dictation's transcript; its sentence still reaches the user
    /// through the window notice and the tray tooltip, and the HUD draws it
    /// whenever nothing is running.
    #[must_use]
    pub fn for_frame(
        dictation: &Dictation,
        transcript: &str,
        take: &Phase,
        levels: [f32; 2],
        refusal: Option<&str>,
        now: Instant,
    ) -> Option<Self> {
        Self::dictation(dictation, transcript, now)
            .or_else(|| Self::take(take, levels, now))
            .or_else(|| refusal.map(Self::refused))
    }

    /// The HUD a meeting take draws, or `None` when no take is running.
    #[must_use]
    pub fn take(take: &Phase, levels: [f32; 2], now: Instant) -> Option<Self> {
        let phase = match take {
            Phase::Arming { .. } => HudPhase::Starting,
            Phase::Recording(_) => HudPhase::Recording,
            Phase::Stopping { .. } => HudPhase::Saving,
            Phase::Idle | Phase::Done(_) => return None,
        };
        let live = take.live();
        let elapsed = take.elapsed(now).unwrap_or_default();
        Some(Self {
            phase,
            elapsed: Duration::from_secs(elapsed.as_secs()),
            live: [
                live.contains(Source::Microphone),
                live.contains(Source::SystemAudio),
            ],
            levels,
            warning: take.warning().map(str::to_owned),
            transcript: String::new(),
        })
    }

    /// The HUD a running dictation draws, or `None` when none is running.
    ///
    /// `transcript` is the text the dictation thread has recognized so far.
    #[must_use]
    pub fn dictation(dictation: &Dictation, transcript: &str, now: Instant) -> Option<Self> {
        let phase = match dictation {
            Dictation::Listening { .. } => HudPhase::Listening,
            Dictation::Stopping { .. } => HudPhase::Transcribing,
            Dictation::Idle | Dictation::Done(_) => return None,
        };
        let elapsed = dictation.elapsed(now).unwrap_or_default();
        Some(Self {
            phase,
            elapsed: Duration::from_secs(elapsed.as_secs()),
            live: [false; 2],
            levels: [0.0; 2],
            warning: None,
            transcript: transcript_tail(transcript),
        })
    }

    /// The HUD for a hotkey press that was refused.
    #[must_use]
    pub fn refused(message: &str) -> Self {
        Self {
            phase: HudPhase::Refused,
            elapsed: Duration::ZERO,
            live: [false; 2],
            levels: [0.0; 2],
            warning: Some(message.to_owned()),
            transcript: String::new(),
        }
    }
}

/// The refusal sentence still worth drawing at `now`, or `None` once its hold
/// has run out.
#[must_use]
pub fn held_refusal(message: &str, until: Instant, now: Instant) -> Option<&str> {
    (now < until).then_some(message)
}

/// The status word shown beside the dot.
#[must_use]
pub const fn badge(state: &HudState) -> &'static str {
    match state.phase {
        HudPhase::Starting => "STARTING",
        HudPhase::Recording => "REC",
        HudPhase::Saving => "SAVING",
        HudPhase::Listening => "DICTATING",
        HudPhase::Transcribing => "TRANSCRIBING",
        HudPhase::Refused => "REFUSED",
    }
}

/// The elapsed time as `MM:SS`, or `H:MM:SS` once it reaches an hour.
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

/// The trailing part of `text` the transcript box can show.
///
/// A dropped start is marked with an ellipsis, so the text reads as a
/// continuation rather than as the beginning of a sentence. The cut lands on
/// a `char` boundary, so the result is valid UTF-8, but it can begin inside a
/// grapheme cluster — a dangling combining accent, or part of an emoji ZWJ
/// sequence — because the box is measured in code points, not clusters.
#[must_use]
fn transcript_tail(text: &str) -> String {
    let text = text.trim();
    let count = text.chars().count();
    if count <= TRANSCRIPT_LIMIT {
        return text.to_owned();
    }
    let start = text
        .char_indices()
        .nth(count - TRANSCRIPT_LIMIT)
        .map_or(0, |(index, _)| index);
    format!("…{}", text[start..].trim_start())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;
    use crate::dictation::DictationDone;
    use crate::state::{Done, Live, Sources, StopCause};

    fn hud(phase: HudPhase) -> HudState {
        HudState {
            phase,
            elapsed: Duration::ZERO,
            live: [true, false],
            levels: [0.0, 0.0],
            warning: None,
            transcript: String::new(),
        }
    }

    fn listening(started: Instant) -> Dictation {
        Dictation::Listening { started }
    }

    #[test]
    fn each_phase_has_its_own_badge() {
        assert_eq!(badge(&hud(HudPhase::Starting)), "STARTING");
        assert_eq!(badge(&hud(HudPhase::Recording)), "REC");
        assert_eq!(badge(&hud(HudPhase::Saving)), "SAVING");
        assert_eq!(badge(&hud(HudPhase::Listening)), "DICTATING");
        assert_eq!(badge(&hud(HudPhase::Transcribing)), "TRANSCRIBING");
        assert_eq!(badge(&hud(HudPhase::Refused)), "REFUSED");
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

    #[test]
    fn a_take_maps_each_active_phase_and_draws_no_transcript() {
        let now = Instant::now();
        let live = Live::new(now);

        let arming = Phase::Arming {
            live: live.clone(),
            pending: Sources::MICROPHONE,
        };
        assert_eq!(
            HudState::take(&arming, [0.0; 2], now).unwrap().phase,
            HudPhase::Starting
        );

        let recording = Phase::Recording(Live {
            live: Sources::MICROPHONE,
            ..live.clone()
        });
        let state = HudState::take(&recording, [0.5, 0.0], now).unwrap();
        assert_eq!(state.phase, HudPhase::Recording);
        assert_eq!(state.live, [true, false]);
        assert_eq!(state.levels, [0.5, 0.0]);
        assert_eq!(state.transcript, "");

        let stopping = Phase::Stopping {
            live,
            cause: StopCause::User,
        };
        assert_eq!(
            HudState::take(&stopping, [0.0; 2], now).unwrap().phase,
            HudPhase::Saving
        );
    }

    #[test]
    fn an_idle_or_finished_take_closes_the_hud() {
        let now = Instant::now();
        assert!(HudState::take(&Phase::Idle, [0.0; 2], now).is_none());
        let done = Phase::Done(Box::new(Done {
            output: "take.ogg".into(),
            summary: None,
            failure: None,
            duration: Duration::from_secs(1),
            warning: None,
        }));
        assert!(HudState::take(&done, [0.0; 2], now).is_none());
    }

    #[test]
    fn a_listening_dictation_shows_its_transcript_and_elapsed_time() {
        let now = Instant::now();
        let dictation = listening(now);
        let state =
            HudState::dictation(&dictation, "hello wor", now + Duration::from_secs(2)).unwrap();
        assert_eq!(state.phase, HudPhase::Listening);
        assert_eq!(state.elapsed, Duration::from_secs(2));
        assert_eq!(state.transcript, "hello wor");
        assert_eq!(state.warning, None);
    }

    #[test]
    fn a_stopping_dictation_says_it_is_transcribing() {
        let now = Instant::now();
        let dictation = Dictation::Stopping { started: now };
        let state = HudState::dictation(&dictation, "hello", now).unwrap();
        assert_eq!(state.phase, HudPhase::Transcribing);
        assert_eq!(badge(&state), "TRANSCRIBING");
    }

    #[test]
    fn an_idle_or_finished_dictation_closes_the_hud() {
        let now = Instant::now();
        assert!(HudState::dictation(&Dictation::Idle, "", now).is_none());
        let done = Dictation::Done(Box::new(DictationDone {
            text: "hi".to_owned(),
            failure: None,
            duration: Duration::from_secs(1),
        }));
        assert!(HudState::dictation(&done, "hi", now).is_none());
    }

    #[test]
    fn ticks_inside_one_second_draw_the_same_state() {
        let now = Instant::now();
        let dictation = listening(now);
        let first = HudState::dictation(&dictation, "hello", now).unwrap();
        let second =
            HudState::dictation(&dictation, "hello", now + Duration::from_millis(400)).unwrap();
        assert_eq!(first, second);
        let grown = HudState::dictation(&dictation, "hello world", now).unwrap();
        assert_ne!(first, grown);
        let next = HudState::dictation(&dictation, "hello", now + Duration::from_secs(1)).unwrap();
        assert_ne!(first, next);
    }

    #[test]
    fn a_refusal_is_held_until_its_deadline() {
        let now = Instant::now();
        let until = now + Duration::from_secs(5);
        assert_eq!(held_refusal("Refused.", until, now), Some("Refused."));
        assert_eq!(held_refusal("Refused.", until, until), None);
        assert_eq!(
            held_refusal("Refused.", until, until + Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn a_refusal_never_displaces_a_running_capture() {
        let now = Instant::now();
        let recording = Phase::Recording(Live::new(now));
        let later = now + Duration::from_secs(3);
        let state = HudState::for_frame(
            &Dictation::Idle,
            "",
            &recording,
            [0.5, 0.0],
            Some("A meeting recording owns the microphone."),
            later,
        )
        .unwrap();
        assert_eq!(state.phase, HudPhase::Recording);
        assert_eq!(state.elapsed, Duration::from_secs(3));

        let dictating = listening(now);
        let state = HudState::for_frame(
            &dictating,
            "hello",
            &recording,
            [0.5, 0.0],
            Some("A meeting recording owns the microphone."),
            later,
        )
        .unwrap();
        assert_eq!(state.phase, HudPhase::Listening);
        assert_eq!(state.transcript, "hello");
    }

    #[test]
    fn a_refusal_draws_while_nothing_is_running() {
        let now = Instant::now();
        let idle = Phase::Idle;
        let state =
            HudState::for_frame(&Dictation::Idle, "", &idle, [0.0; 2], Some("Refused."), now)
                .unwrap();
        assert_eq!(state.phase, HudPhase::Refused);
        assert!(HudState::for_frame(&Dictation::Idle, "", &idle, [0.0; 2], None, now).is_none());
    }

    #[test]
    fn a_refused_press_shows_its_reason_in_place_of_a_transcript() {
        let state = HudState::refused("Dictation needs a transcription model.");
        assert_eq!(state.phase, HudPhase::Refused);
        assert_eq!(badge(&state), "REFUSED");
        assert_eq!(
            state.warning.as_deref(),
            Some("Dictation needs a transcription model.")
        );
        assert_eq!(state.transcript, "");
    }

    #[test]
    fn a_short_transcript_is_shown_whole() {
        assert_eq!(transcript_tail("こんにちは"), "こんにちは");
        assert_eq!(transcript_tail("  hello  "), "hello");
        assert_eq!(transcript_tail(""), "");
        assert_eq!(transcript_tail("   "), "");
    }

    #[test]
    fn a_long_transcript_keeps_its_last_words() {
        let text = "あ".repeat(TRANSCRIPT_LIMIT * 2);
        let tail = transcript_tail(&text);
        let kept = tail.strip_prefix('…').unwrap();
        assert_eq!(kept.chars().count(), TRANSCRIPT_LIMIT);
        assert!(kept.chars().all(|glyph| glyph == 'あ'));
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        let text = "あ".repeat(TRANSCRIPT_LIMIT + 1);
        let tail = transcript_tail(&text);
        assert_eq!(tail, format!("…{}", "あ".repeat(TRANSCRIPT_LIMIT)));
    }
}
