//! The session state machine.

use alloc::vec::Vec;
use core::fmt;

use koe_capture_core::Source;

use crate::input::{Effect, Input};
use crate::permission::{Permission, Permissions};
use crate::sources::SourceSet;
use crate::transcript::{Fault, Outcome, SegmentOutcome, SegmentResult, Transcript};

/// Where a meeting is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
    /// No meeting.
    Idle,
    /// Permissions are being settled and the first devices are opening.
    Preparing,
    /// At least one source is capturing.
    Recording,
    /// The meeting is on hold and keeps its transcript.
    Paused,
    /// Capture is closed and the transcription sessions are finishing.
    Closing,
    /// The transcript is ready for the user to confirm.
    Reviewing,
    /// The user confirmed the transcript.
    Confirmed,
}

impl Stage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Preparing => "preparing",
            Self::Recording => "recording",
            Self::Paused => "paused",
            Self::Closing => "closing",
            Self::Reviewing => "reviewing",
            Self::Confirmed => "confirmed",
        }
    }
}

/// Why an input was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rejected {
    /// The input does not apply to the current stage.
    WrongStage(Stage),
    /// A meeting must record at least one source.
    NoSources,
    /// The source's permission is denied, so the meeting cannot start.
    PermissionDenied(Source),
    /// The source's permission state no longer allows the input.
    PermissionOutOfOrder {
        /// The source whose permission disagrees.
        source: Source,
        /// The permission the source currently has.
        permission: Permission,
    },
    /// No device open is outstanding for the source.
    SourceNotPending(Source),
    /// The source is not capturing.
    SourceNotActive(Source),
    /// No transcription session for the source is finishing.
    SourceNotFinishing(Source),
}

impl fmt::Display for Rejected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongStage(stage) => {
                write!(formatter, "input is not allowed while {}", stage.as_str())
            }
            Self::NoSources => formatter.write_str("a meeting needs at least one source"),
            Self::PermissionDenied(source) => write!(formatter, "{source} permission is denied"),
            Self::PermissionOutOfOrder { source, permission } => {
                write!(formatter, "{source} permission is {permission}")
            }
            Self::SourceNotPending(source) => write!(formatter, "{source} has no device opening"),
            Self::SourceNotActive(source) => write!(formatter, "{source} is not capturing"),
            Self::SourceNotFinishing(source) => {
                write!(formatter, "{source} has no transcription session finishing")
            }
        }
    }
}

impl core::error::Error for Rejected {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Progress {
    pub transcript: Transcript,
    pub outcome: Outcome,
}

impl Progress {
    const fn new() -> Self {
        Self {
            transcript: Transcript::new(),
            outcome: Outcome::Complete,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    Fresh,
    Resume(Progress),
}

impl Origin {
    /// The progress a `Recording` inherits when it starts capturing.
    ///
    /// A resumed meeting keeps the transcript it already committed, so the
    /// progress a `Resume` carried must survive the preparation.
    const fn carried(self) -> Progress {
        match self {
            Self::Fresh => Progress::new(),
            Self::Resume(progress) => progress,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Preparing {
    pub sources: SourceSet,
    pub pending: SourceSet,
    pub open: SourceSet,
    pub origin: Origin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Recording {
    pub sources: SourceSet,
    pub open: SourceSet,
    pub finishing: SourceSet,
    pub progress: Progress,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Paused {
    pub sources: SourceSet,
    pub progress: Progress,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Next {
    Pause,
    Stop,
}

#[derive(Clone, Copy)]
enum PreparingFailure {
    Source,
    Engine,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Closing {
    pub sources: SourceSet,
    pub finishing: SourceSet,
    pub progress: Progress,
    pub next: Next,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Reviewing {
    pub sources: SourceSet,
    pub progress: Progress,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Confirmed {
    pub sources: SourceSet,
    pub progress: Progress,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Phase {
    Idle,
    Preparing(Preparing),
    Recording(Recording),
    Paused(Paused),
    Closing(Closing),
    Reviewing(Reviewing),
    Confirmed(Confirmed),
}

impl Phase {
    const fn stage(self) -> Stage {
        match self {
            Self::Idle => Stage::Idle,
            Self::Preparing(_) => Stage::Preparing,
            Self::Recording(_) => Stage::Recording,
            Self::Paused(_) => Stage::Paused,
            Self::Closing(_) => Stage::Closing,
            Self::Reviewing(_) => Stage::Reviewing,
            Self::Confirmed(_) => Stage::Confirmed,
        }
    }
}

/// One accepted input, and the work it asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// The session after the input.
    pub session: Session,
    /// Work to run, in order.
    pub effects: Vec<Effect>,
}

/// The recording, permission, and transcription state of one meeting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Session {
    pub(crate) permissions: Permissions,
    pub(crate) phase: Phase,
}

impl Session {
    /// Starts idle, with both permissions as given.
    #[must_use]
    pub const fn new(permissions: Permissions) -> Self {
        Self {
            permissions,
            phase: Phase::Idle,
        }
    }

    /// The current stage.
    #[must_use]
    pub const fn stage(&self) -> Stage {
        self.phase.stage()
    }

    /// The permission of every source.
    #[must_use]
    pub const fn permissions(&self) -> Permissions {
        self.permissions
    }

    /// The permission of `source`.
    #[must_use]
    pub const fn permission(&self, source: Source) -> Permission {
        self.permissions.get(source)
    }

    /// The sources the meeting records, or [`None`] when there is no meeting.
    #[must_use]
    pub const fn sources(&self) -> Option<SourceSet> {
        match self.phase {
            Phase::Idle => None,
            Phase::Preparing(preparing) => Some(preparing.sources),
            Phase::Recording(recording) => Some(recording.sources),
            Phase::Paused(paused) => Some(paused.sources),
            Phase::Closing(closing) => Some(closing.sources),
            Phase::Reviewing(reviewing) => Some(reviewing.sources),
            Phase::Confirmed(confirmed) => Some(confirmed.sources),
        }
    }

    /// Capture streams that are open now.
    #[must_use]
    pub const fn open_sources(&self) -> SourceSet {
        match self.phase {
            Phase::Preparing(preparing) => preparing.open,
            Phase::Recording(recording) => recording.open,
            _ => SourceSet::EMPTY,
        }
    }

    /// Capture streams whose device open is still outstanding.
    #[must_use]
    pub const fn pending_sources(&self) -> SourceSet {
        match self.phase {
            Phase::Preparing(preparing) => preparing.pending,
            _ => SourceSet::EMPTY,
        }
    }

    /// Transcription sessions that are finishing and await their last segment.
    #[must_use]
    pub const fn finishing_sources(&self) -> SourceSet {
        match self.phase {
            Phase::Recording(recording) => recording.finishing,
            Phase::Closing(closing) => closing.finishing,
            _ => SourceSet::EMPTY,
        }
    }

    /// Transcript progress, or [`None`] while no meeting has text yet.
    ///
    /// A resumed meeting reports the progress it paused with, including while
    /// its devices open again.
    #[must_use]
    pub const fn transcript(&self) -> Option<Transcript> {
        match self.phase {
            Phase::Idle => None,
            Phase::Preparing(preparing) => match preparing.origin {
                Origin::Fresh => None,
                Origin::Resume(progress) => Some(progress.transcript),
            },
            Phase::Recording(recording) => Some(recording.progress.transcript),
            Phase::Paused(paused) => Some(paused.progress.transcript),
            Phase::Closing(closing) => Some(closing.progress.transcript),
            Phase::Reviewing(reviewing) => Some(reviewing.progress.transcript),
            Phase::Confirmed(confirmed) => Some(confirmed.progress.transcript),
        }
    }

    /// Whether the meeting's audio was fully transcribed, or [`None`] while no
    /// meeting has text yet.
    #[must_use]
    pub const fn outcome(&self) -> Option<Outcome> {
        match self.phase {
            Phase::Idle => None,
            Phase::Preparing(preparing) => match preparing.origin {
                Origin::Fresh => None,
                Origin::Resume(progress) => Some(progress.outcome),
            },
            Phase::Recording(recording) => Some(recording.progress.outcome),
            Phase::Paused(paused) => Some(paused.progress.outcome),
            Phase::Closing(closing) => Some(closing.progress.outcome),
            Phase::Reviewing(reviewing) => Some(reviewing.progress.outcome),
            Phase::Confirmed(confirmed) => Some(confirmed.progress.outcome),
        }
    }

    /// Applies `input` and reports the next session with the work to run.
    ///
    /// # Errors
    ///
    /// Returns why the input does not apply. The session is unchanged.
    pub fn step(&self, input: Input) -> Result<Step, Rejected> {
        let mut session = *self;
        let mut effects = Vec::new();
        match input {
            Input::RequestPermission(source) => session.request_permission(source, &mut effects)?,
            Input::PermissionGranted(source) => session.grant(source, &mut effects)?,
            Input::PermissionDenied(source) => session.deny(source, &mut effects)?,
            Input::PermissionRevoked(source) => session.revoke(source, &mut effects)?,
            other => session.advance(other, &mut effects)?,
        }
        Ok(Step { session, effects })
    }

    fn request_permission(
        &mut self,
        source: Source,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Rejected> {
        let permission = self.permission(source);
        if !permission.can_request() {
            return Err(Rejected::PermissionOutOfOrder { source, permission });
        }
        self.permissions.set(source, Permission::Requested);
        effects.push(Effect::RequestPermission(source));
        Ok(())
    }

    fn grant(&mut self, source: Source, effects: &mut Vec<Effect>) -> Result<(), Rejected> {
        let permission = self.permission(source);
        if permission.is_granted() {
            return Err(Rejected::PermissionOutOfOrder { source, permission });
        }
        self.permissions.set(source, Permission::Granted);
        if let Phase::Preparing(preparing) = self.phase
            && self.permissions.all_granted(preparing.sources)
        {
            let mut opened = preparing;
            for candidate in preparing.sources.iter() {
                if !opened.pending.union(opened.open).contains(candidate) {
                    opened.pending = opened.pending.union(candidate.into());
                    effects.push(Effect::OpenSource(candidate));
                }
            }
            self.phase = Phase::Preparing(opened);
        }
        Ok(())
    }

    fn deny(&mut self, source: Source, effects: &mut Vec<Effect>) -> Result<(), Rejected> {
        let permission = self.permission(source);
        if permission != Permission::Requested {
            return Err(Rejected::PermissionOutOfOrder { source, permission });
        }
        self.permissions.set(source, Permission::Denied);
        if let Phase::Preparing(preparing) = self.phase
            && preparing.sources.contains(source)
        {
            self.abort_preparing(preparing, effects);
        }
        Ok(())
    }

    fn revoke(&mut self, source: Source, effects: &mut Vec<Effect>) -> Result<(), Rejected> {
        let permission = self.permission(source);
        if permission != Permission::Granted {
            return Err(Rejected::PermissionOutOfOrder { source, permission });
        }
        self.permissions.set(source, Permission::Denied);
        match self.phase {
            Phase::Preparing(preparing) if preparing.sources.contains(source) => {
                self.abort_preparing(preparing, effects);
            }
            Phase::Recording(recording) if recording.open.contains(source) => {
                self.drop_recording_source(recording, source, Fault::Permission(source), effects);
            }
            _ => {}
        }
        Ok(())
    }

    fn advance(&mut self, input: Input, effects: &mut Vec<Effect>) -> Result<(), Rejected> {
        match (self.phase, input) {
            (Phase::Idle, Input::Start(sources)) => self.start(sources, effects),
            (Phase::Preparing(preparing), Input::DeviceOpened(source)) => {
                self.device_opened(preparing, source, effects)
            }
            (
                Phase::Preparing(preparing),
                Input::DeviceFailed(source) | Input::CaptureFailed(source),
            ) => self.failed_preparing(preparing, source, PreparingFailure::Source, effects),
            (Phase::Preparing(preparing), Input::EngineFailed(source)) => {
                self.failed_preparing(preparing, source, PreparingFailure::Engine, effects)
            }
            (Phase::Preparing(preparing), Input::Stop) => {
                self.abort_preparing(preparing, effects);
                Ok(())
            }
            (Phase::Recording(recording), Input::Pause) => {
                self.close_recording(recording, Next::Pause, effects);
                Ok(())
            }
            (Phase::Recording(recording), Input::Stop) => {
                self.close_recording(recording, Next::Stop, effects);
                Ok(())
            }
            (Phase::Recording(recording), Input::CaptureFailed(source)) => {
                self.failed_recording_source(recording, source, Fault::Capture(source), effects)
            }
            (Phase::Recording(recording), Input::DeviceFailed(source)) => {
                self.failed_recording_source(recording, source, Fault::Device(source), effects)
            }
            (Phase::Recording(recording), Input::EngineFailed(source)) => {
                self.engine_failed(recording, source, effects)
            }
            (Phase::Recording(recording), Input::EngineCommitted(source)) => {
                self.engine_event_recording(recording, source, true)
            }
            (Phase::Recording(recording), Input::EngineVolatile(source)) => {
                self.engine_event_recording(recording, source, false)
            }
            (Phase::Recording(recording), Input::SegmentClosed { source, result }) => {
                self.segment_closed_recording(recording, source, result)
            }
            (Phase::Closing(closing), Input::EngineCommitted(source)) => {
                self.engine_event_closing(closing, source, true)
            }
            (Phase::Closing(closing), Input::EngineVolatile(source)) => {
                self.engine_event_closing(closing, source, false)
            }
            (Phase::Closing(closing), Input::EngineFailed(source)) => {
                self.engine_failed_closing(closing, source)
            }
            (Phase::Closing(closing), Input::SegmentClosed { source, result }) => {
                self.segment_closed_closing(closing, source, result)
            }
            (Phase::Closing(closing), Input::Stop) if closing.next == Next::Pause => {
                self.phase = Phase::Closing(Closing {
                    next: Next::Stop,
                    ..closing
                });
                Ok(())
            }
            (Phase::Paused(paused), Input::Resume) => self.resume(paused, effects),
            (Phase::Paused(paused), Input::Stop) => {
                self.phase = Phase::Reviewing(Reviewing {
                    sources: paused.sources,
                    progress: paused.progress,
                });
                Ok(())
            }
            (Phase::Reviewing(reviewing), Input::Confirm) => {
                self.phase = Phase::Confirmed(Confirmed {
                    sources: reviewing.sources,
                    progress: reviewing.progress,
                });
                effects.push(Effect::SaveTranscript);
                Ok(())
            }
            (Phase::Reviewing(_), Input::Reset) => {
                self.phase = Phase::Idle;
                effects.push(Effect::DiscardTranscript);
                Ok(())
            }
            (Phase::Confirmed(_), Input::Reset) => {
                self.phase = Phase::Idle;
                Ok(())
            }
            (phase, _) => Err(Rejected::WrongStage(phase.stage())),
        }
    }

    fn start(&mut self, sources: SourceSet, effects: &mut Vec<Effect>) -> Result<(), Rejected> {
        if sources.is_empty() {
            return Err(Rejected::NoSources);
        }
        for source in sources.iter() {
            match self.permission(source) {
                Permission::Granted | Permission::Requested => {}
                Permission::Unknown => {
                    self.permissions.set(source, Permission::Requested);
                    effects.push(Effect::RequestPermission(source));
                }
                Permission::Denied => return Err(Rejected::PermissionDenied(source)),
            }
        }
        let mut preparing = Preparing {
            sources,
            pending: SourceSet::EMPTY,
            open: SourceSet::EMPTY,
            origin: Origin::Fresh,
        };
        if self.permissions.all_granted(sources) {
            for source in sources.iter() {
                preparing.pending = preparing.pending.union(source.into());
                effects.push(Effect::OpenSource(source));
            }
        }
        self.phase = Phase::Preparing(preparing);
        Ok(())
    }

    fn device_opened(
        &mut self,
        preparing: Preparing,
        source: Source,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Rejected> {
        if !preparing.pending.contains(source) {
            return Err(Rejected::SourceNotPending(source));
        }
        if !self.permission(source).is_granted() {
            return self.device_failed(preparing, source, effects);
        }
        let opened = Preparing {
            pending: preparing.pending.without(source),
            open: preparing.open.union(source.into()),
            ..preparing
        };
        self.phase = self.settle(opened);
        Ok(())
    }

    /// Ends preparation once every chosen source is permitted and no open is pending.
    ///
    /// A meeting that asked for both sources waits for both answers. Starting on
    /// the first open would leave a later grant with no way to join the meeting.
    const fn settle(&self, preparing: Preparing) -> Phase {
        if preparing.pending.is_empty()
            && !preparing.open.is_empty()
            && self.permissions.all_granted(preparing.sources)
        {
            Phase::Recording(Recording {
                sources: preparing.sources,
                open: preparing.open,
                finishing: SourceSet::EMPTY,
                progress: preparing.origin.carried(),
            })
        } else if preparing.pending.is_empty() && preparing.open.is_empty() {
            leave(preparing)
        } else {
            Phase::Preparing(preparing)
        }
    }

    fn device_failed(
        &mut self,
        preparing: Preparing,
        source: Source,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Rejected> {
        if !preparing.pending.contains(source) {
            return Err(Rejected::SourceNotPending(source));
        }
        effects.push(Effect::CloseSource(source));
        effects.push(Effect::DiscardSource(source));
        let failed = Preparing {
            pending: preparing.pending.without(source),
            ..preparing
        };
        self.phase = self.settle(failed);
        Ok(())
    }

    fn failed_preparing(
        &mut self,
        preparing: Preparing,
        source: Source,
        failure: PreparingFailure,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Rejected> {
        let active = preparing.open.union(preparing.pending).contains(source);
        match failure {
            PreparingFailure::Source if preparing.open.contains(source) => {
                self.abort_preparing(preparing, effects);
                Ok(())
            }
            PreparingFailure::Source => self.device_failed(preparing, source, effects),
            PreparingFailure::Engine if active => {
                self.abort_preparing(preparing, effects);
                Ok(())
            }
            PreparingFailure::Engine => Err(Rejected::SourceNotActive(source)),
        }
    }

    fn abort_preparing(&mut self, preparing: Preparing, effects: &mut Vec<Effect>) {
        let covered = preparing.open.union(preparing.pending);
        for source in preparing.sources.iter() {
            if covered.contains(source) {
                effects.push(Effect::CloseSource(source));
                effects.push(Effect::DiscardSource(source));
            }
        }
        self.phase = leave(preparing);
    }

    fn close_recording(&mut self, recording: Recording, next: Next, effects: &mut Vec<Effect>) {
        for source in recording.open.iter() {
            effects.push(Effect::CloseSource(source));
            effects.push(Effect::FinishSource(source));
        }
        self.phase = Phase::Closing(Closing {
            sources: recording.sources,
            finishing: recording.finishing.union(recording.open),
            progress: recording.progress,
            next,
        });
    }

    fn failed_recording_source(
        &mut self,
        recording: Recording,
        source: Source,
        fault: Fault,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Rejected> {
        if !recording.open.contains(source) {
            return Err(Rejected::SourceNotActive(source));
        }
        self.drop_recording_source(recording, source, fault, effects);
        Ok(())
    }

    fn drop_recording_source(
        &mut self,
        recording: Recording,
        source: Source,
        fault: Fault,
        effects: &mut Vec<Effect>,
    ) {
        effects.push(Effect::CloseSource(source));
        effects.push(Effect::FinishSource(source));
        let dropped = Recording {
            open: recording.open.without(source),
            finishing: recording.finishing.union(source.into()),
            progress: Progress {
                outcome: recording.progress.outcome.or_incomplete(fault),
                ..recording.progress
            },
            ..recording
        };
        self.phase = if dropped.open.is_empty() {
            Phase::Closing(Closing {
                sources: dropped.sources,
                finishing: dropped.finishing,
                progress: dropped.progress,
                next: Next::Stop,
            })
        } else {
            Phase::Recording(dropped)
        };
    }

    fn engine_failed(
        &mut self,
        recording: Recording,
        source: Source,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Rejected> {
        if recording.open.contains(source) {
            self.drop_recording_source(recording, source, Fault::Engine(source), effects);
            Ok(())
        } else if recording.finishing.contains(source) {
            self.phase = Phase::Recording(Recording {
                progress: faulty(recording.progress, Fault::Engine(source)),
                ..recording
            });
            Ok(())
        } else {
            Err(Rejected::SourceNotActive(source))
        }
    }

    const fn engine_failed_closing(
        &mut self,
        closing: Closing,
        source: Source,
    ) -> Result<(), Rejected> {
        if !closing.finishing.contains(source) {
            return Err(Rejected::SourceNotActive(source));
        }
        self.phase = Phase::Closing(Closing {
            progress: faulty(closing.progress, Fault::Engine(source)),
            ..closing
        });
        Ok(())
    }

    const fn engine_event_recording(
        &mut self,
        recording: Recording,
        source: Source,
        committed: bool,
    ) -> Result<(), Rejected> {
        if !recording.open.union(recording.finishing).contains(source) {
            return Err(Rejected::SourceNotActive(source));
        }
        self.phase = Phase::Recording(Recording {
            progress: record(recording.progress, source, committed),
            ..recording
        });
        Ok(())
    }

    const fn engine_event_closing(
        &mut self,
        closing: Closing,
        source: Source,
        committed: bool,
    ) -> Result<(), Rejected> {
        if !closing.finishing.contains(source) {
            return Err(Rejected::SourceNotActive(source));
        }
        self.phase = Phase::Closing(Closing {
            progress: record(closing.progress, source, committed),
            ..closing
        });
        Ok(())
    }

    const fn segment_closed_recording(
        &mut self,
        recording: Recording,
        source: Source,
        result: SegmentResult,
    ) -> Result<(), Rejected> {
        if !recording.finishing.contains(source) {
            return Err(Rejected::SourceNotFinishing(source));
        }
        self.phase = Phase::Recording(Recording {
            finishing: recording.finishing.without(source),
            progress: merge(recording.progress, source, result),
            ..recording
        });
        Ok(())
    }

    const fn segment_closed_closing(
        &mut self,
        closing: Closing,
        source: Source,
        result: SegmentResult,
    ) -> Result<(), Rejected> {
        if !closing.finishing.contains(source) {
            return Err(Rejected::SourceNotFinishing(source));
        }
        let progress = merge(closing.progress, source, result);
        let finishing = closing.finishing.without(source);
        self.phase = if finishing.is_empty() {
            match closing.next {
                Next::Pause => Phase::Paused(Paused {
                    sources: closing.sources,
                    progress,
                }),
                Next::Stop => Phase::Reviewing(Reviewing {
                    sources: closing.sources,
                    progress,
                }),
            }
        } else {
            Phase::Closing(Closing {
                finishing,
                progress,
                ..closing
            })
        };
        Ok(())
    }

    fn resume(&mut self, paused: Paused, effects: &mut Vec<Effect>) -> Result<(), Rejected> {
        if let Some(source) = paused
            .sources
            .iter()
            .find(|source| !self.permission(*source).is_granted())
        {
            return Err(Rejected::PermissionDenied(source));
        }
        for source in paused.sources.iter() {
            effects.push(Effect::OpenSource(source));
        }
        self.phase = Phase::Preparing(Preparing {
            sources: paused.sources,
            pending: paused.sources,
            open: SourceSet::EMPTY,
            origin: Origin::Resume(paused.progress),
        });
        Ok(())
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new(Permissions::default())
    }
}

const fn leave(preparing: Preparing) -> Phase {
    match preparing.origin {
        Origin::Fresh => Phase::Idle,
        Origin::Resume(progress) => Phase::Paused(Paused {
            sources: preparing.sources,
            progress,
        }),
    }
}

const fn faulty(progress: Progress, fault: Fault) -> Progress {
    Progress {
        outcome: progress.outcome.or_incomplete(fault),
        ..progress
    }
}

const fn record(mut progress: Progress, source: Source, committed: bool) -> Progress {
    if committed {
        progress.transcript.set_committed(source, true);
        progress.transcript.set_volatile(source, false);
    } else {
        progress.transcript.set_volatile(source, true);
    }
    progress
}

const fn merge(mut progress: Progress, source: Source, result: SegmentResult) -> Progress {
    progress.transcript.set_committed(
        source,
        progress.transcript.committed(source) || result.committed,
    );
    progress.transcript.set_volatile(source, false);
    match result.outcome {
        SegmentOutcome::Complete => {}
        SegmentOutcome::Incomplete => {
            progress = faulty(progress, Fault::Engine(source));
        }
    }
    progress
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn granted() -> Session {
        Session::new(Permissions::new(Permission::Granted, Permission::Granted))
    }

    fn drive(session: Session, inputs: &[Input]) -> Session {
        inputs.iter().fold(session, |session, input| {
            session.step(*input).unwrap().session
        })
    }

    fn recording_one() -> Session {
        drive(
            granted(),
            &[
                Input::Start(SourceSet::MICROPHONE),
                Input::DeviceOpened(Source::Microphone),
            ],
        )
    }

    fn recording_two() -> Session {
        drive(
            granted(),
            &[
                Input::Start(SourceSet::MEETING),
                Input::DeviceOpened(Source::Microphone),
                Input::DeviceOpened(Source::SystemAudio),
            ],
        )
    }

    fn closed(closing: Session) -> Session {
        drive(closing, &[Input::Stop])
    }

    fn segment(outcome: SegmentOutcome, committed: bool) -> Input {
        Input::SegmentClosed {
            source: Source::Microphone,
            result: SegmentResult { outcome, committed },
        }
    }

    fn segment_for(source: Source, outcome: SegmentOutcome, committed: bool) -> Input {
        Input::SegmentClosed {
            source,
            result: SegmentResult { outcome, committed },
        }
    }

    fn reviewing() -> Session {
        drive(
            closed(recording_one()),
            &[segment(SegmentOutcome::Complete, true)],
        )
    }

    fn confirmed() -> Session {
        drive(reviewing(), &[Input::Confirm])
    }

    fn paused() -> Session {
        drive(
            recording_one(),
            &[Input::Pause, segment(SegmentOutcome::Complete, true)],
        )
    }

    fn paused_two() -> Session {
        drive(
            recording_two(),
            &[
                Input::Pause,
                segment(SegmentOutcome::Complete, true),
                segment_for(Source::SystemAudio, SegmentOutcome::Complete, false),
            ],
        )
    }

    fn preparing_pending_two() -> Session {
        drive(granted(), &[Input::Start(SourceSet::MEETING)])
    }

    fn preparing_open_one_pending_one() -> Session {
        drive(
            preparing_pending_two(),
            &[Input::DeviceOpened(Source::Microphone)],
        )
    }

    fn preparing_resume() -> Session {
        drive(paused(), &[Input::Resume])
    }

    #[test]
    fn default_session_is_idle_with_unknown_permissions() {
        let session = Session::default();
        assert_eq!(session.stage(), Stage::Idle);
        assert_eq!(session.permissions(), Permissions::default());
        assert_eq!(session.permission(Source::Microphone), Permission::Unknown);
        assert_eq!(session.permission(Source::SystemAudio), Permission::Unknown);
        assert_eq!(session.sources(), None);
        assert_eq!(session.open_sources(), SourceSet::EMPTY);
        assert_eq!(session.pending_sources(), SourceSet::EMPTY);
        assert_eq!(session.finishing_sources(), SourceSet::EMPTY);
        assert_eq!(session.transcript(), None);
        assert_eq!(session.outcome(), None);
    }

    #[test]
    fn a_rejection_leaves_the_session_untouched() {
        let session = recording_one();
        assert_eq!(
            session.step(Input::Confirm),
            Err(Rejected::WrongStage(Stage::Recording))
        );
        assert_eq!(session, recording_one());
    }

    #[test]
    fn rejections_are_displayed() {
        assert_eq!(
            Rejected::WrongStage(Stage::Paused).to_string(),
            "input is not allowed while paused"
        );
        assert_eq!(
            Rejected::PermissionDenied(Source::Microphone).to_string(),
            "microphone permission is denied"
        );
        assert_eq!(
            Rejected::PermissionOutOfOrder {
                source: Source::SystemAudio,
                permission: Permission::Requested,
            }
            .to_string(),
            "system audio permission is requested"
        );
        assert_eq!(
            Rejected::SourceNotPending(Source::Microphone).to_string(),
            "microphone has no device opening"
        );
        assert_eq!(
            Rejected::SourceNotActive(Source::SystemAudio).to_string(),
            "system audio is not capturing"
        );
        assert_eq!(
            Rejected::SourceNotFinishing(Source::Microphone).to_string(),
            "microphone has no transcription session finishing"
        );
        let rejected: &dyn core::error::Error = &Rejected::WrongStage(Stage::Idle);
        assert_eq!(rejected.to_string(), "input is not allowed while idle");
    }

    #[test]
    fn every_stage_is_named() {
        assert_eq!(Stage::Idle.as_str(), "idle");
        assert_eq!(Stage::Preparing.as_str(), "preparing");
        assert_eq!(Stage::Recording.as_str(), "recording");
        assert_eq!(Stage::Paused.as_str(), "paused");
        assert_eq!(Stage::Closing.as_str(), "closing");
        assert_eq!(Stage::Reviewing.as_str(), "reviewing");
        assert_eq!(Stage::Confirmed.as_str(), "confirmed");
    }

    #[test]
    fn permission_walks_unknown_requested_granted_and_denied() {
        let requested = Session::default()
            .step(Input::RequestPermission(Source::Microphone))
            .unwrap();
        assert_eq!(
            requested.effects,
            vec![Effect::RequestPermission(Source::Microphone)]
        );
        assert_eq!(
            requested.session.permission(Source::Microphone),
            Permission::Requested
        );
        assert_eq!(requested.session.stage(), Stage::Idle);

        let denied = requested
            .session
            .step(Input::PermissionDenied(Source::Microphone))
            .unwrap();
        assert_eq!(denied.effects, vec![]);
        assert_eq!(
            denied.session.permission(Source::Microphone),
            Permission::Denied
        );
        assert_eq!(denied.session.stage(), Stage::Idle);

        let asked_again = denied
            .session
            .step(Input::RequestPermission(Source::Microphone))
            .unwrap();
        assert_eq!(
            asked_again.effects,
            vec![Effect::RequestPermission(Source::Microphone)]
        );
        assert_eq!(
            asked_again.session.permission(Source::Microphone),
            Permission::Requested
        );

        let granted = asked_again
            .session
            .step(Input::PermissionGranted(Source::Microphone))
            .unwrap();
        assert_eq!(granted.effects, vec![]);
        assert_eq!(
            granted.session.permission(Source::Microphone),
            Permission::Granted
        );
        assert_eq!(granted.session.stage(), Stage::Idle);
    }

    #[test]
    fn a_request_in_flight_and_a_granted_permission_cannot_be_requested() {
        let requested = drive(
            Session::default(),
            &[Input::RequestPermission(Source::Microphone)],
        );
        assert_eq!(
            requested.step(Input::RequestPermission(Source::Microphone)),
            Err(Rejected::PermissionOutOfOrder {
                source: Source::Microphone,
                permission: Permission::Requested,
            })
        );
        assert_eq!(
            granted().step(Input::RequestPermission(Source::Microphone)),
            Err(Rejected::PermissionOutOfOrder {
                source: Source::Microphone,
                permission: Permission::Granted,
            })
        );
    }

    #[test]
    fn a_granted_permission_cannot_be_granted_again() {
        assert_eq!(
            granted().step(Input::PermissionGranted(Source::Microphone)),
            Err(Rejected::PermissionOutOfOrder {
                source: Source::Microphone,
                permission: Permission::Granted,
            })
        );
    }

    #[test]
    fn a_denied_permission_cannot_be_denied_or_revoked() {
        let denied = drive(
            Session::default(),
            &[
                Input::RequestPermission(Source::Microphone),
                Input::PermissionDenied(Source::Microphone),
            ],
        );
        for input in [
            Input::PermissionDenied(Source::Microphone),
            Input::PermissionRevoked(Source::Microphone),
        ] {
            assert_eq!(
                denied.step(input),
                Err(Rejected::PermissionOutOfOrder {
                    source: Source::Microphone,
                    permission: Permission::Denied,
                })
            );
        }
    }

    #[test]
    fn a_grant_while_preparing_opens_the_source() {
        let step = drive(Session::default(), &[Input::Start(SourceSet::MICROPHONE)])
            .step(Input::PermissionGranted(Source::Microphone))
            .unwrap();
        assert_eq!(step.effects, vec![Effect::OpenSource(Source::Microphone)]);
        assert_eq!(step.session.stage(), Stage::Preparing);
        assert_eq!(step.session.pending_sources(), SourceSet::MICROPHONE);
        assert_eq!(
            step.session.permission(Source::Microphone),
            Permission::Granted
        );
    }

    #[test]
    fn a_grant_for_a_source_outside_the_meeting_changes_only_the_permission() {
        let started = Session::new(Permissions::new(Permission::Requested, Permission::Unknown))
            .step(Input::Start(SourceSet::MICROPHONE))
            .unwrap()
            .session;
        let step = started
            .step(Input::PermissionGranted(Source::SystemAudio))
            .unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.pending_sources(), SourceSet::EMPTY);
        assert_eq!(step.session.stage(), Stage::Preparing);
        assert_eq!(
            step.session.permission(Source::SystemAudio),
            Permission::Granted
        );
    }

    #[test]
    fn a_grant_for_a_source_already_opening_does_not_open_it_twice() {
        let session = Session {
            permissions: Permissions::new(Permission::Requested, Permission::Unknown),
            phase: Phase::Preparing(Preparing {
                sources: SourceSet::MICROPHONE,
                pending: SourceSet::MICROPHONE,
                open: SourceSet::EMPTY,
                origin: Origin::Fresh,
            }),
        };
        let step = session
            .step(Input::PermissionGranted(Source::Microphone))
            .unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.pending_sources(), SourceSet::MICROPHONE);
        assert_eq!(
            step.session.permission(Source::Microphone),
            Permission::Granted
        );
    }

    #[test]
    fn a_grant_during_recording_does_not_open_anything() {
        let session = drive(
            Session::new(Permissions::new(Permission::Granted, Permission::Unknown)),
            &[
                Input::Start(SourceSet::MICROPHONE),
                Input::DeviceOpened(Source::Microphone),
            ],
        );
        let step = session
            .step(Input::PermissionGranted(Source::SystemAudio))
            .unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.stage(), Stage::Recording);
        assert_eq!(step.session.open_sources(), SourceSet::MICROPHONE);
        assert_eq!(
            step.session.permission(Source::SystemAudio),
            Permission::Granted
        );
    }

    #[test]
    fn a_denial_aborts_preparing() {
        let session = drive(
            Session::default(),
            &[
                Input::Start(SourceSet::MEETING),
                Input::PermissionDenied(Source::Microphone),
            ],
        );
        assert_eq!(session.stage(), Stage::Idle);
        assert_eq!(session.sources(), None);
        assert_eq!(session.permission(Source::Microphone), Permission::Denied);

        let step = drive(
            Session::default(),
            &[
                Input::Start(SourceSet::MEETING),
                Input::PermissionDenied(Source::SystemAudio),
            ],
        );
        assert_eq!(step.stage(), Stage::Idle);
    }

    #[test]
    fn a_denial_outside_preparing_only_marks_the_permission() {
        let session = drive(
            Session::default(),
            &[Input::RequestPermission(Source::Microphone)],
        );
        let step = session
            .step(Input::PermissionDenied(Source::Microphone))
            .unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.stage(), Stage::Idle);
        assert_eq!(
            step.session.permission(Source::Microphone),
            Permission::Denied
        );
    }

    #[test]
    fn a_revocation_while_preparing_aborts_and_discards_what_was_opening() {
        let step = preparing_open_one_pending_one()
            .step(Input::PermissionRevoked(Source::Microphone))
            .unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::Microphone),
                Effect::DiscardSource(Source::Microphone),
                Effect::CloseSource(Source::SystemAudio),
                Effect::DiscardSource(Source::SystemAudio),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Idle);
        assert_eq!(
            step.session.permission(Source::Microphone),
            Permission::Denied
        );
    }

    #[test]
    fn a_revocation_while_resuming_returns_to_paused() {
        let step = preparing_resume()
            .step(Input::PermissionRevoked(Source::Microphone))
            .unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::Microphone),
                Effect::DiscardSource(Source::Microphone),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Paused);
        assert_eq!(
            step.session
                .transcript()
                .map(|t| t.committed(Source::Microphone)),
            Some(true)
        );
    }

    #[test]
    fn a_revocation_while_recording_drops_the_source() {
        let step = recording_two()
            .step(Input::PermissionRevoked(Source::Microphone))
            .unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::Microphone),
                Effect::FinishSource(Source::Microphone),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Recording);
        assert_eq!(step.session.open_sources(), SourceSet::SYSTEM_AUDIO);
        assert_eq!(step.session.finishing_sources(), SourceSet::MICROPHONE);
        assert_eq!(
            step.session.outcome(),
            Some(Outcome::Incomplete(Fault::Permission(Source::Microphone)))
        );

        let last = recording_one()
            .step(Input::PermissionRevoked(Source::Microphone))
            .unwrap();
        assert_eq!(last.session.stage(), Stage::Closing);
        assert_eq!(last.session.finishing_sources(), SourceSet::MICROPHONE);
    }

    #[test]
    fn a_revocation_of_a_finishing_source_keeps_the_stage() {
        let step = closed(recording_two())
            .step(Input::PermissionRevoked(Source::Microphone))
            .unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.stage(), Stage::Closing);
        assert_eq!(step.session.finishing_sources(), SourceSet::MEETING);
        assert_eq!(
            step.session.permission(Source::Microphone),
            Permission::Denied
        );
    }

    #[test]
    fn a_revocation_of_a_source_that_is_only_finishing_keeps_recording() {
        let session = drive(recording_two(), &[Input::CaptureFailed(Source::Microphone)]);
        let step = session
            .step(Input::PermissionRevoked(Source::Microphone))
            .unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.stage(), Stage::Recording);
        assert_eq!(step.session.finishing_sources(), SourceSet::MICROPHONE);
        assert_eq!(
            step.session.outcome(),
            Some(Outcome::Incomplete(Fault::Capture(Source::Microphone)))
        );
    }

    #[test]
    fn a_revocation_for_a_source_outside_the_meeting_keeps_preparing() {
        let session = drive(granted(), &[Input::Start(SourceSet::MICROPHONE)]);
        let step = session
            .step(Input::PermissionRevoked(Source::SystemAudio))
            .unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.stage(), Stage::Preparing);
        assert_eq!(step.session.pending_sources(), SourceSet::MICROPHONE);
        assert_eq!(
            step.session.permission(Source::SystemAudio),
            Permission::Denied
        );
    }

    #[test]
    fn a_revocation_while_paused_keeps_the_meeting_until_resume() {
        let step = paused()
            .step(Input::PermissionRevoked(Source::Microphone))
            .unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.stage(), Stage::Paused);
        assert_eq!(
            step.session.permission(Source::Microphone),
            Permission::Denied
        );
        assert_eq!(
            step.session.step(Input::Resume),
            Err(Rejected::PermissionDenied(Source::Microphone))
        );

        let regranted = step
            .session
            .step(Input::PermissionGranted(Source::Microphone))
            .unwrap()
            .session;
        assert_eq!(
            regranted.step(Input::Resume).unwrap().session.stage(),
            Stage::Preparing
        );
    }

    #[test]
    fn start_opens_granted_sources_in_order() {
        let step = granted().step(Input::Start(SourceSet::MEETING)).unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::OpenSource(Source::Microphone),
                Effect::OpenSource(Source::SystemAudio),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Preparing);
        assert_eq!(step.session.sources(), Some(SourceSet::MEETING));
        assert_eq!(step.session.pending_sources(), SourceSet::MEETING);
        assert_eq!(step.session.open_sources(), SourceSet::EMPTY);
        assert_eq!(step.session.transcript(), None);
        assert_eq!(step.session.outcome(), None);
    }

    #[test]
    fn start_requests_the_unknown_sources_in_order() {
        let step = Session::default()
            .step(Input::Start(SourceSet::MEETING))
            .unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::RequestPermission(Source::Microphone),
                Effect::RequestPermission(Source::SystemAudio),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Preparing);
        assert_eq!(step.session.pending_sources(), SourceSet::EMPTY);
    }

    #[test]
    fn start_does_not_request_a_source_already_asked() {
        let session = Session::new(Permissions::new(
            Permission::Requested,
            Permission::Requested,
        ));
        let step = session.step(Input::Start(SourceSet::MICROPHONE)).unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.stage(), Stage::Preparing);
    }

    #[test]
    fn start_rejects_a_denied_source_in_order() {
        let microphone_denied =
            Session::new(Permissions::new(Permission::Denied, Permission::Granted));
        assert_eq!(
            microphone_denied.step(Input::Start(SourceSet::MEETING)),
            Err(Rejected::PermissionDenied(Source::Microphone))
        );
        assert_eq!(microphone_denied.stage(), Stage::Idle);
        assert_eq!(microphone_denied.sources(), None);

        let system_denied = Session::new(Permissions::new(Permission::Granted, Permission::Denied));
        assert_eq!(
            system_denied.step(Input::Start(SourceSet::MEETING)),
            Err(Rejected::PermissionDenied(Source::SystemAudio))
        );
    }

    #[test]
    fn start_rejects_an_empty_source_set() {
        let empty = SourceSet::MEETING
            .without(Source::Microphone)
            .without(Source::SystemAudio);
        let session = granted();
        assert_eq!(session.step(Input::Start(empty)), Err(Rejected::NoSources));
        assert_eq!(session.stage(), Stage::Idle);
    }

    #[test]
    fn start_outside_idle_is_rejected() {
        assert_eq!(
            recording_one().step(Input::Start(SourceSet::MEETING)),
            Err(Rejected::WrongStage(Stage::Recording))
        );
        assert_eq!(
            paused().step(Input::Start(SourceSet::MEETING)),
            Err(Rejected::WrongStage(Stage::Paused))
        );
        assert_eq!(
            reviewing().step(Input::Start(SourceSet::MEETING)),
            Err(Rejected::WrongStage(Stage::Reviewing))
        );
        assert_eq!(
            confirmed().step(Input::Start(SourceSet::MEETING)),
            Err(Rejected::WrongStage(Stage::Confirmed))
        );
    }

    #[test]
    fn the_last_device_opened_starts_recording() {
        let step = preparing_pending_two()
            .step(Input::DeviceOpened(Source::Microphone))
            .unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.stage(), Stage::Preparing);
        assert_eq!(step.session.sources(), Some(SourceSet::MEETING));
        assert_eq!(step.session.open_sources(), SourceSet::MICROPHONE);
        assert_eq!(step.session.pending_sources(), SourceSet::SYSTEM_AUDIO);

        let step = step
            .session
            .step(Input::DeviceOpened(Source::SystemAudio))
            .unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.stage(), Stage::Recording);
        assert_eq!(step.session.open_sources(), SourceSet::MEETING);
        assert_eq!(step.session.pending_sources(), SourceSet::EMPTY);
        assert_eq!(step.session.finishing_sources(), SourceSet::EMPTY);
        assert_eq!(step.session.transcript(), Some(Transcript::default()));
        assert_eq!(step.session.outcome(), Some(Outcome::Complete));
    }

    #[test]
    fn a_two_source_meeting_waits_for_the_second_permission() {
        let step = Session::new(Permissions::new(Permission::Granted, Permission::Unknown))
            .step(Input::Start(SourceSet::MEETING))
            .unwrap();
        assert_eq!(
            step.effects,
            vec![Effect::RequestPermission(Source::SystemAudio)]
        );
        assert_eq!(step.session.stage(), Stage::Preparing);
        assert_eq!(step.session.open_sources(), SourceSet::EMPTY);
        assert_eq!(step.session.pending_sources(), SourceSet::EMPTY);

        let step = step
            .session
            .step(Input::PermissionGranted(Source::SystemAudio))
            .unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::OpenSource(Source::Microphone),
                Effect::OpenSource(Source::SystemAudio),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Preparing);
        assert_eq!(step.session.pending_sources(), SourceSet::MEETING);

        let step = step
            .session
            .step(Input::DeviceOpened(Source::Microphone))
            .unwrap();
        assert_eq!(step.session.stage(), Stage::Preparing);
        assert_eq!(step.session.open_sources(), SourceSet::MICROPHONE);
        assert_eq!(step.session.pending_sources(), SourceSet::SYSTEM_AUDIO);

        let step = step
            .session
            .step(Input::DeviceOpened(Source::SystemAudio))
            .unwrap();
        assert_eq!(step.session.stage(), Stage::Recording);
        assert_eq!(step.session.open_sources(), SourceSet::MEETING);
    }

    #[test]
    fn a_device_open_without_a_pending_source_is_rejected() {
        assert_eq!(
            preparing_pending_two().step(Input::DeviceOpened(Source::Microphone)),
            Ok(Step {
                session: preparing_open_one_pending_one(),
                effects: vec![],
            })
        );
        assert_eq!(
            preparing_open_one_pending_one().step(Input::DeviceOpened(Source::Microphone)),
            Err(Rejected::SourceNotPending(Source::Microphone))
        );
        assert_eq!(
            granted().step(Input::DeviceOpened(Source::Microphone)),
            Err(Rejected::WrongStage(Stage::Idle))
        );
    }

    #[test]
    fn a_device_open_without_permission_fails_the_device() {
        let session = Session {
            permissions: Permissions::new(Permission::Unknown, Permission::Unknown),
            phase: Phase::Preparing(Preparing {
                sources: SourceSet::MICROPHONE,
                pending: SourceSet::MICROPHONE,
                open: SourceSet::EMPTY,
                origin: Origin::Fresh,
            }),
        };
        let step = session
            .step(Input::DeviceOpened(Source::Microphone))
            .unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::Microphone),
                Effect::DiscardSource(Source::Microphone),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Idle);
    }

    #[test]
    fn a_device_failure_discards_the_source() {
        let step = preparing_open_one_pending_one()
            .step(Input::DeviceFailed(Source::SystemAudio))
            .unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::SystemAudio),
                Effect::DiscardSource(Source::SystemAudio),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Recording);
        assert_eq!(step.session.open_sources(), SourceSet::MICROPHONE);

        let step = preparing_pending_two()
            .step(Input::DeviceFailed(Source::Microphone))
            .unwrap();
        assert_eq!(step.session.stage(), Stage::Preparing);
        assert_eq!(step.session.pending_sources(), SourceSet::SYSTEM_AUDIO);

        let step = drive(granted(), &[Input::Start(SourceSet::MICROPHONE)])
            .step(Input::DeviceFailed(Source::Microphone))
            .unwrap();
        assert_eq!(step.session.stage(), Stage::Idle);
        assert_eq!(step.session.sources(), None);
    }

    #[test]
    fn a_device_failure_of_a_resumed_source_returns_to_paused() {
        let step = preparing_resume()
            .step(Input::DeviceFailed(Source::Microphone))
            .unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::Microphone),
                Effect::DiscardSource(Source::Microphone),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Paused);
    }

    #[test]
    fn a_device_failure_without_a_pending_source_is_rejected() {
        assert_eq!(
            preparing_pending_two().step(Input::DeviceFailed(Source::SystemAudio)),
            Ok(Step {
                session: drive(
                    preparing_pending_two(),
                    &[Input::DeviceFailed(Source::SystemAudio)]
                ),
                effects: vec![
                    Effect::CloseSource(Source::SystemAudio),
                    Effect::DiscardSource(Source::SystemAudio),
                ],
            })
        );
        let outside = Session {
            permissions: Permissions::new(Permission::Granted, Permission::Granted),
            phase: Phase::Preparing(Preparing {
                sources: SourceSet::MICROPHONE,
                pending: SourceSet::MICROPHONE,
                open: SourceSet::EMPTY,
                origin: Origin::Fresh,
            }),
        };
        assert_eq!(
            outside.step(Input::DeviceFailed(Source::SystemAudio)),
            Err(Rejected::SourceNotPending(Source::SystemAudio))
        );
    }

    #[test]
    fn a_device_failure_after_open_during_preparation_aborts() {
        let step = preparing_open_one_pending_one()
            .step(Input::DeviceFailed(Source::Microphone))
            .unwrap();
        assert_eq!(step.session.stage(), Stage::Idle);
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::Microphone),
                Effect::DiscardSource(Source::Microphone),
                Effect::CloseSource(Source::SystemAudio),
                Effect::DiscardSource(Source::SystemAudio),
            ]
        );
    }

    #[test]
    fn a_capture_failure_after_open_during_preparation_aborts() {
        let step = preparing_open_one_pending_one()
            .step(Input::CaptureFailed(Source::Microphone))
            .unwrap();
        assert_eq!(step.session.stage(), Stage::Idle);
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::Microphone),
                Effect::DiscardSource(Source::Microphone),
                Effect::CloseSource(Source::SystemAudio),
                Effect::DiscardSource(Source::SystemAudio),
            ]
        );
    }

    #[test]
    fn an_engine_failure_during_preparation_aborts() {
        let step = preparing_open_one_pending_one()
            .step(Input::EngineFailed(Source::Microphone))
            .unwrap();
        assert_eq!(step.session.stage(), Stage::Idle);
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::Microphone),
                Effect::DiscardSource(Source::Microphone),
                Effect::CloseSource(Source::SystemAudio),
                Effect::DiscardSource(Source::SystemAudio),
            ]
        );
    }

    #[test]
    fn an_engine_failure_outside_preparation_is_rejected() {
        let outside = Session {
            permissions: Permissions::new(Permission::Granted, Permission::Granted),
            phase: Phase::Preparing(Preparing {
                sources: SourceSet::MICROPHONE,
                pending: SourceSet::MICROPHONE,
                open: SourceSet::EMPTY,
                origin: Origin::Fresh,
            }),
        };
        assert_eq!(
            outside.step(Input::EngineFailed(Source::SystemAudio)),
            Err(Rejected::SourceNotActive(Source::SystemAudio))
        );
    }

    #[test]
    fn a_capture_failure_while_preparing_acts_as_a_device_failure() {
        let step = preparing_pending_two()
            .step(Input::CaptureFailed(Source::Microphone))
            .unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::Microphone),
                Effect::DiscardSource(Source::Microphone),
            ]
        );
        assert_eq!(step.session.pending_sources(), SourceSet::SYSTEM_AUDIO);
    }

    #[test]
    fn stopping_while_preparing_closes_and_discards_what_was_opening() {
        let step = preparing_open_one_pending_one().step(Input::Stop).unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::Microphone),
                Effect::DiscardSource(Source::Microphone),
                Effect::CloseSource(Source::SystemAudio),
                Effect::DiscardSource(Source::SystemAudio),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Idle);
        assert_eq!(step.session.sources(), None);
    }

    #[test]
    fn stopping_while_preparing_leaves_untouched_sources_alone() {
        let step = drive(granted(), &[Input::Start(SourceSet::MICROPHONE)])
            .step(Input::Stop)
            .unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::Microphone),
                Effect::DiscardSource(Source::Microphone),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Idle);
    }

    #[test]
    fn stopping_while_resuming_returns_to_paused() {
        let step = preparing_resume().step(Input::Stop).unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::Microphone),
                Effect::DiscardSource(Source::Microphone),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Paused);
        assert_eq!(step.session.sources(), Some(SourceSet::MICROPHONE));
    }

    #[test]
    fn preparing_rejects_everything_else() {
        let preparing = preparing_pending_two();
        for input in [
            Input::Pause,
            Input::Resume,
            Input::Confirm,
            Input::Reset,
            Input::EngineCommitted(Source::Microphone),
            segment(SegmentOutcome::Complete, false),
        ] {
            assert_eq!(
                preparing.step(input),
                Err(Rejected::WrongStage(Stage::Preparing))
            );
        }
    }

    #[test]
    fn pausing_closes_and_finishes_every_open_source() {
        let step = recording_two().step(Input::Pause).unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::Microphone),
                Effect::FinishSource(Source::Microphone),
                Effect::CloseSource(Source::SystemAudio),
                Effect::FinishSource(Source::SystemAudio),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Closing);
        assert_eq!(step.session.finishing_sources(), SourceSet::MEETING);
        assert_eq!(step.session.open_sources(), SourceSet::EMPTY);
    }

    #[test]
    fn a_pause_keeps_a_source_that_was_already_finishing() {
        let session = drive(recording_two(), &[Input::CaptureFailed(Source::Microphone)]);
        let step = session.step(Input::Pause).unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::SystemAudio),
                Effect::FinishSource(Source::SystemAudio),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Closing);
        assert_eq!(step.session.finishing_sources(), SourceSet::MEETING);
    }

    #[test]
    fn stopping_closes_and_finishes_every_open_source() {
        let step = recording_two().step(Input::Stop).unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::Microphone),
                Effect::FinishSource(Source::Microphone),
                Effect::CloseSource(Source::SystemAudio),
                Effect::FinishSource(Source::SystemAudio),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Closing);
        assert_eq!(step.session.finishing_sources(), SourceSet::MEETING);
    }

    #[test]
    fn a_capture_failure_drops_only_its_source() {
        let step = recording_two()
            .step(Input::CaptureFailed(Source::Microphone))
            .unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::Microphone),
                Effect::FinishSource(Source::Microphone),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Recording);
        assert_eq!(step.session.open_sources(), SourceSet::SYSTEM_AUDIO);
        assert_eq!(step.session.finishing_sources(), SourceSet::MICROPHONE);
        assert_eq!(
            step.session.outcome(),
            Some(Outcome::Incomplete(Fault::Capture(Source::Microphone)))
        );

        let step = recording_one()
            .step(Input::CaptureFailed(Source::Microphone))
            .unwrap();
        assert_eq!(step.session.stage(), Stage::Closing);
    }

    #[test]
    fn a_failure_of_an_inactive_source_is_rejected() {
        assert_eq!(
            recording_two().step(Input::CaptureFailed(Source::SystemAudio)),
            Ok(recording_two()
                .step(Input::CaptureFailed(Source::SystemAudio))
                .unwrap())
        );
        assert_eq!(
            recording_one().step(Input::CaptureFailed(Source::SystemAudio)),
            Err(Rejected::SourceNotActive(Source::SystemAudio))
        );
    }

    #[test]
    fn a_device_failure_while_recording_drops_its_source() {
        let step = recording_two()
            .step(Input::DeviceFailed(Source::SystemAudio))
            .unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::SystemAudio),
                Effect::FinishSource(Source::SystemAudio),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Recording);
        assert_eq!(step.session.open_sources(), SourceSet::MICROPHONE);
        assert_eq!(
            step.session.outcome(),
            Some(Outcome::Incomplete(Fault::Device(Source::SystemAudio)))
        );
    }

    #[test]
    fn a_device_failure_of_an_inactive_source_is_rejected() {
        assert_eq!(
            recording_one().step(Input::DeviceFailed(Source::SystemAudio)),
            Err(Rejected::SourceNotActive(Source::SystemAudio))
        );
    }

    #[test]
    fn an_engine_failure_drops_an_open_source() {
        let step = recording_two()
            .step(Input::EngineFailed(Source::Microphone))
            .unwrap();
        assert_eq!(
            step.effects,
            vec![
                Effect::CloseSource(Source::Microphone),
                Effect::FinishSource(Source::Microphone),
            ]
        );
        assert_eq!(step.session.stage(), Stage::Recording);
        assert_eq!(
            step.session.outcome(),
            Some(Outcome::Incomplete(Fault::Engine(Source::Microphone)))
        );
    }

    #[test]
    fn an_engine_failure_of_a_finishing_source_only_marks_the_outcome() {
        let session = recording_two()
            .step(Input::CaptureFailed(Source::Microphone))
            .unwrap()
            .session;
        let step = session
            .step(Input::EngineFailed(Source::Microphone))
            .unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.stage(), Stage::Recording);
        assert_eq!(step.session.finishing_sources(), SourceSet::MICROPHONE);
        assert_eq!(
            step.session.outcome(),
            Some(Outcome::Incomplete(Fault::Capture(Source::Microphone)))
        );
    }

    #[test]
    fn an_engine_failure_of_an_unknown_source_is_rejected() {
        assert_eq!(
            recording_one().step(Input::EngineFailed(Source::SystemAudio)),
            Err(Rejected::SourceNotActive(Source::SystemAudio))
        );
    }

    #[test]
    fn an_engine_failure_marks_a_clean_finishing_source_incomplete() {
        let session = Session {
            permissions: Permissions::new(Permission::Granted, Permission::Granted),
            phase: Phase::Recording(Recording {
                sources: SourceSet::MICROPHONE,
                open: SourceSet::SYSTEM_AUDIO,
                finishing: SourceSet::MICROPHONE,
                progress: Progress {
                    transcript: Transcript::default(),
                    outcome: Outcome::Complete,
                },
            }),
        };
        let step = session
            .step(Input::EngineFailed(Source::Microphone))
            .unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(
            step.session.outcome(),
            Some(Outcome::Incomplete(Fault::Engine(Source::Microphone)))
        );
        assert_eq!(step.session.finishing_sources(), SourceSet::MICROPHONE);
    }

    #[test]
    fn engine_text_updates_the_transcript() {
        let committed = recording_one()
            .step(Input::EngineCommitted(Source::Microphone))
            .unwrap();
        assert_eq!(committed.effects, vec![]);
        let transcript = committed.session.transcript().unwrap();
        assert!(transcript.committed(Source::Microphone));
        assert!(!transcript.volatile(Source::Microphone));

        let floating = recording_one()
            .step(Input::EngineVolatile(Source::Microphone))
            .unwrap();
        assert_eq!(floating.effects, vec![]);
        assert!(
            floating
                .session
                .transcript()
                .unwrap()
                .volatile(Source::Microphone)
        );
        assert!(
            !floating
                .session
                .transcript()
                .unwrap()
                .committed(Source::Microphone)
        );

        let settled = drive(
            recording_one(),
            &[
                Input::EngineVolatile(Source::Microphone),
                Input::EngineCommitted(Source::Microphone),
            ],
        );
        let transcript = settled.transcript().unwrap();
        assert!(transcript.committed(Source::Microphone));
        assert!(!transcript.volatile(Source::Microphone));
    }

    #[test]
    fn engine_text_for_an_inactive_source_is_rejected() {
        for input in [
            Input::EngineCommitted(Source::SystemAudio),
            Input::EngineVolatile(Source::SystemAudio),
        ] {
            assert_eq!(
                recording_one().step(input),
                Err(Rejected::SourceNotActive(Source::SystemAudio))
            );
        }
    }

    #[test]
    fn a_finishing_source_accepts_engine_text() {
        let session = closed(recording_two());
        let step = session
            .step(Input::EngineCommitted(Source::SystemAudio))
            .unwrap();
        assert!(
            step.session
                .transcript()
                .unwrap()
                .committed(Source::SystemAudio)
        );
        let step = step
            .session
            .step(Input::EngineVolatile(Source::Microphone))
            .unwrap();
        assert!(
            step.session
                .transcript()
                .unwrap()
                .volatile(Source::Microphone)
        );
        let step = step
            .session
            .step(Input::EngineCommitted(Source::Microphone))
            .unwrap();
        assert!(
            step.session
                .transcript()
                .unwrap()
                .committed(Source::Microphone)
        );
        assert!(
            !step
                .session
                .transcript()
                .unwrap()
                .volatile(Source::Microphone)
        );
    }

    #[test]
    fn a_closed_segment_merges_its_text() {
        let session = drive(
            recording_one(),
            &[Input::EngineVolatile(Source::Microphone), Input::Stop],
        );
        let step = session
            .step(segment(SegmentOutcome::Complete, true))
            .unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.stage(), Stage::Reviewing);
        assert_eq!(step.session.sources(), Some(SourceSet::MICROPHONE));
        let transcript = step.session.transcript().unwrap();
        assert!(transcript.committed(Source::Microphone));
        assert!(!transcript.volatile(Source::Microphone));
        assert_eq!(step.session.outcome(), Some(Outcome::Complete));
    }

    #[test]
    fn a_closed_segment_keeps_text_committed_before_it() {
        let session = drive(
            recording_one(),
            &[Input::EngineCommitted(Source::Microphone), Input::Stop],
        );
        let step = session
            .step(segment(SegmentOutcome::Complete, false))
            .unwrap();
        assert!(
            step.session
                .transcript()
                .unwrap()
                .committed(Source::Microphone)
        );
    }

    #[test]
    fn an_incomplete_segment_marks_the_meeting_incomplete() {
        let step = closed(recording_one())
            .step(segment(SegmentOutcome::Incomplete, false))
            .unwrap();
        assert_eq!(
            step.session.outcome(),
            Some(Outcome::Incomplete(Fault::Engine(Source::Microphone)))
        );
        let step = step.session.step(Input::Confirm).unwrap();
        assert_eq!(step.session.stage(), Stage::Confirmed);
        assert_eq!(
            step.session.outcome(),
            Some(Outcome::Incomplete(Fault::Engine(Source::Microphone)))
        );
        assert_eq!(step.session.transcript(), Some(Transcript::default()));
    }

    #[test]
    fn a_closed_segment_for_a_source_not_finishing_is_rejected() {
        assert_eq!(
            recording_one().step(segment(SegmentOutcome::Complete, false)),
            Err(Rejected::SourceNotFinishing(Source::Microphone))
        );
        assert_eq!(
            closed(recording_one()).step(segment_for(
                Source::SystemAudio,
                SegmentOutcome::Complete,
                false
            )),
            Err(Rejected::SourceNotFinishing(Source::SystemAudio))
        );
    }

    #[test]
    fn a_segment_closed_while_another_source_records_keeps_recording() {
        let session = drive(recording_two(), &[Input::CaptureFailed(Source::Microphone)]);
        assert_eq!(session.stage(), Stage::Recording);
        let step = session
            .step(segment(SegmentOutcome::Complete, true))
            .unwrap();
        assert_eq!(step.session.stage(), Stage::Recording);
        assert_eq!(step.session.finishing_sources(), SourceSet::EMPTY);
        assert_eq!(step.session.open_sources(), SourceSet::SYSTEM_AUDIO);
        assert!(
            step.session
                .transcript()
                .unwrap()
                .committed(Source::Microphone)
        );
    }

    #[test]
    fn recording_rejects_the_rest() {
        for input in [
            Input::Resume,
            Input::Confirm,
            Input::Reset,
            Input::DeviceOpened(Source::Microphone),
        ] {
            assert_eq!(
                recording_one().step(input),
                Err(Rejected::WrongStage(Stage::Recording))
            );
        }
    }

    #[test]
    fn an_engine_failure_while_closing_marks_the_outcome() {
        let step = closed(recording_two())
            .step(Input::EngineFailed(Source::SystemAudio))
            .unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.stage(), Stage::Closing);
        assert_eq!(
            step.session.outcome(),
            Some(Outcome::Incomplete(Fault::Engine(Source::SystemAudio)))
        );
    }

    #[test]
    fn engine_text_while_closing_updates_the_transcript() {
        let step = closed(recording_two())
            .step(Input::EngineCommitted(Source::Microphone))
            .unwrap();
        assert!(
            step.session
                .transcript()
                .unwrap()
                .committed(Source::Microphone)
        );
        let step = step
            .session
            .step(Input::EngineVolatile(Source::SystemAudio))
            .unwrap();
        assert!(
            step.session
                .transcript()
                .unwrap()
                .volatile(Source::SystemAudio)
        );
    }

    #[test]
    fn closing_rejects_engine_events_for_an_idle_source() {
        let session = closed(recording_one());
        for input in [
            Input::EngineCommitted(Source::SystemAudio),
            Input::EngineVolatile(Source::SystemAudio),
            Input::EngineFailed(Source::SystemAudio),
        ] {
            assert_eq!(
                session.step(input),
                Err(Rejected::SourceNotActive(Source::SystemAudio))
            );
        }
    }

    #[test]
    fn closing_waits_for_every_finishing_source() {
        let step = closed(recording_two())
            .step(segment(SegmentOutcome::Complete, true))
            .unwrap();
        assert_eq!(step.session.stage(), Stage::Closing);
        assert_eq!(step.session.finishing_sources(), SourceSet::SYSTEM_AUDIO);
        assert_eq!(step.session.sources(), Some(SourceSet::MEETING));
        assert_eq!(
            step.session
                .transcript()
                .map(|t| t.committed(Source::Microphone)),
            Some(true)
        );
    }

    #[test]
    fn the_last_closed_segment_after_a_pause_goes_to_paused() {
        let session = paused_two();
        assert_eq!(session.stage(), Stage::Paused);
        assert_eq!(session.open_sources(), SourceSet::EMPTY);
        assert_eq!(session.sources(), Some(SourceSet::MEETING));
        assert_eq!(
            session
                .transcript()
                .map(|t| t.committed(Source::Microphone)),
            Some(true)
        );
    }

    #[test]
    fn the_last_closed_segment_after_a_stop_goes_to_reviewing() {
        let session = drive(
            closed(recording_one()),
            &[segment(SegmentOutcome::Complete, true)],
        );
        assert_eq!(session.stage(), Stage::Reviewing);
        assert_eq!(session.finishing_sources(), SourceSet::EMPTY);
        assert_eq!(session.sources(), Some(SourceSet::MICROPHONE));
    }

    #[test]
    fn stopping_after_a_pause_finishes_the_meeting() {
        let step = drive(recording_one(), &[Input::Pause])
            .step(Input::Stop)
            .unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.stage(), Stage::Closing);
        assert_eq!(step.session.finishing_sources(), SourceSet::MICROPHONE);
        let step = step
            .session
            .step(segment(SegmentOutcome::Complete, true))
            .unwrap();
        assert_eq!(step.session.stage(), Stage::Reviewing);
    }

    #[test]
    fn stopping_after_a_stop_is_rejected() {
        assert_eq!(
            closed(recording_one()).step(Input::Stop),
            Err(Rejected::WrongStage(Stage::Closing))
        );
    }

    #[test]
    fn closing_rejects_the_rest() {
        let session = closed(recording_one());
        for input in [
            Input::Pause,
            Input::Resume,
            Input::Confirm,
            Input::Reset,
            Input::Start(SourceSet::MICROPHONE),
            Input::DeviceOpened(Source::Microphone),
        ] {
            assert_eq!(
                session.step(input),
                Err(Rejected::WrongStage(Stage::Closing))
            );
        }
    }

    #[test]
    fn resuming_reopens_every_source_in_order() {
        let step = paused().step(Input::Resume).unwrap();
        assert_eq!(step.effects, vec![Effect::OpenSource(Source::Microphone)]);
        assert_eq!(step.session.stage(), Stage::Preparing);
        assert_eq!(step.session.sources(), Some(SourceSet::MICROPHONE));
        assert_eq!(step.session.pending_sources(), SourceSet::MICROPHONE);
        assert_eq!(step.session.open_sources(), SourceSet::EMPTY);

        let resumed = step
            .session
            .step(Input::DeviceOpened(Source::Microphone))
            .unwrap()
            .session;
        assert_eq!(resumed.stage(), Stage::Recording);
        assert_eq!(
            resumed
                .transcript()
                .map(|t| t.committed(Source::Microphone)),
            Some(true)
        );

        let two = paused_two().step(Input::Resume).unwrap();
        assert_eq!(
            two.effects,
            vec![
                Effect::OpenSource(Source::Microphone),
                Effect::OpenSource(Source::SystemAudio),
            ]
        );
        assert_eq!(two.session.pending_sources(), SourceSet::MEETING);
    }

    #[test]
    fn resuming_reports_the_first_source_without_permission() {
        let system_revoked = paused_two()
            .step(Input::PermissionRevoked(Source::SystemAudio))
            .unwrap()
            .session;
        assert_eq!(
            system_revoked.step(Input::Resume),
            Err(Rejected::PermissionDenied(Source::SystemAudio))
        );

        let microphone_revoked = drive(paused(), &[Input::PermissionRevoked(Source::Microphone)]);
        assert_eq!(
            microphone_revoked.step(Input::Resume),
            Err(Rejected::PermissionDenied(Source::Microphone))
        );
    }

    #[test]
    fn stopping_while_paused_goes_to_reviewing() {
        let step = paused().step(Input::Stop).unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.stage(), Stage::Reviewing);
        assert_eq!(step.session.sources(), Some(SourceSet::MICROPHONE));
        assert_eq!(
            step.session
                .transcript()
                .map(|t| t.committed(Source::Microphone)),
            Some(true)
        );
    }

    #[test]
    fn paused_rejects_the_rest() {
        let session = paused();
        for input in [
            Input::Pause,
            Input::Confirm,
            Input::Reset,
            Input::Start(SourceSet::MICROPHONE),
            Input::EngineCommitted(Source::Microphone),
            segment(SegmentOutcome::Complete, false),
        ] {
            assert_eq!(
                session.step(input),
                Err(Rejected::WrongStage(Stage::Paused))
            );
        }
    }

    #[test]
    fn confirming_saves_the_transcript() {
        let step = reviewing().step(Input::Confirm).unwrap();
        assert_eq!(step.effects, vec![Effect::SaveTranscript]);
        assert_eq!(step.session.stage(), Stage::Confirmed);
        assert_eq!(step.session.sources(), Some(SourceSet::MICROPHONE));
        assert_eq!(step.session.outcome(), Some(Outcome::Complete));
        assert_eq!(
            step.session
                .transcript()
                .map(|t| t.committed(Source::Microphone)),
            Some(true)
        );
    }

    #[test]
    fn confirming_without_text_is_allowed() {
        let session = drive(
            closed(recording_one()),
            &[segment(SegmentOutcome::Complete, false)],
        );
        let step = session.step(Input::Confirm).unwrap();
        assert_eq!(step.effects, vec![Effect::SaveTranscript]);
        assert_eq!(step.session.transcript(), Some(Transcript::default()));
    }

    #[test]
    fn resetting_while_reviewing_discards_the_transcript() {
        let step = reviewing().step(Input::Reset).unwrap();
        assert_eq!(step.effects, vec![Effect::DiscardTranscript]);
        assert_eq!(step.session.stage(), Stage::Idle);
        assert_eq!(step.session.sources(), None);
        assert_eq!(step.session.transcript(), None);
    }

    #[test]
    fn reviewing_rejects_the_rest() {
        let session = reviewing();
        for input in [
            Input::Stop,
            Input::Pause,
            Input::Resume,
            Input::Start(SourceSet::MICROPHONE),
        ] {
            assert_eq!(
                session.step(input),
                Err(Rejected::WrongStage(Stage::Reviewing))
            );
        }
    }

    #[test]
    fn resetting_after_confirming_is_silent() {
        let step = confirmed().step(Input::Reset).unwrap();
        assert_eq!(step.effects, vec![]);
        assert_eq!(step.session.stage(), Stage::Idle);
        assert_eq!(step.session.sources(), None);
    }

    #[test]
    fn confirmed_rejects_the_rest() {
        let session = confirmed();
        for input in [
            Input::Stop,
            Input::Pause,
            Input::Resume,
            Input::Confirm,
            Input::Start(SourceSet::MICROPHONE),
        ] {
            assert_eq!(
                session.step(input),
                Err(Rejected::WrongStage(Stage::Confirmed))
            );
        }
    }

    #[test]
    fn a_meeting_can_be_paused_resumed_and_stopped_again() {
        let session = drive(
            paused(),
            &[
                Input::Resume,
                Input::DeviceOpened(Source::Microphone),
                Input::EngineCommitted(Source::Microphone),
                Input::Stop,
            ],
        );
        assert_eq!(session.stage(), Stage::Closing);
        let session = drive(
            session,
            &[segment(SegmentOutcome::Complete, true), Input::Confirm],
        );
        assert_eq!(session.stage(), Stage::Confirmed);
        assert_eq!(
            session
                .transcript()
                .map(|t| t.committed(Source::Microphone)),
            Some(true)
        );
    }

    #[test]
    fn a_new_meeting_starts_from_a_finished_one() {
        let session = drive(
            drive(confirmed(), &[Input::Reset]),
            &[
                Input::Start(SourceSet::SYSTEM_AUDIO),
                Input::DeviceOpened(Source::SystemAudio),
            ],
        );
        assert_eq!(session.stage(), Stage::Recording);
        assert_eq!(session.open_sources(), SourceSet::SYSTEM_AUDIO);
    }
}
