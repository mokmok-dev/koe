//! Stateright model for the koe session lifecycle.
//!
//! [`SessionModel`] is a thin adapter: its actions come from
//! [`Session::enabled_actions`] and its transitions from [`Session::apply`],
//! so the model cannot drift from the code the app runs.
//!
//! Stateright properties only see one state, so the model state is a [`Step`]
//! that also records the previous session and the action taken. The state
//! invariants read the current session; the transition invariants read the
//! whole step, which catches an action that lands in a legal-looking but wrong
//! state. The [`FlawedModel`] controls deliberately break one rule at a time
//! to prove the properties are not vacuous.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use stateright::{Checker, HasDiscoveries, Model, Property};

use super::{
    Action, Audio, Permission, Permissions, Phase, Session, Source, SystemEvent, Transcription,
    UserAction,
};

/// Number of distinct steps reachable from the two initial sessions.
const EXPECTED_UNIQUE_STATES: usize = 468;

/// A model state: the current session plus the action that produced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Step {
    prev: Option<Session>,
    action: Option<Action>,
    session: Session,
}

const fn initial(session: Session) -> Step {
    Step {
        prev: None,
        action: None,
        session,
    }
}

const fn user(action: UserAction) -> Action {
    Action::User(action)
}

const fn system(event: SystemEvent) -> Action {
    Action::System(event)
}

const fn with_phase(prev: Session, phase: Phase) -> Session {
    Session {
        transcription_enabled: prev.transcription_enabled,
        permissions: prev.permissions,
        phase,
        transcription: prev.transcription,
        audio: prev.audio,
    }
}

const fn with_audio(prev: Session, audio: Audio) -> Session {
    Session {
        transcription_enabled: prev.transcription_enabled,
        permissions: prev.permissions,
        phase: prev.phase,
        transcription: prev.transcription,
        audio,
    }
}

const fn with_transcription(prev: Session, transcription: Transcription) -> Session {
    Session {
        transcription_enabled: prev.transcription_enabled,
        permissions: prev.permissions,
        phase: prev.phase,
        transcription,
        audio: prev.audio,
    }
}

const fn set_permission(
    permissions: Permissions,
    source: Source,
    permission: Permission,
) -> Permissions {
    match source {
        Source::Microphone => Permissions::new(permission, permissions.get(Source::SystemAudio)),
        Source::SystemAudio => Permissions::new(permissions.get(Source::Microphone), permission),
    }
}

const fn with_permission(prev: Session, source: Source, permission: Permission) -> Session {
    Session {
        transcription_enabled: prev.transcription_enabled,
        permissions: set_permission(prev.permissions, source, permission),
        phase: prev.phase,
        transcription: prev.transcription,
        audio: prev.audio,
    }
}

const fn transcription_matches_phase(state: Session) -> bool {
    match state.transcription() {
        Transcription::Disabled => !matches!(state.phase(), Phase::Deleted),
        Transcription::Live => matches!(state.phase(), Phase::Recording | Phase::Paused),
        Transcription::Finalizing => matches!(state.phase(), Phase::Stopping | Phase::Reviewing),
        Transcription::Draft | Transcription::Confirmed => {
            matches!(state.phase(), Phase::Reviewing)
        }
        Transcription::Failed => matches!(
            state.phase(),
            Phase::Recording | Phase::Paused | Phase::Stopping | Phase::Reviewing
        ),
        Transcription::Discarded => matches!(state.phase(), Phase::Reviewing | Phase::Deleted),
    }
}

const fn audio_matches_phase(state: Session) -> bool {
    match state.phase() {
        Phase::Idle | Phase::Preparing | Phase::Recording | Phase::Paused | Phase::Stopping => {
            matches!(state.audio(), Audio::None)
        }
        Phase::Reviewing => matches!(state.audio(), Audio::Saved),
        Phase::Deleted => matches!(state.audio(), Audio::Deleted),
    }
}

fn start_and_stop_outcomes(step: &Step) -> bool {
    let (Some(prev), Some(action)) = (step.prev, step.action) else {
        return true;
    };
    let next = step.session;
    match action {
        Action::User(UserAction::Start) => {
            matches!(prev.phase(), Phase::Idle) && next == with_phase(prev, Phase::Preparing)
        }
        Action::User(UserAction::Cancel) => {
            matches!(prev.phase(), Phase::Preparing) && next == with_phase(prev, Phase::Idle)
        }
        Action::User(UserAction::BeginCapture) => {
            matches!(prev.phase(), Phase::Preparing)
                && prev.permissions().all_granted()
                && next
                    == with_transcription(
                        with_phase(prev, Phase::Recording),
                        if prev.transcription_enabled() {
                            Transcription::Live
                        } else {
                            prev.transcription()
                        },
                    )
        }
        Action::User(UserAction::Pause) => {
            matches!(prev.phase(), Phase::Recording) && next == with_phase(prev, Phase::Paused)
        }
        Action::User(UserAction::Resume) => {
            matches!(prev.phase(), Phase::Paused) && next == with_phase(prev, Phase::Recording)
        }
        Action::User(UserAction::Stop) => {
            matches!(prev.phase(), Phase::Recording | Phase::Paused)
                && next
                    == with_transcription(
                        with_phase(prev, Phase::Stopping),
                        if matches!(prev.transcription(), Transcription::Live) {
                            Transcription::Finalizing
                        } else {
                            prev.transcription()
                        },
                    )
        }
        _ => true,
    }
}

fn permission_events_only_move_permissions(step: &Step) -> bool {
    let (Some(prev), Some(action)) = (step.prev, step.action) else {
        return true;
    };
    let next = step.session;
    match action {
        Action::System(SystemEvent::PermissionGranted(source)) => {
            matches!(
                prev.permissions().get(source),
                Permission::Requested | Permission::Denied
            ) && next == with_permission(prev, source, Permission::Granted)
        }
        Action::System(SystemEvent::PermissionDenied(source)) => {
            let permission = prev.permissions().get(source);
            if !matches!(permission, Permission::Requested | Permission::Granted) {
                return false;
            }
            let mut phase = prev.phase();
            let mut transcription = prev.transcription();
            if matches!(permission, Permission::Granted)
                && matches!(phase, Phase::Recording | Phase::Paused)
            {
                phase = Phase::Stopping;
                if matches!(transcription, Transcription::Live) {
                    transcription = Transcription::Finalizing;
                }
            }
            next == with_transcription(
                with_phase(with_permission(prev, source, Permission::Denied), phase),
                transcription,
            )
        }
        _ => true,
    }
}

fn capture_events_end_capture(step: &Step) -> bool {
    let (Some(prev), Some(action)) = (step.prev, step.action) else {
        return true;
    };
    let next = step.session;
    match action {
        Action::System(SystemEvent::CaptureClosed) => {
            matches!(prev.phase(), Phase::Stopping)
                && next == with_audio(with_phase(prev, Phase::Reviewing), Audio::Saved)
        }
        Action::System(SystemEvent::CaptureFailed) => match prev.phase() {
            Phase::Preparing => {
                matches!(prev.audio(), Audio::None) && next == with_phase(prev, Phase::Idle)
            }
            Phase::Recording | Phase::Paused | Phase::Stopping => {
                next == with_audio(
                    with_transcription(
                        with_phase(prev, Phase::Reviewing),
                        if matches!(prev.transcription(), Transcription::Live) {
                            Transcription::Failed
                        } else {
                            prev.transcription()
                        },
                    ),
                    Audio::Saved,
                )
            }
            Phase::Idle | Phase::Reviewing | Phase::Deleted => false,
        },
        _ => true,
    }
}

fn transcript_events_follow_capture(step: &Step) -> bool {
    let (Some(prev), Some(action)) = (step.prev, step.action) else {
        return true;
    };
    let next = step.session;
    match action {
        Action::System(SystemEvent::TranscriptFinalized) => {
            matches!(prev.phase(), Phase::Reviewing)
                && matches!(prev.transcription(), Transcription::Finalizing)
                && next == with_transcription(prev, Transcription::Draft)
        }
        Action::System(SystemEvent::TranscriptFailed) => {
            matches!(
                prev.transcription(),
                Transcription::Live | Transcription::Finalizing
            ) && next == with_transcription(prev, Transcription::Failed)
        }
        _ => true,
    }
}

fn review_actions_use_final_states(step: &Step) -> bool {
    let (Some(prev), Some(action)) = (step.prev, step.action) else {
        return true;
    };
    let next = step.session;
    match action {
        Action::User(UserAction::Confirm) => {
            matches!(prev.phase(), Phase::Reviewing)
                && matches!(prev.transcription(), Transcription::Draft)
                && next == with_transcription(prev, Transcription::Confirmed)
        }
        Action::User(UserAction::Discard) => {
            matches!(prev.phase(), Phase::Reviewing)
                && matches!(
                    prev.transcription(),
                    Transcription::Draft | Transcription::Failed
                )
                && next == with_transcription(prev, Transcription::Discarded)
        }
        Action::User(UserAction::Delete) => {
            matches!(prev.phase(), Phase::Reviewing)
                && next
                    == with_audio(
                        with_transcription(
                            with_phase(prev, Phase::Deleted),
                            Transcription::Discarded,
                        ),
                        Audio::Deleted,
                    )
        }
        _ => true,
    }
}

/// The property suite, shared by [`SessionModel`] and every [`FlawedModel`].
fn state_invariants<M>() -> Vec<Property<M>>
where
    M: Model<State = Step, Action = Action>,
{
    vec![
        Property::always("capture_needs_permissions", |_, step: &Step| {
            let state = step.session;
            !matches!(state.phase(), Phase::Recording | Phase::Paused)
                || state.permissions().all_granted()
        }),
        Property::always("live_transcript_needs_capture", |_, step: &Step| {
            let state = step.session;
            !matches!(state.transcription(), Transcription::Live)
                || matches!(state.phase(), Phase::Recording | Phase::Paused)
        }),
        Property::always("finalizing_needs_stop", |_, step: &Step| {
            let state = step.session;
            !matches!(state.transcription(), Transcription::Finalizing)
                || matches!(state.phase(), Phase::Stopping | Phase::Reviewing)
        }),
        Property::always("draft_needs_closed_capture", |_, step: &Step| {
            let state = step.session;
            !matches!(state.transcription(), Transcription::Draft)
                || (matches!(state.phase(), Phase::Reviewing)
                    && matches!(state.audio(), Audio::Saved))
        }),
        Property::always("confirmed_needs_review", |_, step: &Step| {
            let state = step.session;
            !matches!(state.transcription(), Transcription::Confirmed)
                || (matches!(state.phase(), Phase::Reviewing)
                    && matches!(state.audio(), Audio::Saved))
        }),
        Property::always("review_has_audio", |_, step: &Step| {
            let state = step.session;
            !matches!(state.phase(), Phase::Reviewing) || matches!(state.audio(), Audio::Saved)
        }),
        Property::always("deleted_is_clean", |_, step: &Step| {
            let state = step.session;
            !matches!(state.phase(), Phase::Deleted)
                || (matches!(state.audio(), Audio::Deleted)
                    && matches!(state.transcription(), Transcription::Discarded))
        }),
        Property::always("transcription_matches_phase", |_, step: &Step| {
            transcription_matches_phase(step.session)
        }),
        Property::always("audio_matches_phase", |_, step: &Step| {
            audio_matches_phase(step.session)
        }),
        Property::always("progress", |model: &M, step: &Step| {
            if step.session.is_terminal() {
                return true;
            }
            let mut actions = Vec::new();
            model.actions(step, &mut actions);
            !actions.is_empty()
        }),
    ]
}

fn transition_invariants<M>() -> Vec<Property<M>>
where
    M: Model<State = Step, Action = Action>,
{
    vec![
        Property::always("start_and_stop_outcomes", |_, step: &Step| {
            start_and_stop_outcomes(step)
        }),
        Property::always(
            "permission_events_only_move_permissions",
            |_, step: &Step| permission_events_only_move_permissions(step),
        ),
        Property::always("capture_events_end_capture", |_, step: &Step| {
            capture_events_end_capture(step)
        }),
        Property::always("transcript_events_follow_capture", |_, step: &Step| {
            transcript_events_follow_capture(step)
        }),
        Property::always("review_actions_use_final_states", |_, step: &Step| {
            review_actions_use_final_states(step)
        }),
    ]
}

fn reachability_properties<M>() -> Vec<Property<M>>
where
    M: Model<State = Step, Action = Action>,
{
    vec![
        Property::sometimes("can_record", |_, step: &Step| {
            matches!(step.session.phase(), Phase::Recording)
        }),
        Property::sometimes("can_pause", |_, step: &Step| {
            matches!(step.session.phase(), Phase::Paused)
        }),
        Property::sometimes("can_confirm", |_, step: &Step| {
            matches!(step.session.transcription(), Transcription::Confirmed)
        }),
        Property::sometimes("can_discard", |_, step: &Step| {
            matches!(step.session.transcription(), Transcription::Discarded)
        }),
        Property::sometimes("can_delete", |_, step: &Step| {
            matches!(step.session.phase(), Phase::Deleted)
        }),
        Property::sometimes(
            "can_keep_audio_after_transcript_failure",
            |_, step: &Step| {
                let state = step.session;
                matches!(state.phase(), Phase::Reviewing)
                    && matches!(state.transcription(), Transcription::Failed)
                    && matches!(state.audio(), Audio::Saved)
            },
        ),
        Property::sometimes("can_abort_after_revocation", |_, step: &Step| {
            let state = step.session;
            matches!(state.phase(), Phase::Reviewing)
                && matches!(state.audio(), Audio::Saved)
                && !state.permissions().all_granted()
        }),
        Property::sometimes("can_record_without_transcription", |_, step: &Step| {
            let state = step.session;
            matches!(state.phase(), Phase::Reviewing)
                && matches!(state.transcription(), Transcription::Disabled)
                && matches!(state.audio(), Audio::Saved)
        }),
    ]
}

fn session_properties<M>() -> Vec<Property<M>>
where
    M: Model<State = Step, Action = Action>,
{
    let mut properties = state_invariants();
    properties.extend(transition_invariants());
    properties.extend(reachability_properties());
    properties
}

/// The production machine: `init_states`, `actions`, and `next_state` call the
/// shipped code directly.
struct SessionModel;

impl Model for SessionModel {
    type State = Step;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![initial(Session::new(false)), initial(Session::new(true))]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        state.session.enabled_actions(actions);
    }

    fn next_state(&self, last_state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let next = last_state.session.apply(action)?;
        Some(Step {
            prev: Some(last_state.session),
            action: Some(action),
            session: next,
        })
    }

    fn properties(&self) -> Vec<Property<Self>> {
        session_properties()
    }
}

/// One deliberately broken rule. Each flaw is exposed as an extra action and
/// taken by [`FlawedModel::next_state`], bypassing [`Session::apply`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flaw {
    BeginCaptureWithoutPermission,
    FinalizeWhileStopping,
    ConfirmWhileRecording,
    IgnoreRevocation,
}

impl Flaw {
    const fn action(self) -> Action {
        match self {
            Self::BeginCaptureWithoutPermission => user(UserAction::BeginCapture),
            Self::FinalizeWhileStopping => system(SystemEvent::TranscriptFinalized),
            Self::ConfirmWhileRecording => user(UserAction::Confirm),
            Self::IgnoreRevocation => system(SystemEvent::PermissionDenied(Source::Microphone)),
        }
    }

    const fn applies_to(self, state: Session) -> bool {
        match self {
            Self::BeginCaptureWithoutPermission => {
                matches!(state.phase(), Phase::Preparing) && !state.permissions().all_granted()
            }
            Self::FinalizeWhileStopping => {
                matches!(state.phase(), Phase::Stopping)
                    && matches!(state.transcription(), Transcription::Finalizing)
            }
            Self::ConfirmWhileRecording => {
                matches!(state.phase(), Phase::Recording)
                    && matches!(state.transcription(), Transcription::Live)
            }
            Self::IgnoreRevocation => {
                matches!(state.phase(), Phase::Recording | Phase::Paused)
                    && matches!(
                        state.permissions().get(Source::Microphone),
                        Permission::Granted
                    )
            }
        }
    }

    const fn transition(self, state: Session) -> Session {
        match self {
            Self::BeginCaptureWithoutPermission => Session {
                transcription_enabled: state.transcription_enabled,
                permissions: state.permissions,
                phase: Phase::Recording,
                transcription: if state.transcription_enabled {
                    Transcription::Live
                } else {
                    Transcription::Disabled
                },
                audio: Audio::None,
            },
            Self::FinalizeWhileStopping => Session {
                transcription_enabled: state.transcription_enabled,
                permissions: state.permissions,
                phase: Phase::Stopping,
                transcription: Transcription::Draft,
                audio: Audio::None,
            },
            Self::ConfirmWhileRecording => Session {
                transcription_enabled: state.transcription_enabled,
                permissions: state.permissions,
                phase: Phase::Recording,
                transcription: Transcription::Confirmed,
                audio: Audio::None,
            },
            Self::IgnoreRevocation => state,
        }
    }
}

/// A copy of the production machine with exactly one rule broken.
struct FlawedModel {
    flaw: Flaw,
}

impl Model for FlawedModel {
    type State = Step;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![initial(Session::new(false)), initial(Session::new(true))]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        state.session.enabled_actions(actions);
        if self.flaw.applies_to(state.session) {
            actions.push(self.flaw.action());
        }
    }

    fn next_state(&self, last_state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let next = if action == self.flaw.action() && self.flaw.applies_to(last_state.session) {
            self.flaw.transition(last_state.session)
        } else {
            last_state.session.apply(action)?
        };
        Some(Step {
            prev: Some(last_state.session),
            action: Some(action),
            session: next,
        })
    }

    fn properties(&self) -> Vec<Property<Self>> {
        session_properties()
    }
}

fn ready_path() -> Vec<Action> {
    vec![
        user(UserAction::Start),
        user(UserAction::RequestPermission(Source::Microphone)),
        user(UserAction::RequestPermission(Source::SystemAudio)),
        system(SystemEvent::PermissionGranted(Source::Microphone)),
        system(SystemEvent::PermissionGranted(Source::SystemAudio)),
    ]
}

fn recording_path() -> Vec<Action> {
    let mut path = ready_path();
    path.push(user(UserAction::BeginCapture));
    path
}

#[test]
fn session_model_satisfies_every_property() {
    let checker = SessionModel.checker().threads(1).spawn_dfs().join();
    eprintln!(
        "unique_state_count={} state_count={} max_depth={}",
        checker.unique_state_count(),
        checker.state_count(),
        checker.max_depth()
    );
    checker.assert_properties();
    assert!(checker.is_done());
    assert_eq!(checker.unique_state_count(), EXPECTED_UNIQUE_STATES);

    checker.assert_discovery("can_record", recording_path());

    let mut path = recording_path();
    path.push(user(UserAction::Pause));
    checker.assert_discovery("can_pause", path);

    let mut path = recording_path();
    path.extend([
        user(UserAction::Stop),
        system(SystemEvent::CaptureClosed),
        system(SystemEvent::TranscriptFinalized),
        user(UserAction::Confirm),
    ]);
    checker.assert_discovery("can_confirm", path);

    let mut path = recording_path();
    path.extend([
        user(UserAction::Stop),
        system(SystemEvent::CaptureClosed),
        system(SystemEvent::TranscriptFinalized),
        user(UserAction::Discard),
    ]);
    checker.assert_discovery("can_discard", path);

    let mut path = recording_path();
    path.extend([
        user(UserAction::Stop),
        system(SystemEvent::CaptureClosed),
        user(UserAction::Delete),
    ]);
    checker.assert_discovery("can_delete", path);

    let mut path = recording_path();
    path.extend([
        system(SystemEvent::TranscriptFailed),
        user(UserAction::Stop),
        system(SystemEvent::CaptureClosed),
    ]);
    checker.assert_discovery("can_keep_audio_after_transcript_failure", path);

    // Revocation is exercised while Recording, so the forced-stop rule itself
    // (Recording -> Stopping, Live -> Finalizing) is what makes the state
    // reachable, not a denial applied after review.
    let mut path = recording_path();
    path.extend([
        system(SystemEvent::PermissionDenied(Source::SystemAudio)),
        system(SystemEvent::CaptureClosed),
    ]);
    checker.assert_discovery("can_abort_after_revocation", path);

    let mut path = recording_path();
    path.extend([user(UserAction::Stop), system(SystemEvent::CaptureClosed)]);
    checker.assert_discovery("can_record_without_transcription", path);
}

fn flawed(flaw: Flaw) -> impl Checker<FlawedModel> {
    FlawedModel { flaw }
        .checker()
        .threads(1)
        .finish_when(HasDiscoveries::AllFailures)
        .spawn_dfs()
        .join()
}

#[test]
fn begin_capture_without_permission_violates_capture_needs_permissions() {
    let checker = flawed(Flaw::BeginCaptureWithoutPermission);
    assert!(
        checker.discovery("capture_needs_permissions").is_some(),
        "the model must catch capture without permission"
    );
}

#[test]
fn finalize_while_stopping_violates_draft_needs_closed_capture() {
    let checker = flawed(Flaw::FinalizeWhileStopping);
    assert!(
        checker.discovery("draft_needs_closed_capture").is_some(),
        "the model must catch a draft transcript while capture is open"
    );
}

#[test]
fn confirm_while_recording_violates_confirmed_needs_review() {
    let checker = flawed(Flaw::ConfirmWhileRecording);
    assert!(
        checker.discovery("confirmed_needs_review").is_some(),
        "the model must catch a confirmed transcript before review"
    );
}

#[test]
fn ignoring_revocation_violates_permission_events_only_move_permissions() {
    let checker = flawed(Flaw::IgnoreRevocation);
    assert!(
        checker
            .discovery("permission_events_only_move_permissions")
            .is_some(),
        "the model must catch an ignored permission revocation"
    );
}
