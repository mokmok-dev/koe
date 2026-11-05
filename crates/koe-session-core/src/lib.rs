//! Meeting session lifecycle, encoded once and model checked.
//!
//! [`Session::apply`] is the only transition function. The app and the
//! test-only Stateright model in `src/stateright.rs` both call it, so the
//! checked machine is the shipped machine rather than a copy of it.
//!
//! # State
//!
//! A [`Session`] is the product of five fields:
//!
//! | Field | Type | Meaning |
//! | --- | --- | --- |
//! | `transcription_enabled` | `bool` | Whether the user configured transcription. It never changes. |
//! | `permissions` | [`Permissions`] | Last known OS permission per capture source. |
//! | `phase` | [`Phase`] | Where the session is in the recording lifecycle. |
//! | `transcription` | [`Transcription`] | Lifecycle of the transcript, not its content. |
//! | `audio` | [`Audio`] | Whether recorded audio is on disk. |
//!
//! # Actions
//!
//! [`Action::User`] commands come from the UI. [`Action::System`] events must
//! come only from OS reports and backend callbacks.
//!
//! | User actions | System events |
//! | --- | --- |
//! | [`UserAction::Start`] | [`SystemEvent::PermissionGranted`] |
//! | [`UserAction::RequestPermission`] | [`SystemEvent::PermissionDenied`] |
//! | [`UserAction::BeginCapture`] | [`SystemEvent::CaptureClosed`] |
//! | [`UserAction::Cancel`] | [`SystemEvent::CaptureFailed`] |
//! | [`UserAction::Pause`] | [`SystemEvent::TranscriptFinalized`] |
//! | [`UserAction::Resume`] | [`SystemEvent::TranscriptFailed`] |
//! | [`UserAction::Stop`] | |
//! | [`UserAction::Confirm`] | |
//! | [`UserAction::Discard`] | |
//! | [`UserAction::Delete`] | |
//!
//! There are 19 distinct actions once the source parameter is expanded.
//!
//! # Transitions
//!
//! | # | Action | From | To |
//! | --- | --- | --- | --- |
//! | 1 | `Start` | Idle | Preparing |
//! | 2 | `RequestPermission` | Idle or Preparing with Unknown/Denied | same phase, Requested |
//! | 3 | `BeginCapture` | Preparing with every permission granted | Recording, Live when transcription is enabled |
//! | 4 | `Cancel` | Preparing | Idle |
//! | 5 | `Pause` | Recording | Paused |
//! | 6 | `Resume` | Paused with every permission granted | Recording |
//! | 7 | `Stop` | Recording or Paused | Stopping, Live becomes Finalizing |
//! | 8 | `Confirm` | Reviewing with Draft | Confirmed |
//! | 9 | `Discard` | Reviewing with Draft or Failed | Discarded |
//! | 10 | `Delete` | Reviewing | Deleted, audio Deleted, transcript Discarded |
//! | 11 | `PermissionGranted` | permission Requested or Denied | permission Granted |
//! | 12 | `PermissionDenied` | permission Requested or Granted | permission Denied; a granted source revoked during Recording/Paused also forces Stopping and Finalizing |
//! | 13 | `CaptureClosed` | Stopping | Reviewing, audio Saved |
//! | 14 | `CaptureFailed` | Preparing | Idle, nothing was captured |
//! | 15 | `CaptureFailed` | Recording, Paused, Stopping | Reviewing, audio Saved, Live becomes Failed |
//! | 16 | `TranscriptFinalized` | Reviewing with Finalizing | Draft |
//! | 17 | `TranscriptFailed` | Recording/Paused/Stopping/Reviewing with Live/Finalizing | Failed |
//!
//! Every other combination returns `None`. Every action is rejected in
//! Deleted. `transcription_enabled` is never mutated.
//!
//! # Invariants
//!
//! The Stateright model checks these with `always`:
//!
//! | Name | Statement |
//! | --- | --- |
//! | `capture_needs_permissions` | Recording or Paused implies every permission is granted. |
//! | `live_transcript_needs_capture` | Live implies Recording or Paused. |
//! | `finalizing_needs_stop` | Finalizing implies Stopping or Reviewing. |
//! | `draft_needs_closed_capture` | Draft implies Reviewing and Saved audio. |
//! | `confirmed_needs_review` | Confirmed implies Reviewing and Saved audio. |
//! | `review_has_audio` | Reviewing implies Saved audio. |
//! | `deleted_is_clean` | Deleted implies Deleted audio and Discarded transcript. |
//! | `transcription_matches_phase` | Disabled in any non-Deleted phase, Live in Recording/Paused, Finalizing in Stopping/Reviewing, Draft/Confirmed in Reviewing, Failed in Recording/Paused/Stopping/Reviewing, Discarded in Reviewing/Deleted. |
//! | `audio_matches_phase` | None in Idle/Preparing/Recording/Paused/Stopping, Saved in Reviewing, Deleted in Deleted. |
//! | `progress` | Every non-Deleted state has an enabled action. |
//!
//! A Stateright property sees only one state, so the model wraps each session
//! with the previous session and the action taken. These transition invariants
//! check that every action produces exactly its row in the table above:
//!
//! | Name | Statement |
//! | --- | --- |
//! | `start_and_stop_outcomes` | Start, Cancel, `BeginCapture`, Pause, Resume, and Stop only produce their listed outcome. |
//! | `permission_events_only_move_permissions` | Permission events only move permissions, except a granted-source revocation during capture, which also forces Stopping. |
//! | `capture_events_end_capture` | `CaptureClosed` and `CaptureFailed` only produce their listed end-of-capture outcome. |
//! | `transcript_events_follow_capture` | `TranscriptFinalized` and `TranscriptFailed` only produce their listed transcription outcome. |
//! | `review_actions_use_final_states` | Confirm, Discard, and Delete only act on Reviewing and produce their listed outcome. |
//!
//! It checks these with `sometimes`: `can_record`, `can_pause`, `can_confirm`,
//! `can_discard`, `can_delete`, `can_keep_audio_after_transcript_failure`,
//! `can_abort_after_revocation`, and `can_record_without_transcription`. Each
//! one is paired with an exact action path, so the claimed behavior is
//! reachable through the transitions, not just through the state predicate.
//!
//! # Scope and assumptions
//!
//! - Both configured sources are required: no partial-source recording.
//! - Transcript content lives in `koe-transcribe-core`; this crate tracks the
//!   transcript lifecycle only.
//! - A confirmed transcript is terminal except for deletion. There is no edit
//!   after confirmation.
//! - A failed transcription is not retried; the recorded audio survives it.
//! - A device-open failure returns to Idle because nothing was captured.
//! - Events are atomic and may arrive in any order.
//! - The app must feed only actual OS permission reports into
//!   [`SystemEvent::PermissionGranted`] and [`SystemEvent::PermissionDenied`].
//!
//! # Model checking
//!
//! Run `cargo test -p koe-session-core`. The test explores every reachable
//! state and every ordering of the modeled actions, and asserts the state
//! invariants, the transition invariants, and the reachability properties
//! above. The model does not reimplement the machine: it calls
//! [`Session::enabled_actions`] and [`Session::apply`] for its transitions. The
//! transition invariants restate the expected outcome independently of
//! [`Session::apply`], so they are the oracle, not the machine.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;

use koe_capture_core::Source;

/// Last known OS permission for one capture source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Permission {
    /// The app has not asked the OS yet.
    Unknown,
    /// A permission request is in flight.
    Requested,
    /// The OS granted access.
    Granted,
    /// The OS or the user denied access.
    Denied,
}

/// Last known OS permission for each capture source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Permissions {
    microphone: Permission,
    system_audio: Permission,
}

impl Permissions {
    /// Builds the pair of permissions.
    #[must_use]
    pub const fn new(microphone: Permission, system_audio: Permission) -> Self {
        Self {
            microphone,
            system_audio,
        }
    }

    /// Returns the permission for `source`.
    #[must_use]
    pub const fn get(self, source: Source) -> Permission {
        match source {
            Source::Microphone => self.microphone,
            Source::SystemAudio => self.system_audio,
        }
    }

    /// Reports whether both sources are granted.
    #[must_use]
    pub const fn all_granted(self) -> bool {
        matches!(self.microphone, Permission::Granted)
            && matches!(self.system_audio, Permission::Granted)
    }

    const fn set(&mut self, source: Source, permission: Permission) {
        match source {
            Source::Microphone => self.microphone = permission,
            Source::SystemAudio => self.system_audio = permission,
        }
    }
}

/// Where a session is in the recording lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Nothing has started.
    Idle,
    /// Permissions are being resolved before capture.
    Preparing,
    /// Both sources are being captured.
    Recording,
    /// Capture is open but not advancing.
    Paused,
    /// Capture is being closed.
    Stopping,
    /// Capture is closed and its result can be reviewed.
    Reviewing,
    /// The session and its artifacts are gone.
    Deleted,
}

/// Lifecycle of the transcript, not its content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Transcription {
    /// The session was not configured to transcribe.
    Disabled,
    /// A transcript is being produced while capture runs.
    Live,
    /// Capture stopped and the transcript is being finalized.
    Finalizing,
    /// A final transcript exists and can be reviewed.
    Draft,
    /// The user confirmed the final transcript.
    Confirmed,
    /// Transcription failed; any recorded audio is unaffected.
    Failed,
    /// The transcript was discarded by the user.
    Discarded,
}

/// Whether recorded audio exists on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Audio {
    /// Nothing has been written.
    None,
    /// The recording is on disk.
    Saved,
    /// The recording was deleted.
    Deleted,
}

/// A command from the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UserAction {
    /// Enter the preparation phase.
    Start,
    /// Ask the OS for permission to capture `source`.
    RequestPermission(Source),
    /// Open both capture sources.
    BeginCapture,
    /// Abandon preparation without recording.
    Cancel,
    /// Suspend capture.
    Pause,
    /// Resume capture.
    Resume,
    /// Close capture and finalize the transcript.
    Stop,
    /// Accept the final transcript.
    Confirm,
    /// Throw the final transcript away.
    Discard,
    /// Remove the session, its audio, and its transcript.
    Delete,
}

/// A report from the OS or a capture/transcription backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SystemEvent {
    /// The OS granted `source`.
    PermissionGranted(Source),
    /// The OS or the user denied `source`.
    PermissionDenied(Source),
    /// Capture closed normally.
    CaptureClosed,
    /// Capture or device setup failed.
    CaptureFailed,
    /// The final transcript is ready for review.
    TranscriptFinalized,
    /// Transcription failed.
    TranscriptFailed,
}

/// A user command or a system event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// A command from the UI.
    User(UserAction),
    /// An event from the OS or a backend.
    System(SystemEvent),
}

/// Every action a session can ever receive, in a deterministic order.
///
/// [`Session::enabled_actions`] filters this list through [`Session::apply`],
/// so the two cannot disagree about what is legal.
const CANDIDATE_ACTIONS: [Action; 19] = [
    Action::User(UserAction::Start),
    Action::User(UserAction::RequestPermission(Source::Microphone)),
    Action::User(UserAction::RequestPermission(Source::SystemAudio)),
    Action::User(UserAction::BeginCapture),
    Action::User(UserAction::Cancel),
    Action::User(UserAction::Pause),
    Action::User(UserAction::Resume),
    Action::User(UserAction::Stop),
    Action::User(UserAction::Confirm),
    Action::User(UserAction::Discard),
    Action::User(UserAction::Delete),
    Action::System(SystemEvent::PermissionGranted(Source::Microphone)),
    Action::System(SystemEvent::PermissionGranted(Source::SystemAudio)),
    Action::System(SystemEvent::PermissionDenied(Source::Microphone)),
    Action::System(SystemEvent::PermissionDenied(Source::SystemAudio)),
    Action::System(SystemEvent::CaptureClosed),
    Action::System(SystemEvent::CaptureFailed),
    Action::System(SystemEvent::TranscriptFinalized),
    Action::System(SystemEvent::TranscriptFailed),
];

/// The complete state of one meeting session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Session {
    transcription_enabled: bool,
    permissions: Permissions,
    phase: Phase,
    transcription: Transcription,
    audio: Audio,
}

impl Session {
    /// Creates an Idle session with no permission resolved and nothing recorded.
    #[must_use]
    pub const fn new(transcription_enabled: bool) -> Self {
        Self {
            transcription_enabled,
            permissions: Permissions::new(Permission::Unknown, Permission::Unknown),
            phase: Phase::Idle,
            transcription: Transcription::Disabled,
            audio: Audio::None,
        }
    }

    /// Whether transcription was configured for this session.
    #[must_use]
    pub const fn transcription_enabled(&self) -> bool {
        self.transcription_enabled
    }

    /// Last known permissions.
    #[must_use]
    pub const fn permissions(&self) -> Permissions {
        self.permissions
    }

    /// Current lifecycle phase.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// Current transcript lifecycle.
    #[must_use]
    pub const fn transcription(&self) -> Transcription {
        self.transcription
    }

    /// Whether audio exists on disk.
    #[must_use]
    pub const fn audio(&self) -> Audio {
        self.audio
    }

    /// Whether the session is Deleted and therefore terminal.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self.phase, Phase::Deleted)
    }

    /// Applies one action, returning the next state or `None` when the action
    /// is not legal in this state.
    ///
    /// Every action is rejected once the session is Deleted.
    #[must_use]
    pub const fn apply(&self, action: Action) -> Option<Self> {
        if matches!(self.phase, Phase::Deleted) {
            return None;
        }
        match action {
            Action::User(action) => self.apply_user(action),
            Action::System(event) => self.apply_system(event),
        }
    }

    const fn apply_user(self, action: UserAction) -> Option<Self> {
        let mut next = self;
        match action {
            UserAction::Start if matches!(self.phase, Phase::Idle) => {
                next.phase = Phase::Preparing;
            }
            UserAction::RequestPermission(source)
                if matches!(self.phase, Phase::Idle | Phase::Preparing)
                    && matches!(
                        self.permissions.get(source),
                        Permission::Unknown | Permission::Denied
                    ) =>
            {
                next.permissions.set(source, Permission::Requested);
            }
            UserAction::BeginCapture
                if matches!(self.phase, Phase::Preparing) && self.permissions.all_granted() =>
            {
                next.phase = Phase::Recording;
                if self.transcription_enabled {
                    next.transcription = Transcription::Live;
                }
            }
            UserAction::Cancel if matches!(self.phase, Phase::Preparing) => {
                next.phase = Phase::Idle;
            }
            UserAction::Pause if matches!(self.phase, Phase::Recording) => {
                next.phase = Phase::Paused;
            }
            UserAction::Resume
                if matches!(self.phase, Phase::Paused) && self.permissions.all_granted() =>
            {
                next.phase = Phase::Recording;
            }
            UserAction::Stop if matches!(self.phase, Phase::Recording | Phase::Paused) => {
                next.phase = Phase::Stopping;
                if matches!(self.transcription, Transcription::Live) {
                    next.transcription = Transcription::Finalizing;
                }
            }
            UserAction::Confirm
                if matches!(self.phase, Phase::Reviewing)
                    && matches!(self.transcription, Transcription::Draft) =>
            {
                next.transcription = Transcription::Confirmed;
            }
            UserAction::Discard
                if matches!(self.phase, Phase::Reviewing)
                    && matches!(
                        self.transcription,
                        Transcription::Draft | Transcription::Failed
                    ) =>
            {
                next.transcription = Transcription::Discarded;
            }
            UserAction::Delete if matches!(self.phase, Phase::Reviewing) => {
                next.phase = Phase::Deleted;
                next.audio = Audio::Deleted;
                next.transcription = Transcription::Discarded;
            }
            UserAction::Start
            | UserAction::RequestPermission(_)
            | UserAction::BeginCapture
            | UserAction::Cancel
            | UserAction::Pause
            | UserAction::Resume
            | UserAction::Stop
            | UserAction::Confirm
            | UserAction::Discard
            | UserAction::Delete => return None,
        }
        Some(next)
    }

    const fn apply_system(self, event: SystemEvent) -> Option<Self> {
        let mut next = self;
        match event {
            SystemEvent::PermissionGranted(source)
                if matches!(
                    self.permissions.get(source),
                    Permission::Requested | Permission::Denied
                ) =>
            {
                next.permissions.set(source, Permission::Granted);
            }
            SystemEvent::PermissionDenied(source)
                if matches!(
                    self.permissions.get(source),
                    Permission::Requested | Permission::Granted
                ) =>
            {
                let was_granted = matches!(self.permissions.get(source), Permission::Granted);
                next.permissions.set(source, Permission::Denied);
                if was_granted && matches!(self.phase, Phase::Recording | Phase::Paused) {
                    next.phase = Phase::Stopping;
                    if matches!(self.transcription, Transcription::Live) {
                        next.transcription = Transcription::Finalizing;
                    }
                }
            }
            SystemEvent::CaptureClosed if matches!(self.phase, Phase::Stopping) => {
                next.phase = Phase::Reviewing;
                next.audio = Audio::Saved;
            }
            SystemEvent::CaptureFailed => match self.phase {
                Phase::Preparing => next.phase = Phase::Idle,
                Phase::Recording | Phase::Paused | Phase::Stopping => {
                    next.phase = Phase::Reviewing;
                    next.audio = Audio::Saved;
                    if matches!(self.transcription, Transcription::Live) {
                        next.transcription = Transcription::Failed;
                    }
                }
                Phase::Idle | Phase::Reviewing | Phase::Deleted => return None,
            },
            SystemEvent::TranscriptFinalized
                if matches!(self.phase, Phase::Reviewing)
                    && matches!(self.transcription, Transcription::Finalizing) =>
            {
                next.transcription = Transcription::Draft;
            }
            SystemEvent::TranscriptFailed
                if matches!(
                    self.phase,
                    Phase::Recording | Phase::Paused | Phase::Stopping | Phase::Reviewing
                ) && matches!(
                    self.transcription,
                    Transcription::Live | Transcription::Finalizing
                ) =>
            {
                next.transcription = Transcription::Failed;
            }
            SystemEvent::PermissionGranted(_)
            | SystemEvent::PermissionDenied(_)
            | SystemEvent::CaptureClosed
            | SystemEvent::TranscriptFinalized
            | SystemEvent::TranscriptFailed => return None,
        }
        Some(next)
    }

    /// Appends every action that [`Session::apply`] accepts in this state.
    ///
    /// The result is always a subset of the crate's fixed candidate list, and
    /// it is computed by applying each candidate, so `enabled_actions` and
    /// `apply` cannot drift apart.
    pub fn enabled_actions(&self, out: &mut Vec<Action>) {
        for action in CANDIDATE_ACTIONS {
            if self.apply(action).is_some() {
                out.push(action);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const ALL_GRANTED: Permissions = Permissions::new(Permission::Granted, Permission::Granted);

    fn user(action: UserAction) -> Action {
        Action::User(action)
    }

    fn system(event: SystemEvent) -> Action {
        Action::System(event)
    }

    fn step(session: Session, action: Action) -> Session {
        session.apply(action).unwrap()
    }

    fn requested() -> Session {
        let session = step(Session::new(false), user(UserAction::Start));
        let session = step(
            session,
            user(UserAction::RequestPermission(Source::Microphone)),
        );
        step(
            session,
            user(UserAction::RequestPermission(Source::SystemAudio)),
        )
    }

    fn ready(transcription_enabled: bool) -> Session {
        let session = step(Session::new(transcription_enabled), user(UserAction::Start));
        let session = step(
            session,
            user(UserAction::RequestPermission(Source::Microphone)),
        );
        let session = step(
            session,
            user(UserAction::RequestPermission(Source::SystemAudio)),
        );
        let session = step(
            session,
            system(SystemEvent::PermissionGranted(Source::Microphone)),
        );
        step(
            session,
            system(SystemEvent::PermissionGranted(Source::SystemAudio)),
        )
    }

    fn recording(transcription_enabled: bool) -> Session {
        step(ready(transcription_enabled), user(UserAction::BeginCapture))
    }

    fn stopping(transcription_enabled: bool) -> Session {
        step(recording(transcription_enabled), user(UserAction::Stop))
    }

    fn reviewing(transcription_enabled: bool) -> Session {
        step(
            stopping(transcription_enabled),
            system(SystemEvent::CaptureClosed),
        )
    }

    fn draft() -> Session {
        step(reviewing(true), system(SystemEvent::TranscriptFinalized))
    }

    #[test]
    fn new_session_is_idle_and_empty() {
        for enabled in [false, true] {
            let session = Session::new(enabled);
            assert_eq!(session.transcription_enabled(), enabled);
            assert_eq!(
                session.permissions().get(Source::Microphone),
                Permission::Unknown
            );
            assert_eq!(
                session.permissions().get(Source::SystemAudio),
                Permission::Unknown
            );
            assert_eq!(session.phase(), Phase::Idle);
            assert_eq!(session.transcription(), Transcription::Disabled);
            assert_eq!(session.audio(), Audio::None);
            assert!(!session.is_terminal());
            assert!(!session.permissions().all_granted());
        }
    }

    #[test]
    fn start_only_moves_idle_to_preparing() {
        let idle = Session::new(false);
        let preparing = step(idle, user(UserAction::Start));
        assert_eq!(preparing.phase(), Phase::Preparing);
        assert_eq!(idle.apply(user(UserAction::Start)), Some(preparing));
        assert_eq!(preparing.apply(user(UserAction::Start)), None);
        assert_eq!(recording(false).apply(user(UserAction::Start)), None);
    }

    #[test]
    fn permission_requests_only_cover_unknown_and_denied() {
        let idle = Session::new(false);
        let requested = step(
            idle,
            user(UserAction::RequestPermission(Source::Microphone)),
        );
        assert_eq!(
            idle.apply(user(UserAction::RequestPermission(Source::Microphone))),
            Some(requested)
        );
        assert_eq!(
            requested.permissions().get(Source::Microphone),
            Permission::Requested
        );
        assert_eq!(
            requested.permissions().get(Source::SystemAudio),
            Permission::Unknown
        );
        assert_eq!(requested.phase(), Phase::Idle);
        assert_eq!(requested.transcription(), Transcription::Disabled);
        assert_eq!(requested.audio(), Audio::None);
        assert_eq!(
            requested.apply(user(UserAction::RequestPermission(Source::Microphone))),
            None
        );
        let denied = step(
            requested,
            system(SystemEvent::PermissionDenied(Source::Microphone)),
        );
        let re_requested = step(
            denied,
            user(UserAction::RequestPermission(Source::Microphone)),
        );
        assert_eq!(
            re_requested.permissions().get(Source::Microphone),
            Permission::Requested
        );
        let granted = step(
            re_requested,
            system(SystemEvent::PermissionGranted(Source::Microphone)),
        );
        assert_eq!(
            granted.apply(user(UserAction::RequestPermission(Source::Microphone))),
            None
        );
        assert_eq!(
            recording(false).apply(user(UserAction::RequestPermission(Source::Microphone))),
            None
        );
    }

    #[test]
    fn permission_events_follow_the_request() {
        let idle = Session::new(false);
        assert_eq!(
            idle.apply(system(SystemEvent::PermissionGranted(Source::Microphone))),
            None
        );
        assert_eq!(
            idle.apply(system(SystemEvent::PermissionDenied(Source::Microphone))),
            None
        );
        let requested = step(
            idle,
            user(UserAction::RequestPermission(Source::Microphone)),
        );
        let granted = step(
            requested,
            system(SystemEvent::PermissionGranted(Source::Microphone)),
        );
        assert_eq!(
            granted.permissions().get(Source::Microphone),
            Permission::Granted
        );
        assert_eq!(
            granted.apply(system(SystemEvent::PermissionGranted(Source::Microphone))),
            None
        );
        let denied = step(
            granted,
            system(SystemEvent::PermissionDenied(Source::Microphone)),
        );
        assert_eq!(
            denied.permissions().get(Source::Microphone),
            Permission::Denied
        );
        assert_eq!(denied.phase(), Phase::Idle);
        let regranted = step(
            denied,
            system(SystemEvent::PermissionGranted(Source::Microphone)),
        );
        assert_eq!(
            regranted.permissions().get(Source::Microphone),
            Permission::Granted
        );
    }

    #[test]
    fn begin_capture_requires_every_permission() {
        let idle = Session::new(false);
        assert_eq!(idle.apply(user(UserAction::BeginCapture)), None);
        let preparing = step(idle, user(UserAction::Start));
        assert_eq!(preparing.apply(user(UserAction::BeginCapture)), None);
        let only_microphone = step(
            step(
                step(
                    preparing,
                    user(UserAction::RequestPermission(Source::Microphone)),
                ),
                system(SystemEvent::PermissionGranted(Source::Microphone)),
            ),
            user(UserAction::RequestPermission(Source::SystemAudio)),
        );
        assert_eq!(only_microphone.apply(user(UserAction::BeginCapture)), None);
        assert_eq!(
            ready(false).apply(user(UserAction::BeginCapture)),
            Some(recording(false))
        );
    }

    #[test]
    fn begin_capture_rejects_a_denied_source() {
        let preparing = step(Session::new(false), user(UserAction::Start));
        let session = step(
            preparing,
            user(UserAction::RequestPermission(Source::Microphone)),
        );
        let session = step(
            session,
            system(SystemEvent::PermissionDenied(Source::Microphone)),
        );
        let session = step(
            session,
            user(UserAction::RequestPermission(Source::SystemAudio)),
        );
        let session = step(
            session,
            system(SystemEvent::PermissionGranted(Source::SystemAudio)),
        );
        assert_eq!(
            session.permissions().get(Source::Microphone),
            Permission::Denied
        );
        assert_eq!(session.apply(user(UserAction::BeginCapture)), None);
    }

    #[test]
    fn capture_transcribes_live_only_when_configured() {
        assert_eq!(recording(false).transcription(), Transcription::Disabled);
        assert_eq!(recording(true).transcription(), Transcription::Live);
    }

    #[test]
    fn cancel_returns_to_idle_before_capture() {
        let preparing = step(Session::new(false), user(UserAction::Start));
        let cancelled = step(preparing, user(UserAction::Cancel));
        assert_eq!(cancelled.phase(), Phase::Idle);
        assert_eq!(cancelled.permissions(), preparing.permissions());
        assert_eq!(recording(false).apply(user(UserAction::Cancel)), None);
        assert_eq!(cancelled.apply(user(UserAction::Cancel)), None);
    }

    #[test]
    fn pause_and_resume_cycle_the_recording_phase() {
        let paused = step(recording(true), user(UserAction::Pause));
        assert_eq!(paused.phase(), Phase::Paused);
        assert_eq!(paused.transcription(), Transcription::Live);
        let resumed = step(paused, user(UserAction::Resume));
        assert_eq!(resumed.phase(), Phase::Recording);
        assert_eq!(resumed, recording(true));
        assert_eq!(recording(false).apply(user(UserAction::Resume)), None);
        assert_eq!(paused.apply(user(UserAction::Pause)), None);
    }

    #[test]
    fn resume_requires_every_permission_again() {
        let mut revoked = recording(true);
        revoked
            .permissions
            .set(Source::SystemAudio, Permission::Denied);
        revoked.phase = Phase::Paused;
        assert_eq!(revoked.apply(user(UserAction::Resume)), None);
    }

    #[test]
    fn stop_enters_stopping_and_finalizes_live_transcription() {
        let stopped = step(recording(true), user(UserAction::Stop));
        assert_eq!(stopped.phase(), Phase::Stopping);
        assert_eq!(stopped.transcription(), Transcription::Finalizing);
        let stopped_without_transcription = step(recording(false), user(UserAction::Stop));
        assert_eq!(stopped_without_transcription.phase(), Phase::Stopping);
        assert_eq!(
            stopped_without_transcription.transcription(),
            Transcription::Disabled
        );
        let paused = step(recording(true), user(UserAction::Pause));
        assert_eq!(
            step(paused, user(UserAction::Stop)).phase(),
            Phase::Stopping
        );
        assert_eq!(ready(true).apply(user(UserAction::Stop)), None);
        assert_eq!(stopped.apply(user(UserAction::Stop)), None);
    }

    #[test]
    fn revocation_during_recording_forces_a_stop() {
        let revoked = step(
            recording(true),
            system(SystemEvent::PermissionDenied(Source::Microphone)),
        );
        assert_eq!(revoked.phase(), Phase::Stopping);
        assert_eq!(revoked.transcription(), Transcription::Finalizing);
        assert_eq!(
            revoked.permissions().get(Source::Microphone),
            Permission::Denied
        );
    }

    #[test]
    fn revocation_during_pause_forces_a_stop() {
        let paused = step(recording(true), user(UserAction::Pause));
        let revoked = step(
            paused,
            system(SystemEvent::PermissionDenied(Source::SystemAudio)),
        );
        assert_eq!(revoked.phase(), Phase::Stopping);
        assert_eq!(revoked.transcription(), Transcription::Finalizing);
    }

    #[test]
    fn revocation_while_preparing_does_not_stop() {
        let preparing = step(Session::new(false), user(UserAction::Start));
        let requested = step(
            preparing,
            user(UserAction::RequestPermission(Source::Microphone)),
        );
        let denied = step(
            requested,
            system(SystemEvent::PermissionDenied(Source::Microphone)),
        );
        assert_eq!(denied.phase(), Phase::Preparing);
        assert_eq!(
            denied.permissions().get(Source::Microphone),
            Permission::Denied
        );
    }

    #[test]
    fn capture_closed_enters_review_with_audio() {
        let reviewed = reviewing(true);
        assert_eq!(reviewed.phase(), Phase::Reviewing);
        assert_eq!(reviewed.audio(), Audio::Saved);
        assert_eq!(reviewed.transcription(), Transcription::Finalizing);
        assert_eq!(reviewing(false).transcription(), Transcription::Disabled);
        assert_eq!(
            recording(false).apply(system(SystemEvent::CaptureClosed)),
            None
        );
        assert_eq!(reviewed.apply(system(SystemEvent::CaptureClosed)), None);
    }

    #[test]
    fn capture_failure_before_capture_returns_to_idle() {
        let preparing = ready(false);
        let failed = step(preparing, system(SystemEvent::CaptureFailed));
        assert_eq!(failed.phase(), Phase::Idle);
        assert_eq!(failed.audio(), Audio::None);
        assert_eq!(failed.permissions(), ALL_GRANTED);
        assert_eq!(
            Session::new(false).apply(system(SystemEvent::CaptureFailed)),
            None
        );
        assert_eq!(
            reviewing(false).apply(system(SystemEvent::CaptureFailed)),
            None
        );
    }

    #[test]
    fn capture_failure_keeps_audio_and_fails_live_transcription() {
        let failed = step(recording(true), system(SystemEvent::CaptureFailed));
        assert_eq!(failed.phase(), Phase::Reviewing);
        assert_eq!(failed.audio(), Audio::Saved);
        assert_eq!(failed.transcription(), Transcription::Failed);
        let without_transcription = step(recording(false), system(SystemEvent::CaptureFailed));
        assert_eq!(
            without_transcription.transcription(),
            Transcription::Disabled
        );
    }

    #[test]
    fn capture_failure_while_finalizing_keeps_finalizing() {
        let failed = step(stopping(true), system(SystemEvent::CaptureFailed));
        assert_eq!(failed.phase(), Phase::Reviewing);
        assert_eq!(failed.audio(), Audio::Saved);
        assert_eq!(failed.transcription(), Transcription::Finalizing);
    }

    #[test]
    fn transcript_finalizes_only_after_capture_closed() {
        let finalized = step(reviewing(true), system(SystemEvent::TranscriptFinalized));
        assert_eq!(finalized.transcription(), Transcription::Draft);
        assert_eq!(finalized.phase(), Phase::Reviewing);
        assert_eq!(
            stopping(true).apply(system(SystemEvent::TranscriptFinalized)),
            None
        );
        assert_eq!(
            reviewing(false).apply(system(SystemEvent::TranscriptFinalized)),
            None
        );
    }

    #[test]
    fn transcript_failure_marks_live_and_finalizing() {
        assert_eq!(
            step(recording(true), system(SystemEvent::TranscriptFailed)).transcription(),
            Transcription::Failed
        );
        assert_eq!(
            step(stopping(true), system(SystemEvent::TranscriptFailed)).transcription(),
            Transcription::Failed
        );
        let reviewed_failed = step(reviewing(true), system(SystemEvent::TranscriptFailed));
        assert_eq!(reviewed_failed.transcription(), Transcription::Failed);
        assert_eq!(reviewed_failed.phase(), Phase::Reviewing);
        assert_eq!(reviewed_failed.audio(), Audio::Saved);
        assert_eq!(draft().apply(system(SystemEvent::TranscriptFailed)), None);
        assert_eq!(
            recording(false).apply(system(SystemEvent::TranscriptFailed)),
            None
        );
    }

    #[test]
    fn confirm_accepts_only_a_reviewed_draft() {
        let confirmed = step(draft(), user(UserAction::Confirm));
        assert_eq!(confirmed.transcription(), Transcription::Confirmed);
        assert_eq!(confirmed.phase(), Phase::Reviewing);
        assert_eq!(confirmed.audio(), Audio::Saved);
        for session in [
            recording(true),
            stopping(true),
            reviewing(true),
            reviewing(false),
            step(reviewing(true), system(SystemEvent::TranscriptFailed)),
            confirmed,
        ] {
            assert_eq!(session.apply(user(UserAction::Confirm)), None);
        }
    }

    #[test]
    fn discard_accepts_only_a_reviewed_draft_or_failure() {
        let discarded = step(draft(), user(UserAction::Discard));
        assert_eq!(discarded.transcription(), Transcription::Discarded);
        assert_eq!(discarded.phase(), Phase::Reviewing);
        let failed = step(reviewing(true), system(SystemEvent::TranscriptFailed));
        assert_eq!(
            step(failed, user(UserAction::Discard)).transcription(),
            Transcription::Discarded
        );
        for session in [
            recording(true),
            stopping(true),
            reviewing(true),
            reviewing(false),
            step(draft(), user(UserAction::Confirm)),
        ] {
            assert_eq!(session.apply(user(UserAction::Discard)), None);
        }
    }

    #[test]
    fn delete_is_review_only_and_clears_everything() {
        let deleted = step(reviewing(false), user(UserAction::Delete));
        assert_eq!(deleted.phase(), Phase::Deleted);
        assert_eq!(deleted.audio(), Audio::Deleted);
        assert_eq!(deleted.transcription(), Transcription::Discarded);
        assert!(deleted.is_terminal());
        for session in [
            Session::new(false),
            ready(false),
            recording(true),
            stopping(true),
        ] {
            assert_eq!(session.apply(user(UserAction::Delete)), None);
        }
    }

    #[test]
    fn deleted_rejects_every_action_and_offers_none() {
        let deleted = step(reviewing(false), user(UserAction::Delete));
        for action in CANDIDATE_ACTIONS {
            assert_eq!(deleted.apply(action), None);
        }
        let mut enabled = Vec::new();
        deleted.enabled_actions(&mut enabled);
        assert_eq!(enabled.len(), 0);
    }

    #[test]
    fn enabled_actions_offer_begin_capture_when_ready() {
        let mut enabled = Vec::new();
        ready(true).enabled_actions(&mut enabled);
        assert!(enabled.contains(&user(UserAction::BeginCapture)));
        assert!(enabled.contains(&user(UserAction::Cancel)));
        assert!(!enabled.contains(&user(UserAction::Start)));
        let mut recording_actions = Vec::new();
        recording(true).enabled_actions(&mut recording_actions);
        assert!(recording_actions.contains(&user(UserAction::Pause)));
        assert!(recording_actions.contains(&user(UserAction::Stop)));
        assert!(!recording_actions.contains(&user(UserAction::BeginCapture)));
        assert_eq!(recording_actions.len(), 6);
    }

    #[test]
    fn enabled_actions_are_always_accepted_by_apply() {
        for enabled in [false, true] {
            for session in [
                Session::new(enabled),
                ready(enabled),
                recording(enabled),
                stopping(enabled),
                reviewing(enabled),
            ] {
                let mut actions = Vec::new();
                session.enabled_actions(&mut actions);
                assert_ne!(actions.len(), 0);
                for action in actions {
                    assert!(session.apply(action).is_some());
                }
            }
        }
    }

    #[test]
    fn candidate_list_covers_every_action() {
        assert_eq!(CANDIDATE_ACTIONS.len(), 19);
        let paused = step(recording(true), user(UserAction::Pause));
        let confirmed = step(draft(), user(UserAction::Confirm));
        let deleted = step(reviewing(false), user(UserAction::Delete));
        let samples = [
            Session::new(true),
            requested(),
            ready(true),
            recording(true),
            paused,
            stopping(true),
            reviewing(true),
            draft(),
            confirmed,
            deleted,
        ];
        let mut seen = Vec::new();
        for session in samples {
            let mut actions = Vec::new();
            session.enabled_actions(&mut actions);
            for action in actions {
                if !seen.contains(&action) {
                    seen.push(action);
                }
            }
        }
        assert_eq!(seen.len(), 19);
    }
}

#[cfg(test)]
mod stateright;
