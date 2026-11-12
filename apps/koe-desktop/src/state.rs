//! The recording state machine.
//!
//! The UI thread owns one [`Phase`]. Every input from the tray, the window,
//! or the recorder thread goes through [`Phase::reduce`], which returns the
//! work the caller must run. The type is pure, so the transitions run in
//! tests without a window, an audio device, or a file.

use std::time::{Duration, Instant};

use koe_capture::Source;
use koe_record::Summary;

use crate::engine::{Completion, SessionPlan};
use crate::error::DesktopError;

/// A set of capture sources, in the order every view draws them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Sources {
    microphone: bool,
    system_audio: bool,
}

impl Sources {
    /// No source.
    pub const EMPTY: Self = Self {
        microphone: false,
        system_audio: false,
    };

    /// The microphone alone.
    pub const MICROPHONE: Self = Self {
        microphone: true,
        system_audio: false,
    };

    /// The audio the machine plays alone.
    pub const SYSTEM_AUDIO: Self = Self {
        microphone: false,
        system_audio: true,
    };

    /// Both sources.
    pub const MEETING: Self = Self {
        microphone: true,
        system_audio: true,
    };

    /// Whether `source` is a member.
    #[must_use]
    pub const fn contains(self, source: Source) -> bool {
        match source {
            Source::Microphone => self.microphone,
            Source::SystemAudio => self.system_audio,
        }
    }

    /// The set with `source` added.
    #[must_use]
    pub const fn with(self, source: Source) -> Self {
        match source {
            Source::Microphone => Self {
                microphone: true,
                ..self
            },
            Source::SystemAudio => Self {
                system_audio: true,
                ..self
            },
        }
    }

    /// The set without `source`.
    #[must_use]
    pub const fn without(self, source: Source) -> Self {
        match source {
            Source::Microphone => Self {
                microphone: false,
                ..self
            },
            Source::SystemAudio => Self {
                system_audio: false,
                ..self
            },
        }
    }

    /// Number of members.
    #[must_use]
    pub const fn len(self) -> usize {
        let mut count = 0;
        if self.microphone {
            count += 1;
        }
        if self.system_audio {
            count += 1;
        }
        count
    }

    /// Whether no source is a member.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.microphone && !self.system_audio
    }

    /// Iterates the members in draw order.
    pub fn iter(self) -> impl Iterator<Item = Source> {
        [Source::Microphone, Source::SystemAudio]
            .into_iter()
            .filter(move |source| self.contains(*source))
    }

    /// Index of `source` in the per-source level arrays.
    #[must_use]
    pub const fn index(source: Source) -> usize {
        match source {
            Source::Microphone => 0,
            Source::SystemAudio => 1,
        }
    }
}

impl From<Source> for Sources {
    fn from(source: Source) -> Self {
        Self::EMPTY.with(source)
    }
}

/// Why the capture is closing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopCause {
    /// The user stopped the take.
    User,
    /// The last live source went away.
    AllSourcesLost,
    /// No source opened.
    StartFailed,
}

/// The shared part of every active phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Live {
    /// When the take began, which also starts the elapsed clock.
    pub started: Instant,
    /// Sources that are delivering audio.
    pub live: Sources,
    /// Sources that opened and then ended, or never opened.
    pub lost: Sources,
    /// The last source warning, if any.
    pub warning: Option<String>,
    /// Time the take has spent paused, accumulated at each resume.
    pub paused: Duration,
    /// When the current pause began, while the take is paused.
    pub pause_started: Option<Instant>,
}

impl Live {
    /// A take that has not opened a source yet.
    #[must_use]
    pub const fn new(started: Instant) -> Self {
        Self {
            started,
            live: Sources::EMPTY,
            lost: Sources::EMPTY,
            warning: None,
            paused: Duration::ZERO,
            pause_started: None,
        }
    }

    /// Time the take has been capturing, excluding every paused interval.
    fn elapsed(&self, now: Instant) -> Duration {
        let paused = self.pause_started.map_or(self.paused, |since| {
            self.paused
                .saturating_add(now.saturating_duration_since(since))
        });
        now.saturating_duration_since(self.started)
            .saturating_sub(paused)
    }
}

/// A finished take.
#[derive(Debug)]
pub struct Done {
    /// File the recorder wrote, or tried to write.
    pub output: std::path::PathBuf,
    /// What the recorder measured, when it finished cleanly.
    pub summary: Option<Summary>,
    /// Why the take could not be completed.
    pub failure: Option<DesktopError>,
    /// Duration shown in the window, from the summary or the clock.
    pub duration: Duration,
    /// The last source warning.
    pub warning: Option<String>,
    /// Whether the session of an empty take was removed from the store.
    ///
    /// The shell records the store's answer after [`Action::Delete`]; a failed
    /// removal leaves it false, so the card does not claim one.
    pub session_removed: bool,
}

impl Done {
    /// Whether the take ended with an error or wrote no audio.
    #[must_use]
    #[allow(
        clippy::option_if_let_else,
        reason = "Option::map_or is not const on this toolchain"
    )]
    pub const fn failed(&self) -> bool {
        match self.summary {
            Some(summary) => summary.frames == 0 || self.failure.is_some(),
            None => true,
        }
    }
}

/// Where the current take is.
#[derive(Debug)]
pub enum Phase {
    /// No take.
    Idle,
    /// At least one source is opening.
    Arming {
        /// The take's shared state.
        live: Live,
        /// Sources whose open has not been answered.
        pending: Sources,
    },
    /// At least one source is capturing.
    Recording(Live),
    /// Capture is open but nothing is being written.
    Paused(Live),
    /// Capture is closing and the file is finishing.
    Stopping {
        /// The take's shared state.
        live: Live,
        /// Why it is closing.
        cause: StopCause,
    },
    /// The take ended.
    Done(Box<Done>),
}

/// Something that happened to the take.
#[derive(Debug)]
pub enum Event {
    /// Begin a take with a prepared plan.
    Start(SessionPlan),
    /// A source's device opened.
    Opened(Source),
    /// A source never opened, or stopped delivering.
    Lost {
        /// Source that went away.
        source: Source,
        /// Why it went away.
        message: String,
    },
    /// The user or the tray asked to stop.
    Stop,
    /// The user asked to stop writing audio without closing the file.
    Pause,
    /// The user asked to write audio again after a pause.
    Resume,
    /// The recorder thread finished the file.
    Finished(Box<Completion>),
    /// The user dismissed the result.
    Dismiss,
}

/// Work the caller must run after a transition.
#[derive(Debug)]
pub enum Action {
    /// Start the recorder with this plan.
    Start(SessionPlan),
    /// Ask the recorder to stop.
    Stop,
    /// Discard the session of a take that wrote no audio.
    Delete,
    /// Ask the recorder to stop writing until it resumes.
    Pause,
    /// Ask the recorder to write again.
    Resume,
}

impl Phase {
    /// Whether a take is starting, recording, paused, or saving.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Arming { .. } | Self::Recording(_) | Self::Paused(_) | Self::Stopping { .. }
        )
    }

    /// Whether a new take can begin.
    #[must_use]
    pub const fn can_start(&self) -> bool {
        matches!(self, Self::Idle | Self::Done(_))
    }

    /// Whether Stop applies.
    #[must_use]
    pub const fn can_stop(&self) -> bool {
        matches!(
            self,
            Self::Arming { .. } | Self::Recording(_) | Self::Paused(_)
        )
    }

    /// Whether the take is paused.
    #[must_use]
    pub const fn is_paused(&self) -> bool {
        matches!(self, Self::Paused(_))
    }

    /// Records whether the finished empty take's session was removed.
    ///
    /// [`Action::Delete`] is the store's work, so its answer arrives after
    /// [`Self::reduce`] built the [`Done`] the card draws.
    pub fn set_session_removed(&mut self, removed: bool) {
        if let Self::Done(done) = self {
            done.session_removed = removed;
        }
    }

    /// The sources delivering audio, or none.
    #[must_use]
    pub const fn live(&self) -> Sources {
        match self {
            Self::Arming { live, .. }
            | Self::Recording(live)
            | Self::Paused(live)
            | Self::Stopping { live, .. } => live.live,
            Self::Idle | Self::Done(_) => Sources::EMPTY,
        }
    }

    /// The sources that failed, or none.
    #[must_use]
    pub const fn lost(&self) -> Sources {
        match self {
            Self::Arming { live, .. }
            | Self::Recording(live)
            | Self::Paused(live)
            | Self::Stopping { live, .. } => live.lost,
            Self::Idle | Self::Done(_) => Sources::EMPTY,
        }
    }

    /// The last source warning, if any.
    #[must_use]
    pub fn warning(&self) -> Option<&str> {
        match self {
            Self::Arming { live, .. }
            | Self::Recording(live)
            | Self::Paused(live)
            | Self::Stopping { live, .. } => live.warning.as_deref(),
            Self::Idle => None,
            Self::Done(done) => done.warning.as_deref(),
        }
    }

    /// Time spent capturing in the current take, excluding paused intervals.
    #[must_use]
    pub fn elapsed(&self, now: Instant) -> Option<Duration> {
        match self {
            Self::Idle => None,
            Self::Arming { live, .. }
            | Self::Recording(live)
            | Self::Paused(live)
            | Self::Stopping { live, .. } => Some(live.elapsed(now)),
            Self::Done(done) => Some(done.duration),
        }
    }

    /// Short status for the window and the tray.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "Ready",
            Self::Arming { .. } => "Starting",
            Self::Recording(_) => "Recording",
            Self::Paused(_) => "Paused",
            Self::Stopping { .. } => "Saving",
            Self::Done(done) if done.failed() => "Failed",
            Self::Done(_) => "Saved",
        }
    }

    /// Applies `event` and returns the work it asks for.
    ///
    /// An event that does not apply leaves the phase unchanged.
    #[allow(
        clippy::too_many_lines,
        reason = "the transition table is one exhaustive match; splitting it hides which events each phase answers"
    )]
    pub fn reduce(&mut self, event: Event, now: Instant) -> Option<Action> {
        let current = std::mem::replace(self, Self::Idle);
        let (next, action) = match (current, event) {
            (Self::Idle, Event::Start(plan)) => start(plan, now).unwrap_or((Self::Idle, None)),
            (Self::Done(done), Event::Start(plan)) => {
                start(plan, now).unwrap_or((Self::Done(done), None))
            }
            (Self::Arming { live, pending }, Event::Opened(source)) => {
                let pending = pending.without(source);
                let mut live = live;
                live.live = live.live.with(source);
                settle_arming(live, pending)
            }
            (Self::Arming { live, pending }, Event::Lost { source, message }) => {
                let pending = pending.without(source);
                let mut live = live;
                live.lost = live.lost.with(source);
                live.warning = Some(message);
                settle_arming(live, pending)
            }
            (Self::Recording(live), Event::Lost { source, message }) => {
                let live = lose(live, source, message);
                if live.live.is_empty() {
                    (
                        Self::Stopping {
                            live,
                            cause: StopCause::AllSourcesLost,
                        },
                        Some(Action::Stop),
                    )
                } else {
                    (Self::Recording(live), None)
                }
            }
            (Self::Paused(live), Event::Lost { source, message }) => {
                let live = lose(live, source, message);
                if live.live.is_empty() {
                    (
                        Self::Stopping {
                            live,
                            cause: StopCause::AllSourcesLost,
                        },
                        Some(Action::Stop),
                    )
                } else {
                    (Self::Paused(live), None)
                }
            }
            (Self::Recording(live), Event::Pause) => (
                Self::Paused(Live {
                    pause_started: Some(now),
                    ..live
                }),
                Some(Action::Pause),
            ),
            (Self::Paused(live), Event::Resume) => {
                let paused = live.paused.saturating_add(
                    live.pause_started
                        .map_or(Duration::ZERO, |since| now.saturating_duration_since(since)),
                );
                (
                    Self::Recording(Live {
                        paused,
                        pause_started: None,
                        ..live
                    }),
                    Some(Action::Resume),
                )
            }
            (
                Self::Arming { live, .. } | Self::Recording(live) | Self::Paused(live),
                Event::Stop,
            ) => (
                Self::Stopping {
                    live,
                    cause: StopCause::User,
                },
                Some(Action::Stop),
            ),
            (Self::Stopping { live, cause }, Event::Stop) => (Self::Stopping { live, cause }, None),
            (
                Self::Arming { live, .. }
                | Self::Recording(live)
                | Self::Paused(live)
                | Self::Stopping { live, .. },
                Event::Finished(completion),
            ) => {
                let Completion {
                    output,
                    summary,
                    failure,
                    transcripts: _,
                } = *completion;
                let delete = summary.as_ref().is_some_and(|summary| summary.frames == 0);
                let duration = summary
                    .as_ref()
                    .map_or_else(|| live.elapsed(now), |summary| summary.duration);
                let done = Done {
                    output,
                    summary,
                    failure,
                    duration,
                    warning: live.warning,
                    session_removed: false,
                };
                (Self::Done(Box::new(done)), delete.then_some(Action::Delete))
            }
            (Self::Done(_), Event::Dismiss) => (Self::Idle, None),
            (phase, _) => (phase, None),
        };
        *self = next;
        action
    }
}

fn start(plan: SessionPlan, now: Instant) -> Option<(Phase, Option<Action>)> {
    if plan.sources.is_empty() {
        return None;
    }
    let pending = plan.sources;
    Some((
        Phase::Arming {
            live: Live::new(now),
            pending,
        },
        Some(Action::Start(plan)),
    ))
}

fn lose(mut live: Live, source: Source, message: String) -> Live {
    live.live = live.live.without(source);
    live.lost = live.lost.with(source);
    live.warning = Some(message);
    live
}

const fn settle_arming(live: Live, pending: Sources) -> (Phase, Option<Action>) {
    if !pending.is_empty() {
        return (Phase::Arming { live, pending }, None);
    }
    if live.live.is_empty() {
        return (
            Phase::Stopping {
                live,
                cause: StopCause::StartFailed,
            },
            Some(Action::Stop),
        );
    }
    (Phase::Recording(live), None)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;

    fn base() -> Instant {
        Instant::now()
    }

    fn plan() -> SessionPlan {
        SessionPlan::meeting(PathBuf::from("take.ogg"), Sources::MEETING).unwrap()
    }

    fn started() -> Phase {
        started_at(base())
    }

    fn started_at(now: Instant) -> Phase {
        let mut phase = Phase::Idle;
        phase.reduce(Event::Start(plan()), now);
        phase
    }

    fn recording() -> Phase {
        recording_at(base())
    }

    fn recording_at(now: Instant) -> Phase {
        let mut phase = started_at(now);
        phase.reduce(Event::Opened(Source::Microphone), now);
        phase.reduce(Event::Opened(Source::SystemAudio), now);
        phase
    }

    fn completion(frames: u64) -> Completion {
        Completion {
            output: PathBuf::from("take.ogg"),
            summary: Some(Summary {
                frames,
                clipped_samples: 0,
                duration: Duration::from_secs(3),
            }),
            failure: None,
            transcripts: koe_session_store::Transcripts::new(),
        }
    }

    #[test]
    fn a_start_arms_the_plan() {
        let mut phase = Phase::Idle;
        let action = phase.reduce(Event::Start(plan()), base());
        assert!(matches!(action, Some(Action::Start(_))));
        assert!(matches!(phase, Phase::Arming { .. }));
        assert!(phase.can_stop());
        assert!(!phase.can_start());
    }

    #[test]
    fn both_opens_start_recording() {
        let phase = recording();
        assert!(matches!(phase, Phase::Recording(_)));
        assert_eq!(phase.live(), Sources::MEETING);
        assert_eq!(phase.label(), "Recording");
    }

    #[test]
    fn one_failed_source_keeps_the_other_recording() {
        let mut phase = started();
        phase.reduce(Event::Opened(Source::Microphone), base());
        phase.reduce(
            Event::Lost {
                source: Source::SystemAudio,
                message: "no monitor".to_owned(),
            },
            base(),
        );
        assert!(matches!(phase, Phase::Recording(_)));
        assert_eq!(phase.live(), Sources::MICROPHONE);
        assert_eq!(phase.lost(), Sources::SYSTEM_AUDIO);
        assert_eq!(phase.warning(), Some("no monitor"));
    }

    #[test]
    fn losing_every_source_stops_the_take() {
        let mut phase = recording();
        let action = phase.reduce(
            Event::Lost {
                source: Source::Microphone,
                message: "unplugged".to_owned(),
            },
            base(),
        );
        assert!(action.is_none());
        let action = phase.reduce(
            Event::Lost {
                source: Source::SystemAudio,
                message: "server stopped".to_owned(),
            },
            base(),
        );
        assert!(matches!(action, Some(Action::Stop)));
        assert!(matches!(
            phase,
            Phase::Stopping {
                cause: StopCause::AllSourcesLost,
                ..
            }
        ));
    }

    #[test]
    fn no_open_source_ends_before_recording() {
        let mut phase = started();
        let action = phase.reduce(
            Event::Lost {
                source: Source::Microphone,
                message: "denied".to_owned(),
            },
            base(),
        );
        assert!(action.is_none());
        let action = phase.reduce(
            Event::Lost {
                source: Source::SystemAudio,
                message: "denied".to_owned(),
            },
            base(),
        );
        assert!(matches!(action, Some(Action::Stop)));
        assert!(matches!(
            phase,
            Phase::Stopping {
                cause: StopCause::StartFailed,
                ..
            }
        ));
    }

    #[test]
    fn stop_moves_to_saving_and_is_idempotent() {
        let mut phase = recording();
        assert!(matches!(
            phase.reduce(Event::Stop, base()),
            Some(Action::Stop)
        ));
        assert!(matches!(phase, Phase::Stopping { .. }));
        assert!(phase.reduce(Event::Stop, base()).is_none());
        assert!(matches!(phase, Phase::Stopping { .. }));
    }

    #[test]
    fn pause_freezes_the_elapsed_time_until_resume() {
        let t = base();
        let mut phase = recording_at(t);
        assert!(matches!(
            phase.reduce(Event::Pause, t + Duration::from_secs(5)),
            Some(Action::Pause)
        ));
        assert!(matches!(phase, Phase::Paused(_)));
        assert!(phase.is_paused());
        assert!(phase.is_active());
        assert!(phase.can_stop());
        assert!(!phase.can_start());
        assert_eq!(phase.label(), "Paused");
        assert_eq!(
            phase.elapsed(t + Duration::from_secs(20)),
            Some(Duration::from_secs(5))
        );

        assert!(matches!(
            phase.reduce(Event::Resume, t + Duration::from_secs(20)),
            Some(Action::Resume)
        ));
        assert!(matches!(phase, Phase::Recording(_)));
        assert_eq!(
            phase.elapsed(t + Duration::from_secs(25)),
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn pause_time_accumulates_across_cycles_and_freezes_while_saving() {
        let t = base();
        let mut phase = recording_at(t);

        phase.reduce(Event::Pause, t + Duration::from_secs(5));
        phase.reduce(Event::Resume, t + Duration::from_secs(20));
        assert_eq!(
            phase.elapsed(t + Duration::from_secs(20)),
            Some(Duration::from_secs(5))
        );

        phase.reduce(Event::Pause, t + Duration::from_secs(25));
        assert_eq!(
            phase.elapsed(t + Duration::from_secs(30)),
            Some(Duration::from_secs(10))
        );

        phase.reduce(Event::Resume, t + Duration::from_secs(40));
        assert_eq!(
            phase.elapsed(t + Duration::from_secs(45)),
            Some(Duration::from_secs(15))
        );

        phase.reduce(Event::Pause, t + Duration::from_secs(50));
        phase.reduce(Event::Stop, t + Duration::from_secs(60));
        assert_eq!(
            phase.elapsed(t + Duration::from_secs(60)),
            Some(Duration::from_secs(20))
        );
        assert_eq!(
            phase.elapsed(t + Duration::from_secs(90)),
            Some(Duration::from_secs(20))
        );
    }

    #[test]
    fn a_pause_or_resume_that_does_not_apply_is_ignored() {
        let t = base();
        let mut paused = recording_at(t);
        paused.reduce(Event::Pause, t);
        assert!(
            paused
                .reduce(Event::Pause, t + Duration::from_secs(1))
                .is_none()
        );
        assert!(paused.is_paused());

        let mut recording = recording_at(t);
        assert!(
            recording
                .reduce(Event::Resume, t + Duration::from_secs(1))
                .is_none()
        );
        assert!(matches!(recording, Phase::Recording(_)));

        let mut arming = started_at(t);
        assert!(arming.reduce(Event::Pause, t).is_none());
        assert!(matches!(arming, Phase::Arming { .. }));
    }

    #[test]
    fn stopping_a_paused_take_finalizes_the_file() {
        let t = base();
        let mut phase = recording_at(t);
        phase.reduce(Event::Pause, t + Duration::from_secs(5));
        assert!(matches!(
            phase.reduce(Event::Stop, t + Duration::from_secs(20)),
            Some(Action::Stop)
        ));
        assert!(matches!(phase, Phase::Stopping { .. }));
        let action = phase.reduce(Event::Finished(Box::new(completion(960))), base());
        assert!(action.is_none());
        assert!(matches!(
            &phase,
            Phase::Done(done) if done.summary.is_some_and(|summary| summary.frames == 960)
        ));
    }

    #[test]
    fn a_paused_take_that_loses_every_source_stops() {
        let t = base();
        let mut phase = recording_at(t);
        phase.reduce(Event::Pause, t);
        phase.reduce(
            Event::Lost {
                source: Source::Microphone,
                message: "unplugged".to_owned(),
            },
            t,
        );
        assert!(phase.is_paused(), "one live source keeps the pause");
        assert_eq!(phase.live(), Sources::SYSTEM_AUDIO);
        let action = phase.reduce(
            Event::Lost {
                source: Source::SystemAudio,
                message: "server stopped".to_owned(),
            },
            t,
        );
        assert!(matches!(action, Some(Action::Stop)));
        assert!(matches!(
            phase,
            Phase::Stopping {
                cause: StopCause::AllSourcesLost,
                ..
            }
        ));
    }

    #[test]
    fn a_finished_paused_take_without_a_summary_counts_captured_time_only() {
        let t = base();
        let mut phase = recording_at(t);
        phase.reduce(Event::Pause, t + Duration::from_secs(5));
        phase.reduce(Event::Stop, t + Duration::from_secs(20));
        phase.reduce(
            Event::Finished(Box::new(Completion {
                output: PathBuf::from("take.ogg"),
                summary: None,
                failure: Some(DesktopError::NoSources),
                transcripts: koe_session_store::Transcripts::new(),
            })),
            t + Duration::from_secs(20),
        );
        assert!(matches!(&phase, Phase::Done(done) if done.duration == Duration::from_secs(5)));
    }

    #[test]
    fn a_finished_take_becomes_done_with_its_summary() {
        let mut phase = recording();
        phase.reduce(Event::Stop, base());
        let action = phase.reduce(Event::Finished(Box::new(completion(960))), base());
        assert!(action.is_none());
        assert!(matches!(
            &phase,
            Phase::Done(done)
                if done.summary.is_some_and(|summary| summary.frames == 960)
                    && done.duration == Duration::from_secs(3)
                    && !done.failed()
        ));
    }

    #[test]
    fn an_empty_file_is_deleted() {
        let mut phase = recording();
        phase.reduce(Event::Stop, base());
        let action = phase.reduce(Event::Finished(Box::new(completion(0))), base());
        assert!(matches!(action, Some(Action::Delete)));
    }

    #[test]
    fn set_session_removed_flips_the_finished_take() {
        let mut phase = recording();
        phase.reduce(Event::Finished(Box::new(completion(0))), base());
        assert!(matches!(&phase, Phase::Done(done) if !done.session_removed));

        phase.set_session_removed(true);
        assert!(matches!(&phase, Phase::Done(done) if done.session_removed));

        phase.set_session_removed(false);
        assert!(matches!(&phase, Phase::Done(done) if !done.session_removed));
    }

    #[test]
    fn a_failed_recorder_reports_failure_and_keeps_the_file() {
        let mut phase = recording();
        let action = phase.reduce(
            Event::Finished(Box::new(Completion {
                output: PathBuf::from("take.ogg"),
                summary: None,
                failure: Some(DesktopError::NoSources),
                transcripts: koe_session_store::Transcripts::new(),
            })),
            base(),
        );
        assert!(action.is_none());
        assert!(matches!(&phase, Phase::Done(done) if done.failed()));
        assert_eq!(phase.label(), "Failed");
    }

    #[test]
    fn dismiss_returns_to_idle() {
        let mut phase = recording();
        phase.reduce(Event::Finished(Box::new(completion(1))), base());
        phase.reduce(Event::Dismiss, base());
        assert!(matches!(phase, Phase::Idle));
        assert!(phase.can_start());
    }

    #[test]
    fn a_done_take_can_start_again() {
        let mut phase = recording();
        phase.reduce(Event::Finished(Box::new(completion(1))), base());
        let action = phase.reduce(Event::Start(plan()), base());
        assert!(matches!(action, Some(Action::Start(_))));
        assert!(matches!(phase, Phase::Arming { .. }));
    }

    #[test]
    fn an_empty_plan_does_not_start() {
        let mut phase = Phase::Idle;
        let plan = SessionPlan {
            output: PathBuf::from("take.ogg"),
            spec: koe_record::RecordingSpec::new(48_000, koe_record::Channels::Stereo).unwrap(),
            streams: Vec::new(),
            sources: Sources::EMPTY,
            transcription: None,
        };
        assert!(phase.reduce(Event::Start(plan), base()).is_none());
        assert!(matches!(phase, Phase::Idle));
    }

    #[test]
    fn elapsed_tracks_the_active_phase() {
        let now = base();
        let mut phase = Phase::Idle;
        phase.reduce(Event::Start(plan()), now);
        assert_eq!(
            phase.elapsed(now + Duration::from_secs(5)),
            Some(Duration::from_secs(5))
        );
        phase.reduce(Event::Finished(Box::new(completion(1))), now);
        assert_eq!(phase.elapsed(now), Some(Duration::from_secs(3)));
    }

    #[test]
    fn unhandled_events_leave_the_phase_alone() {
        let mut phase = recording();
        let before = phase.label();
        phase.reduce(Event::Dismiss, base());
        assert_eq!(phase.label(), before);
        assert!(matches!(phase, Phase::Recording(_)));
    }

    #[test]
    fn source_sets_report_membership_and_size() {
        assert!(Sources::MEETING.contains(Source::Microphone));
        assert!(Sources::MEETING.contains(Source::SystemAudio));
        assert!(!Sources::MICROPHONE.contains(Source::SystemAudio));
        assert!(!Sources::SYSTEM_AUDIO.contains(Source::Microphone));
        assert!(Sources::EMPTY.is_empty());
        assert!(!Sources::MICROPHONE.is_empty());
        assert_eq!(Sources::EMPTY.len(), 0);
        assert_eq!(Sources::MICROPHONE.len(), 1);
        assert_eq!(Sources::SYSTEM_AUDIO.len(), 1);
        assert_eq!(Sources::MEETING.len(), 2);
        assert_eq!(
            Sources::MEETING.iter().collect::<Vec<_>>(),
            vec![Source::Microphone, Source::SystemAudio]
        );
        assert_eq!(
            Sources::SYSTEM_AUDIO.iter().collect::<Vec<_>>(),
            vec![Source::SystemAudio]
        );
        assert_eq!(
            Sources::MICROPHONE.with(Source::SystemAudio),
            Sources::MEETING
        );
        assert_eq!(
            Sources::MEETING.without(Source::Microphone),
            Sources::SYSTEM_AUDIO
        );
        assert_eq!(Sources::from(Source::Microphone), Sources::MICROPHONE);
        assert_eq!(Sources::from(Source::SystemAudio), Sources::SYSTEM_AUDIO);
        assert_eq!(Sources::index(Source::Microphone), 0);
        assert_eq!(Sources::index(Source::SystemAudio), 1);
    }

    #[test]
    fn phase_states_report_capabilities() {
        assert!(!Phase::Idle.is_active());
        assert!(Phase::Idle.can_start());
        assert!(!Phase::Idle.can_stop());

        let arming = started();
        assert!(arming.is_active());
        assert!(!arming.can_start());
        assert!(arming.can_stop());

        let live = recording();
        assert!(live.is_active());
        assert!(!live.can_start());
        assert!(live.can_stop());
        assert!(!live.is_paused());

        let mut paused = recording();
        paused.reduce(Event::Pause, base());
        assert!(paused.is_active());
        assert!(paused.is_paused());
        assert!(!paused.can_start());
        assert!(paused.can_stop());

        let mut stopping = live;
        stopping.reduce(Event::Stop, base());
        assert!(stopping.is_active());
        assert!(!stopping.can_start());
        assert!(!stopping.can_stop());

        let mut done = Phase::Recording(Live::new(base()));
        done.reduce(Event::Finished(Box::new(completion(1))), base());
        assert!(!done.is_active());
        assert!(done.can_start());
        assert!(!done.can_stop());
    }

    #[test]
    fn done_failure_and_label_cover_empty_failed_and_saved() {
        let saved = Phase::Done(Box::new(done(1, None)));
        assert_eq!(saved.label(), "Saved");
        assert!(!matches!(&saved, Phase::Done(done) if done.failed()));

        let empty = Phase::Done(Box::new(done(0, None)));
        assert_eq!(empty.label(), "Failed");
        assert!(matches!(&empty, Phase::Done(done) if done.failed()));

        let failed = Phase::Done(Box::new(done(1, Some(DesktopError::NoSources))));
        assert_eq!(failed.label(), "Failed");
        assert!(matches!(&failed, Phase::Done(done) if done.failed()));

        let mut missing = done(1, None);
        missing.summary = None;
        assert!(missing.failed());
    }

    fn done(frames: u64, failure: Option<DesktopError>) -> Done {
        Done {
            output: PathBuf::from("take.ogg"),
            summary: Some(Summary {
                frames,
                clipped_samples: 0,
                duration: Duration::from_secs(1),
            }),
            failure,
            duration: Duration::from_secs(1),
            warning: None,
            session_removed: false,
        }
    }
}
