//! The dictation state machine.
//!
//! Dictation and a meeting take share one microphone, so they refuse each
//! other explicitly: [`Dictation::toggle`] refuses while a meeting take is
//! active, and [`meeting_start_allowed`] refuses a meeting start while a
//! dictation runs. The type is pure, so both rules run in tests without a
//! microphone, a model, or a window.

use std::time::{Duration, Instant};

use koe_transcribe::Model;

use crate::error::DesktopError;

/// A dictation was refused because a meeting take holds the microphone.
pub const MEETING_ACTIVE: &str =
    "A meeting recording owns the microphone. Stop it before dictating.";

/// A dictation was refused because no transcription model is configured.
///
/// The settings live in a file both shells read, so the sentence names the
/// file rather than a window only one shell has.
pub const MODEL_MISSING: &str = "Dictation needs a transcription model. Set one in the settings file or in the window's Dictation section.";

/// A meeting start was refused because a dictation holds the microphone.
pub const MEETING_BLOCKED: &str =
    "Dictation owns the microphone. Stop it before recording a meeting.";

/// What the app can currently transcribe, asked at every hotkey press.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DictationContext {
    /// Whether a meeting take is starting, recording, or saving.
    pub meeting_active: bool,
    /// The model a dictation would run with, if the platform has one.
    pub model: Option<Model>,
    /// The BCP 47 language a dictation would request, for example `ja`.
    pub language: Option<Box<str>>,
}

/// What one dictation transcribes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DictationPlan {
    /// The model the dictation loads.
    pub model: Model,
    /// The language the dictation requests, or the engine's choice.
    pub language: Option<Box<str>>,
}

/// What the dictation thread reports when it ends.
#[derive(Debug)]
pub struct DictationOutcome {
    /// The committed text, which the thread already tried to type.
    pub text: String,
    /// Why the text could not be typed, when it could not.
    pub failure: Option<DesktopError>,
}

/// A finished dictation.
#[derive(Debug)]
pub struct DictationDone {
    /// The text that was typed, or meant to be.
    pub text: String,
    /// Why the text could not be typed, when it could not.
    pub failure: Option<DesktopError>,
    /// Time the dictation ran for.
    pub duration: Duration,
}

impl DictationDone {
    /// Whether the dictation ended with an error or typed nothing.
    #[must_use]
    pub const fn failed(&self) -> bool {
        self.failure.is_some() || self.text.is_empty()
    }
}

/// Where the dictation is.
#[derive(Debug, Default)]
pub enum Dictation {
    /// No dictation.
    #[default]
    Idle,
    /// The microphone is being captured.
    Listening {
        /// When the dictation began, which starts its elapsed clock.
        started: Instant,
    },
    /// Capture stopped and the transcript is being finished.
    Stopping {
        /// When the dictation began.
        started: Instant,
    },
    /// The dictation ended.
    Done(Box<DictationDone>),
}

/// Something that happened to the dictation.
#[derive(Debug)]
pub enum DictationEvent {
    /// The dictation thread reported its outcome.
    Finished(Box<DictationOutcome>),
    /// The user cleared the result.
    Dismiss,
}

/// Work the caller must run after a transition.
#[derive(Debug)]
pub enum DictationAction {
    /// Start the dictation thread with this plan.
    Start(DictationPlan),
    /// Ask the dictation thread to stop; the transcript is still typed.
    Stop,
    /// Ask the dictation thread to stop and discard; nothing is typed.
    Cancel,
    /// Nothing changed, but the user should see why.
    Refuse(&'static str),
}

impl Dictation {
    /// Whether a dictation is capturing or transcribing, and a meeting
    /// take must not begin.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Listening { .. } | Self::Stopping { .. })
    }

    /// Time the dictation has run for.
    #[must_use]
    pub fn elapsed(&self, now: Instant) -> Option<Duration> {
        match self {
            Self::Idle => None,
            Self::Listening { started } | Self::Stopping { started } => {
                Some(now.saturating_duration_since(*started))
            }
            Self::Done(done) => Some(done.duration),
        }
    }

    /// Applies a hotkey press, given what the app can currently transcribe.
    ///
    /// No refusal is silent: it becomes [`DictationAction::Refuse`] with the
    /// sentence to show, and the phase keeps its previous value.
    pub fn toggle(&mut self, context: DictationContext, now: Instant) -> Option<DictationAction> {
        let current = std::mem::take(self);
        let (next, action) = match (current, context) {
            (Self::Idle, context) => match start(context) {
                Ok(plan) => (
                    Self::Listening { started: now },
                    Some(DictationAction::Start(plan)),
                ),
                Err(message) => (Self::Idle, Some(DictationAction::Refuse(message))),
            },
            (Self::Done(done), context) => match start(context) {
                Ok(plan) => (
                    Self::Listening { started: now },
                    Some(DictationAction::Start(plan)),
                ),
                Err(message) => (Self::Done(done), Some(DictationAction::Refuse(message))),
            },
            (Self::Listening { started }, _) => {
                (Self::Stopping { started }, Some(DictationAction::Stop))
            }
            (Self::Stopping { started }, _) => (Self::Stopping { started }, None),
        };
        *self = next;
        action
    }

    /// Applies an event the dictation thread or the window reports.
    ///
    /// An event that does not apply leaves the phase unchanged.
    pub fn reduce(&mut self, event: DictationEvent, now: Instant) -> Option<DictationAction> {
        let current = std::mem::take(self);
        let (next, action) = match (current, event) {
            (
                Self::Listening { started } | Self::Stopping { started },
                DictationEvent::Finished(outcome),
            ) => {
                let done = DictationDone {
                    duration: now.saturating_duration_since(started),
                    text: outcome.text,
                    failure: outcome.failure,
                };
                (Self::Done(Box::new(done)), None)
            }
            (Self::Done(_), DictationEvent::Dismiss) => (Self::Idle, None),
            (phase, _) => (phase, None),
        };
        *self = next;
        action
    }

    /// Discards the dictation without typing its text.
    ///
    /// A misfire is correctable: the take ends, nothing is typed, and the
    /// phase goes back to idle. A dictation that already ended stays as it
    /// was.
    pub fn cancel(&mut self) -> Option<DictationAction> {
        let current = std::mem::take(self);
        let (next, action) = match current {
            Self::Listening { .. } | Self::Stopping { .. } => {
                (Self::Idle, Some(DictationAction::Cancel))
            }
            phase => (phase, None),
        };
        *self = next;
        action
    }
}

/// Whether a meeting take may begin, and the sentence to show when it may not.
///
/// # Errors
///
/// Returns [`MEETING_BLOCKED`] while a dictation holds the microphone.
pub const fn meeting_start_allowed(dictation: &Dictation) -> Result<(), &'static str> {
    if dictation.is_active() {
        Err(MEETING_BLOCKED)
    } else {
        Ok(())
    }
}

/// The plan a hotkey press asks for, or the refusal sentence.
///
/// # Errors
///
/// Returns [`MEETING_ACTIVE`] while a meeting take runs, and
/// [`MODEL_MISSING`] when the platform has no transcription model.
fn start(context: DictationContext) -> Result<DictationPlan, &'static str> {
    if context.meeting_active {
        return Err(MEETING_ACTIVE);
    }
    let Some(model) = context.model else {
        return Err(MODEL_MISSING);
    };
    Ok(DictationPlan {
        model,
        language: context.language,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::unnecessary_box_returns)]

    use std::time::Duration;

    use super::*;

    fn base() -> Instant {
        Instant::now()
    }

    fn context(model: Option<Model>, meeting_active: bool) -> DictationContext {
        DictationContext {
            meeting_active,
            model,
            language: Some("ja".into()),
        }
    }

    fn configured() -> DictationContext {
        context(Some(Model::gguf("tiny.gguf")), false)
    }

    fn listening() -> Dictation {
        listening_at(base())
    }

    fn listening_at(now: Instant) -> Dictation {
        let mut dictation = Dictation::Idle;
        dictation.toggle(configured(), now);
        dictation
    }

    fn stopping(now: Instant) -> Dictation {
        let mut dictation = listening_at(now);
        dictation.toggle(configured(), now);
        dictation
    }

    fn outcome(text: &str, failure: Option<DesktopError>) -> Box<DictationOutcome> {
        Box::new(DictationOutcome {
            text: text.to_owned(),
            failure,
        })
    }

    fn done() -> Dictation {
        let mut dictation = listening();
        dictation.reduce(DictationEvent::Finished(outcome("hi", None)), base());
        dictation
    }

    #[test]
    fn a_press_with_a_model_starts_listening() {
        let mut dictation = Dictation::Idle;
        let DictationAction::Start(plan) = dictation.toggle(configured(), base()).unwrap() else {
            unreachable!("a configured context starts a dictation");
        };
        assert_eq!(plan.model, Model::gguf("tiny.gguf"));
        assert_eq!(plan.language.as_deref(), Some("ja"));
        assert!(matches!(dictation, Dictation::Listening { .. }));
        assert!(dictation.is_active());
    }

    #[test]
    fn a_second_press_stops_the_take() {
        let now = base();
        let mut dictation = listening_at(now);
        assert!(matches!(
            dictation.toggle(configured(), now),
            Some(DictationAction::Stop)
        ));
        assert!(matches!(dictation, Dictation::Stopping { .. }));
        assert!(dictation.is_active());
    }

    #[test]
    fn a_press_while_transcribing_is_ignored() {
        let now = base();
        let mut dictation = stopping(now);
        assert!(dictation.toggle(configured(), now).is_none());
        assert!(matches!(dictation, Dictation::Stopping { .. }));
    }

    #[test]
    fn a_cancel_discards_a_listening_dictation() {
        let mut dictation = listening();
        assert!(matches!(dictation.cancel(), Some(DictationAction::Cancel)));
        assert!(matches!(dictation, Dictation::Idle));
        assert!(!dictation.is_active());
    }

    #[test]
    fn a_cancel_discards_a_transcribing_dictation() {
        let now = base();
        let mut dictation = stopping(now);
        assert!(matches!(dictation.cancel(), Some(DictationAction::Cancel)));
        assert!(matches!(dictation, Dictation::Idle));
    }

    #[test]
    fn a_cancel_on_an_idle_or_done_dictation_does_nothing() {
        let mut dictation = Dictation::Idle;
        assert!(dictation.cancel().is_none());
        assert!(matches!(dictation, Dictation::Idle));

        let mut dictation = done();
        assert!(dictation.cancel().is_none());
        assert!(matches!(&dictation, Dictation::Done(done) if done.text == "hi"));
    }

    #[test]
    fn a_discarded_dictation_can_start_again() {
        let mut dictation = listening();
        dictation.cancel();
        assert!(matches!(
            dictation.toggle(configured(), base()),
            Some(DictationAction::Start(_))
        ));
    }

    #[test]
    fn a_press_refused_by_a_meeting_keeps_the_idle_phase() {
        let mut dictation = Dictation::Idle;
        let action = dictation
            .toggle(context(Some(Model::gguf("tiny.gguf")), true), base())
            .unwrap();
        assert!(matches!(action, DictationAction::Refuse(MEETING_ACTIVE)));
        assert!(matches!(dictation, Dictation::Idle));
    }

    #[test]
    fn a_press_refused_by_a_meeting_keeps_a_earlier_result() {
        let mut dictation = done();
        let action = dictation
            .toggle(context(Some(Model::gguf("tiny.gguf")), true), base())
            .unwrap();
        assert!(matches!(action, DictationAction::Refuse(MEETING_ACTIVE)));
        assert!(matches!(&dictation, Dictation::Done(done) if done.text == "hi"));
    }

    #[test]
    fn a_press_without_a_model_is_refused() {
        let mut dictation = Dictation::Idle;
        let action = dictation.toggle(context(None, false), base()).unwrap();
        assert!(matches!(action, DictationAction::Refuse(MODEL_MISSING)));
        assert!(matches!(dictation, Dictation::Idle));
        assert!(!dictation.is_active());
    }

    #[test]
    fn a_finished_dictation_becomes_done_with_its_text() {
        let now = base();
        let mut dictation = stopping(now);
        assert!(
            dictation
                .reduce(
                    DictationEvent::Finished(outcome("こんにちは", None)),
                    now + Duration::from_secs(4)
                )
                .is_none()
        );
        let Dictation::Done(done) = &dictation else {
            unreachable!("a finished event makes a done dictation");
        };
        assert_eq!(done.text, "こんにちは");
        assert!(done.failure.is_none());
        assert!(!done.failed());
        assert_eq!(done.duration, Duration::from_secs(4));
        assert!(!dictation.is_active());
    }

    #[test]
    fn a_failed_or_empty_dictation_reports_it() {
        let mut dictation = listening();
        dictation.reduce(
            DictationEvent::Finished(outcome(
                "",
                Some(DesktopError::Dictation("no keyboard".to_owned())),
            )),
            base(),
        );
        assert!(matches!(&dictation, Dictation::Done(done) if done.failed()));

        let mut empty = listening();
        empty.reduce(DictationEvent::Finished(outcome("", None)), base());
        assert!(matches!(&empty, Dictation::Done(done) if done.failed()));
    }

    #[test]
    fn dismiss_returns_to_idle_and_a_new_take_can_begin() {
        let mut dictation = listening();
        dictation.reduce(DictationEvent::Finished(outcome("hi", None)), base());
        assert!(dictation.reduce(DictationEvent::Dismiss, base()).is_none());
        assert!(matches!(dictation, Dictation::Idle));
        assert!(matches!(
            dictation.toggle(configured(), base()),
            Some(DictationAction::Start(_))
        ));
    }

    #[test]
    fn events_that_do_not_apply_leave_the_phase_alone() {
        let mut dictation = Dictation::Idle;
        assert!(
            dictation
                .reduce(DictationEvent::Finished(outcome("hi", None)), base())
                .is_none()
        );
        assert!(matches!(dictation, Dictation::Idle));

        let mut active = listening();
        assert!(active.reduce(DictationEvent::Dismiss, base()).is_none());
        assert!(active.is_active());
    }

    #[test]
    fn elapsed_tracks_the_phase_and_the_result() {
        let now = base();
        let mut dictation = Dictation::Idle;
        assert_eq!(dictation.elapsed(now), None);
        dictation.toggle(configured(), now);
        assert_eq!(
            dictation.elapsed(now + Duration::from_secs(2)),
            Some(Duration::from_secs(2))
        );
        dictation.toggle(configured(), now);
        assert_eq!(
            dictation.elapsed(now + Duration::from_secs(3)),
            Some(Duration::from_secs(3))
        );
        dictation.reduce(
            DictationEvent::Finished(outcome("hi", None)),
            now + Duration::from_secs(5),
        );
        assert_eq!(dictation.elapsed(now), Some(Duration::from_secs(5)));
    }

    #[test]
    fn a_meeting_start_is_allowed_only_while_dictation_is_inactive() {
        assert_eq!(meeting_start_allowed(&Dictation::Idle), Ok(()));
        assert_eq!(meeting_start_allowed(&done()), Ok(()));
        assert_eq!(meeting_start_allowed(&listening()), Err(MEETING_BLOCKED));
        let mut stopping = listening();
        stopping.toggle(configured(), base());
        assert_eq!(meeting_start_allowed(&stopping), Err(MEETING_BLOCKED));
    }

    #[test]
    fn the_refusal_sentences_read_as_instructions() {
        assert!(MEETING_ACTIVE.contains("Stop it"));
        assert!(MODEL_MISSING.contains("model"));
        assert!(MEETING_BLOCKED.contains("Stop it"));
    }
}
