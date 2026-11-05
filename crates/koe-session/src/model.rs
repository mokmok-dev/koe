//! Stateright model of the production [`Session::step`].
//!
//! The model drives the same transition function the application calls, so a
//! counterexample is a real defect rather than a modelling gap. `cargo test
//! -p koe-session` explores the whole reachable space once per test that asks
//! for a checker.

use stateright::{Model, Property};

use koe_capture_core::Source;

use crate::input::{Effect, Input};
use crate::permission::{Permission, Permissions};
use crate::sources::SourceSet;
use crate::state::{
    Closing, Confirmed, Next, Phase, Progress, Recording, Reviewing, Session, Stage,
};
use crate::transcript::{Outcome, SegmentOutcome, SegmentResult, Transcript};

const SOURCES: [Source; 2] = [Source::Microphone, Source::SystemAudio];

const SETS: [SourceSet; 3] = [
    SourceSet::MICROPHONE,
    SourceSet::SYSTEM_AUDIO,
    SourceSet::MEETING,
];

const RESULTS: [SegmentResult; 4] = [
    SegmentResult {
        outcome: SegmentOutcome::Complete,
        committed: false,
    },
    SegmentResult {
        outcome: SegmentOutcome::Complete,
        committed: true,
    },
    SegmentResult {
        outcome: SegmentOutcome::Incomplete,
        committed: false,
    },
    SegmentResult {
        outcome: SegmentOutcome::Incomplete,
        committed: true,
    },
];

/// What the effects so far account for, per meeting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the model records independent meeting-scoped observations"
)]
struct History {
    opens: u8,
    closes: u8,
    finishes: u8,
    discards: u8,
    commit_seen: bool,
    incomplete_seen: bool,
    resumed: bool,
    revoked_during_capture: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ModelState {
    session: Session,
    history: History,
}

fn candidates() -> Vec<Input> {
    let mut inputs = Vec::new();
    for source in SOURCES {
        inputs.push(Input::RequestPermission(source));
        inputs.push(Input::PermissionGranted(source));
        inputs.push(Input::PermissionDenied(source));
        inputs.push(Input::PermissionRevoked(source));
        inputs.push(Input::DeviceOpened(source));
        inputs.push(Input::DeviceFailed(source));
        inputs.push(Input::CaptureFailed(source));
        inputs.push(Input::EngineFailed(source));
        inputs.push(Input::EngineCommitted(source));
        inputs.push(Input::EngineVolatile(source));
        for result in RESULTS {
            inputs.push(Input::SegmentClosed { source, result });
        }
    }
    for sources in SETS {
        inputs.push(Input::Start(sources));
    }
    inputs.push(Input::Pause);
    inputs.push(Input::Resume);
    inputs.push(Input::Stop);
    inputs.push(Input::Confirm);
    inputs.push(Input::Reset);
    inputs
}

fn count(sources: SourceSet) -> u8 {
    u8::try_from(sources.len()).unwrap_or(u8::MAX)
}

/// Pause and resume can alternate forever, so the counters grow without bound
/// and the exploration needs a cap. Four opens cover a two source meeting
/// paused and resumed once, which is every behavior the properties describe.
const BOUND: u8 = 4;

/// The counters alone do not bound the space: a broken transition can close a
/// stream it never opened, which raises `closes` while `opens` stays put. Every
/// counter is capped so a mutated machine still terminates.
fn boundary(state: &ModelState) -> bool {
    let history = state.history;
    history.opens <= BOUND
        && history.closes <= BOUND
        && history.finishes <= BOUND
        && history.discards <= BOUND
}

fn sessions_match(state: &ModelState) -> bool {
    let history = state.history;
    let session = state.session;
    history.opens
        == history.finishes
            + history.discards
            + count(session.pending_sources())
            + count(session.open_sources())
            + count(session.finishing_sources())
}

fn streams_match(state: &ModelState) -> bool {
    let history = state.history;
    let session = state.session;
    history.opens.saturating_sub(history.closes)
        == count(session.pending_sources()) + count(session.open_sources())
}

fn closes_within_opens(state: &ModelState) -> bool {
    state.history.closes <= state.history.opens
}

fn finish_within_close(state: &ModelState) -> bool {
    state.history.finishes + state.history.discards <= state.history.closes
}

fn capture_is_permitted(state: &ModelState) -> bool {
    let session = state.session;
    session
        .permissions()
        .all_granted(session.pending_sources().union(session.open_sources()))
}

fn recording_captures(state: &ModelState) -> bool {
    state.session.stage() != Stage::Recording || !state.session.open_sources().is_empty()
}

fn closing_has_finishing(state: &ModelState) -> bool {
    state.session.stage() != Stage::Closing || !state.session.finishing_sources().is_empty()
}

fn committed_retained(state: &ModelState) -> bool {
    if !state.history.commit_seen
        || !matches!(
            state.session.stage(),
            Stage::Closing | Stage::Reviewing | Stage::Confirmed
        )
    {
        return true;
    }
    state
        .session
        .transcript()
        .is_some_and(Transcript::any_committed)
}

fn incomplete_sticks(state: &ModelState) -> bool {
    !state.history.incomplete_seen || state.session.outcome().is_some_and(Outcome::is_incomplete)
}

fn no_volatile_after_close(state: &ModelState) -> bool {
    if !matches!(
        state.session.stage(),
        Stage::Paused | Stage::Reviewing | Stage::Confirmed
    ) {
        return true;
    }
    !state
        .session
        .transcript()
        .is_some_and(Transcript::any_volatile)
}

fn review_has_progress(state: &ModelState) -> bool {
    if !matches!(state.session.stage(), Stage::Reviewing | Stage::Confirmed) {
        return true;
    }
    state.session.transcript().is_some()
}

fn confirmed_complete(state: &ModelState) -> bool {
    state.session.stage() == Stage::Confirmed && state.session.outcome() == Some(Outcome::Complete)
}

fn confirmed_incomplete(state: &ModelState) -> bool {
    state.session.stage() == Stage::Confirmed
        && state.session.outcome().is_some_and(Outcome::is_incomplete)
}

fn confirmed_with_text(state: &ModelState) -> bool {
    state.session.stage() == Stage::Confirmed
        && state
            .session
            .transcript()
            .is_some_and(Transcript::any_committed)
}

fn empty_confirmed(state: &ModelState) -> bool {
    state.session.stage() == Stage::Confirmed
        && !state
            .session
            .transcript()
            .is_some_and(Transcript::any_committed)
}

fn paused_then_recording(state: &ModelState) -> bool {
    state.session.stage() == Stage::Recording && state.history.resumed
}

fn revoked_then_confirmed(state: &ModelState) -> bool {
    state.session.stage() == Stage::Confirmed && state.history.revoked_during_capture
}

fn both_sources_recording(state: &ModelState) -> bool {
    state.session.stage() == Stage::Recording && state.session.open_sources().len() == 2
}

struct SessionModel;

impl Model for SessionModel {
    type State = ModelState;
    type Action = Input;

    fn init_states(&self) -> Vec<Self::State> {
        vec![ModelState {
            session: Session::default(),
            history: History::default(),
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        for input in candidates() {
            if state.session.step(input).is_ok() {
                actions.push(input);
            }
        }
    }

    fn next_state(&self, last_state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let step = last_state.session.step(action).ok()?;
        let mut history = last_state.history;
        // A meeting that starts or ends resets the observations that only make
        // sense inside one. Reaching idle is the trigger, not the
        // `DiscardTranscript` effect: `Reset` from `Confirmed` ends a meeting
        // without producing that effect.
        if matches!(action, Input::Start(_)) || step.session.stage() == Stage::Idle {
            history.commit_seen = false;
            history.incomplete_seen = false;
            history.resumed = false;
            history.revoked_during_capture = false;
        }
        for effect in &step.effects {
            match effect {
                Effect::OpenSource(_) => history.opens += 1,
                Effect::CloseSource(_) => history.closes += 1,
                Effect::DiscardSource(_) => history.discards += 1,
                Effect::RequestPermission(_)
                | Effect::FinishSource(_)
                | Effect::SaveTranscript
                | Effect::DiscardTranscript => {}
            }
        }
        // A closed segment is what retires a session. `FinishSource` fires as
        // the session enters `finishing`, so counting that effect would count
        // the same session again in `finishing`.
        if matches!(action, Input::SegmentClosed { .. }) {
            history.finishes += 1;
        }
        if matches!(action, Input::EngineCommitted(_))
            || matches!(action, Input::SegmentClosed { result, .. } if result.committed)
        {
            history.commit_seen = true;
        }
        if step.session.outcome().is_some_and(Outcome::is_incomplete) {
            history.incomplete_seen = true;
        }
        if matches!(action, Input::Resume) {
            history.resumed = true;
        }
        if let Input::PermissionRevoked(source) = action
            && last_state.session.stage() == Stage::Recording
            && last_state.session.open_sources().contains(source)
        {
            history.revoked_during_capture = true;
        }
        Some(ModelState {
            session: step.session,
            history,
        })
    }

    fn within_boundary(&self, state: &Self::State) -> bool {
        boundary(state)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::always("sessions_match", |_, state| sessions_match(state)),
            Property::always("streams_match", |_, state| streams_match(state)),
            Property::always("closes_within_opens", |_, state| closes_within_opens(state)),
            Property::always("finish_within_close", |_, state| finish_within_close(state)),
            Property::always("capture_is_permitted", |_, state| {
                capture_is_permitted(state)
            }),
            Property::always("recording_captures", |_, state| recording_captures(state)),
            Property::always("closing_has_finishing", |_, state| {
                closing_has_finishing(state)
            }),
            Property::always("committed_retained", |_, state| committed_retained(state)),
            Property::always("incomplete_sticks", |_, state| incomplete_sticks(state)),
            Property::always("no_volatile_after_close", |_, state| {
                no_volatile_after_close(state)
            }),
            Property::always("review_has_progress", |_, state| review_has_progress(state)),
            Property::sometimes("confirmed_complete", |_, state| confirmed_complete(state)),
            Property::sometimes("confirmed_incomplete", |_, state| {
                confirmed_incomplete(state)
            }),
            Property::sometimes("confirmed_with_text", |_, state| confirmed_with_text(state)),
            Property::sometimes("empty_confirmed", |_, state| empty_confirmed(state)),
            Property::sometimes("paused_then_recording", |_, state| {
                paused_then_recording(state)
            }),
            Property::sometimes("revoked_then_confirmed", |_, state| {
                revoked_then_confirmed(state)
            }),
            Property::sometimes("both_sources_recording", |_, state| {
                both_sources_recording(state)
            }),
        ]
    }
}

fn lose_transcript(session: Session, action: Input) -> Session {
    if !matches!(action, Input::SegmentClosed { .. } | Input::Confirm) {
        return session;
    }
    let empty = Transcript::default();
    let phase = match session.phase {
        Phase::Reviewing(reviewing) => Phase::Reviewing(Reviewing {
            progress: Progress {
                transcript: empty,
                ..reviewing.progress
            },
            ..reviewing
        }),
        Phase::Confirmed(confirmed) => Phase::Confirmed(Confirmed {
            progress: Progress {
                transcript: empty,
                ..confirmed.progress
            },
            ..confirmed
        }),
        other => other,
    };
    Session { phase, ..session }
}

fn leak_revoked(session: Session, last: &Session, action: Input) -> Session {
    if let Input::PermissionRevoked(source) = action
        && last.stage() == Stage::Recording
        && last.open_sources().contains(source)
    {
        return match session.phase {
            Phase::Recording(recording) => Session {
                phase: Phase::Recording(Recording {
                    open: recording.open.union(source.into()),
                    finishing: recording.finishing.without(source),
                    ..recording
                }),
                ..session
            },
            other => Session {
                phase: other,
                ..session
            },
        };
    }
    session
}

/// The state just before `SegmentClosed` confirms a committed source.
///
/// The broken models start beside the corruption they make, so their check
/// stays as cheap as the defect it demonstrates.
fn lossy_init() -> ModelState {
    let mut transcript = Transcript::default();
    transcript.set_committed(Source::Microphone, true);
    ModelState {
        session: Session {
            permissions: Permissions::new(Permission::Granted, Permission::Granted),
            phase: Phase::Closing(Closing {
                sources: SourceSet::MICROPHONE,
                finishing: SourceSet::MICROPHONE,
                progress: Progress {
                    transcript,
                    outcome: Outcome::Complete,
                },
                next: Next::Stop,
            }),
        },
        history: History {
            opens: 1,
            closes: 1,
            finishes: 0,
            discards: 0,
            commit_seen: true,
            incomplete_seen: false,
            resumed: false,
            revoked_during_capture: false,
        },
    }
}

/// The state where both sources record and one revocation arrives.
fn leaky_init() -> ModelState {
    ModelState {
        session: Session {
            permissions: Permissions::new(Permission::Granted, Permission::Granted),
            phase: Phase::Recording(Recording {
                sources: SourceSet::MEETING,
                open: SourceSet::MEETING,
                finishing: SourceSet::EMPTY,
                progress: Progress {
                    transcript: Transcript::default(),
                    outcome: Outcome::Complete,
                },
            }),
        },
        history: History {
            opens: 2,
            closes: 0,
            finishes: 0,
            discards: 0,
            commit_seen: false,
            incomplete_seen: false,
            resumed: false,
            revoked_during_capture: false,
        },
    }
}

struct LossyFinalize;

impl Model for LossyFinalize {
    type State = ModelState;
    type Action = Input;

    fn init_states(&self) -> Vec<Self::State> {
        vec![lossy_init()]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        SessionModel.actions(state, actions);
    }

    fn next_state(&self, last_state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let next = SessionModel.next_state(last_state, action)?;
        Some(ModelState {
            session: lose_transcript(next.session, action),
            history: next.history,
        })
    }

    fn within_boundary(&self, state: &Self::State) -> bool {
        boundary(state)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![Property::always("committed_retained", |_, state| {
            committed_retained(state)
        })]
    }
}

struct LeakyRevoke;

impl Model for LeakyRevoke {
    type State = ModelState;
    type Action = Input;

    fn init_states(&self) -> Vec<Self::State> {
        vec![leaky_init()]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        SessionModel.actions(state, actions);
    }

    fn next_state(&self, last_state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let next = SessionModel.next_state(last_state, action)?;
        Some(ModelState {
            session: leak_revoked(next.session, &last_state.session, action),
            history: next.history,
        })
    }

    fn within_boundary(&self, state: &Self::State) -> bool {
        boundary(state)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![Property::always("capture_is_permitted", |_, state| {
            capture_is_permitted(state)
        })]
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use stateright::{Checker, HasDiscoveries};

    use super::*;

    fn complete_meeting() -> Vec<Input> {
        vec![
            Input::Start(SourceSet::MICROPHONE),
            Input::PermissionGranted(Source::Microphone),
            Input::DeviceOpened(Source::Microphone),
            Input::EngineCommitted(Source::Microphone),
            Input::Stop,
            Input::SegmentClosed {
                source: Source::Microphone,
                result: SegmentResult {
                    outcome: SegmentOutcome::Complete,
                    committed: true,
                },
            },
            Input::Confirm,
        ]
    }

    fn incomplete_meeting() -> Vec<Input> {
        vec![
            Input::Start(SourceSet::MICROPHONE),
            Input::PermissionGranted(Source::Microphone),
            Input::DeviceOpened(Source::Microphone),
            Input::CaptureFailed(Source::Microphone),
            Input::SegmentClosed {
                source: Source::Microphone,
                result: SegmentResult {
                    outcome: SegmentOutcome::Complete,
                    committed: false,
                },
            },
            Input::Confirm,
        ]
    }

    #[test]
    fn the_model_has_no_counterexample_and_reaches_every_example() {
        let checker = SessionModel
            .checker()
            .finish_when(HasDiscoveries::AnyFailures)
            .spawn_bfs()
            .join();
        println!(
            "unique states: {}, max depth: {}",
            checker.unique_state_count(),
            checker.max_depth()
        );
        for (name, path) in checker.discoveries() {
            println!("discovery {name} at depth {}", path.into_actions().len());
        }
        checker.assert_properties();
    }

    #[test]
    fn a_complete_meeting_confirms_with_its_text() {
        let checker = SessionModel
            .checker()
            .finish_when(HasDiscoveries::AnyFailures)
            .spawn_bfs()
            .join();
        checker.assert_discovery("confirmed_complete", complete_meeting());
        checker.assert_discovery("confirmed_with_text", complete_meeting());
    }

    #[test]
    fn a_failed_capture_confirms_as_incomplete() {
        let checker = SessionModel
            .checker()
            .finish_when(HasDiscoveries::AnyFailures)
            .spawn_bfs()
            .join();
        checker.assert_discovery("confirmed_incomplete", incomplete_meeting());
        checker.assert_discovery("empty_confirmed", incomplete_meeting());
    }

    #[test]
    fn losing_the_transcript_breaks_committed_retained() {
        let checker = LossyFinalize
            .checker()
            .finish_when(HasDiscoveries::AnyFailures)
            .spawn_bfs()
            .join();
        let discovery = checker.assert_any_discovery("committed_retained");
        println!("lossy finalize discovery: {discovery}");
        assert!(checker.unique_state_count() > 1);
    }

    #[test]
    fn leaking_a_revoked_source_breaks_capture_is_permitted() {
        let checker = LeakyRevoke
            .checker()
            .finish_when(HasDiscoveries::AnyFailures)
            .spawn_bfs()
            .join();
        let discovery = checker.assert_any_discovery("capture_is_permitted");
        println!("leaky revoke discovery: {discovery}");
        assert!(checker.unique_state_count() > 1);
    }
}
