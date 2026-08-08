//! Application coordinator shared by CLI and MCP adapters.

use std::{
    sync::mpsc::{self, Receiver, SyncSender},
    thread::{self, JoinHandle},
};

use koe_core::{CoreError, OperationId, SessionId, SessionState};
use koe_recording::{
    AudioGap, DriftCorrection, RecordingConfig, RecordingError, SessionManifest, SessionRecorder,
    TimelineBlock, TrackKind,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const COMMAND_CAPACITY: usize = 32;

/// Consent is separate from an OS permission grant and scoped to one start call.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecordingConsent {
    pub microphone: bool,
    pub system_audio: bool,
    pub storage: bool,
}

impl RecordingConsent {
    const fn permits_recording(
        self,
        system_requested: bool,
    ) -> bool {
        self.microphone && self.storage && (!system_requested || self.system_audio)
    }
}

/// Latest state retrieved after reconnect or event lag.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionSnapshot {
    pub operation_id: OperationId,
    pub session_id: Option<SessionId>,
    pub state: SessionState,
}

enum Command {
    Start {
        consent: RecordingConsent,
        response: SyncSender<Result<SessionSnapshot, AppError>>,
    },
    Append {
        samples: Vec<i16>,
        response: SyncSender<Result<(), AppError>>,
    },
    AppendBlock {
        samples: Vec<i16>,
        timeline: TimelineBlock,
        response: SyncSender<Result<(), AppError>>,
    },
    AppendTrack {
        kind: TrackKind,
        samples: Vec<i16>,
        response: SyncSender<Result<(), AppError>>,
    },
    AppendTrackBlock {
        kind: TrackKind,
        samples: Vec<i16>,
        timeline: TimelineBlock,
        response: SyncSender<Result<(), AppError>>,
    },
    RecordOverflow {
        source: TrackKind,
        count: u64,
        response: SyncSender<Result<(), AppError>>,
    },
    RecordPermission {
        source: TrackKind,
        result: String,
        response: SyncSender<Result<(), AppError>>,
    },
    RecordDiscontinuity {
        timeline_timestamp_ns: u64,
        response: SyncSender<Result<(), AppError>>,
    },
    RecordGap {
        gap: AudioGap,
        response: SyncSender<Result<(), AppError>>,
    },
    RecordDrift {
        correction: DriftCorrection,
        response: SyncSender<Result<(), AppError>>,
    },
    MarkRecording {
        response: SyncSender<Result<(), AppError>>,
    },
    MarkDegraded {
        response: SyncSender<Result<(), AppError>>,
    },
    Fail {
        code: String,
        response: SyncSender<Result<SessionSnapshot, AppError>>,
    },
    Stop {
        cancelled: bool,
        response: SyncSender<Result<SessionSnapshot, AppError>>,
    },
    Snapshot {
        response: SyncSender<SessionSnapshot>,
    },
    Shutdown {
        response: Option<SyncSender<Result<(), AppError>>>,
    },
}

/// Cloneable bounded command handle. The worker task exclusively owns the writer.
#[derive(Clone)]
pub struct RecorderCoordinator {
    commands: SyncSender<Command>,
}

impl RecorderCoordinator {
    /// Starts the single-owner coordinator task.
    #[must_use]
    pub fn spawn(config: RecordingConfig) -> (Self, CoordinatorTask) {
        let (commands, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let task = thread::spawn(move || run_coordinator(&config, &receiver));
        (
            Self {
                commands: commands.clone(),
            },
            CoordinatorTask {
                commands,
                task: Some(task),
            },
        )
    }

    /// Starts a recording after application consent validation.
    ///
    /// # Errors
    ///
    /// Returns consent, session conflict, channel, or storage failures.
    pub fn start(
        &self,
        consent: RecordingConsent,
    ) -> Result<SessionSnapshot, AppError> {
        self.request(|response| Command::Start { consent, response })?
    }

    /// Passes already-copied PCM from a non-realtime pipeline worker to storage.
    ///
    /// # Errors
    ///
    /// Returns a conflict if no recording is active or a storage/channel error.
    pub fn append(
        &self,
        samples: Vec<i16>,
    ) -> Result<(), AppError> {
        self.request(|response| Command::Append { samples, response })?
    }

    /// Writes microphone PCM with its canonical session-clock placement.
    ///
    /// # Errors
    ///
    /// Returns a conflict, invalid timeline, storage, or channel error.
    pub fn append_block(
        &self,
        samples: Vec<i16>,
        timeline: TimelineBlock,
    ) -> Result<(), AppError> {
        self.request(|response| Command::AppendBlock {
            samples,
            timeline,
            response,
        })?
    }

    /// Writes an isolated system or canonical mixed track.
    ///
    /// # Errors
    ///
    /// Returns a conflict, configuration, storage, or channel error.
    pub fn append_track(
        &self,
        kind: TrackKind,
        samples: Vec<i16>,
    ) -> Result<(), AppError> {
        self.request(|response| Command::AppendTrack {
            kind,
            samples,
            response,
        })?
    }

    /// Writes an isolated or mixed track with its session-clock placement.
    ///
    /// # Errors
    ///
    /// Returns a conflict, configuration, invalid timeline, storage, or channel error.
    pub fn append_track_block(
        &self,
        kind: TrackKind,
        samples: Vec<i16>,
        timeline: TimelineBlock,
    ) -> Result<(), AppError> {
        self.request(|response| Command::AppendTrackBlock {
            kind,
            samples,
            timeline,
            response,
        })?
    }

    /// Adds rejected callback frames to the durable session metric.
    ///
    /// # Errors
    ///
    /// Returns a conflict if no recording is active.
    pub fn record_overflow(
        &self,
        source: TrackKind,
        count: u64,
    ) -> Result<(), AppError> {
        self.request(|response| Command::RecordOverflow {
            source,
            count,
            response,
        })?
    }

    /// Persists the permission result observed when a source stream starts.
    ///
    /// # Errors
    ///
    /// Returns a conflict, persistence, or coordinator channel failure.
    pub fn record_permission_result(
        &self,
        source: TrackKind,
        result: impl Into<String>,
    ) -> Result<(), AppError> {
        let result = result.into();
        self.request(|response| Command::RecordPermission {
            source,
            result,
            response,
        })?
    }

    /// Adds a capture-clock discontinuity to the durable session timeline.
    ///
    /// # Errors
    ///
    /// Returns a conflict if no recording is active.
    pub fn record_discontinuity(
        &self,
        timeline_timestamp_ns: u64,
    ) -> Result<(), AppError> {
        self.request(|response| Command::RecordDiscontinuity {
            timeline_timestamp_ns,
            response,
        })?
    }

    /// Persists a source-specific missing interval.
    ///
    /// # Errors
    ///
    /// Returns a conflict if no session is active.
    pub fn record_gap(
        &self,
        gap: AudioGap,
    ) -> Result<(), AppError> {
        self.request(|response| Command::RecordGap { gap, response })?
    }

    /// Persists a measured drift correction.
    ///
    /// # Errors
    ///
    /// Returns a conflict if no session is active.
    pub fn record_drift_correction(
        &self,
        correction: DriftCorrection,
    ) -> Result<(), AppError> {
        self.request(|response| Command::RecordDrift {
            correction,
            response,
        })?
    }

    /// Marks a previously degraded session healthy after all requested sources
    /// have been reopened with their original format.
    ///
    /// # Errors
    ///
    /// Returns a conflict or coordinator channel failure.
    pub fn mark_recording(&self) -> Result<(), AppError> {
        self.request(|response| Command::MarkRecording { response })?
    }

    /// Marks an active session degraded immediately when one source is lost.
    ///
    /// # Errors
    ///
    /// Returns a conflict or coordinator channel failure.
    pub fn mark_degraded(&self) -> Result<(), AppError> {
        self.request(|response| Command::MarkDegraded { response })?
    }

    /// Persists a fatal asynchronous capture failure before returning it.
    ///
    /// # Errors
    ///
    /// Returns a conflict, persistence, or coordinator channel failure.
    pub fn fail(
        &self,
        code: impl Into<String>,
    ) -> Result<SessionSnapshot, AppError> {
        let code = code.into();
        self.request(|response| Command::Fail { code, response })?
    }

    /// Cooperatively stops and finalizes. Repeated calls return the same terminal
    /// snapshot and never finalize the writer twice.
    ///
    /// # Errors
    ///
    /// Returns a channel or storage error.
    pub fn stop(&self) -> Result<SessionSnapshot, AppError> {
        self.request(|response| Command::Stop {
            cancelled: false,
            response,
        })?
    }

    /// Cooperatively cancels while retaining partial artifacts.
    ///
    /// # Errors
    ///
    /// Returns a channel or storage error.
    pub fn cancel(&self) -> Result<SessionSnapshot, AppError> {
        self.request(|response| Command::Stop {
            cancelled: true,
            response,
        })?
    }

    /// Retrieves durable latest state instead of relying on an event stream.
    ///
    /// # Errors
    ///
    /// Returns an error if the coordinator task has stopped.
    pub fn snapshot(&self) -> Result<SessionSnapshot, AppError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Snapshot { response })
            .map_err(|_| AppError::CoordinatorStopped)?;
        receiver.recv().map_err(|_| AppError::CoordinatorStopped)
    }

    fn request<T>(
        &self,
        command: impl FnOnce(SyncSender<T>) -> Command,
    ) -> Result<T, AppError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.commands
            .send(command(response))
            .map_err(|_| AppError::CoordinatorStopped)?;
        receiver.recv().map_err(|_| AppError::CoordinatorStopped)
    }
}

/// Join guard for orderly application shutdown.
pub struct CoordinatorTask {
    commands: SyncSender<Command>,
    task: Option<JoinHandle<()>>,
}

impl CoordinatorTask {
    /// Requests shutdown and joins the worker.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::CoordinatorStopped`] if command delivery or the
    /// shutdown response channel fails, [`AppError::CoordinatorPanicked`] if
    /// the worker panics, or propagates recorder finalization/shutdown errors
    /// reported by the coordinator.
    pub fn shutdown(mut self) -> Result<(), AppError> {
        let (response, receiver) = mpsc::sync_channel(1);
        let delivered = self
            .commands
            .send(Command::Shutdown {
                response: Some(response),
            })
            .is_ok();
        let shutdown_result = if delivered {
            receiver
                .recv()
                .map_err(|_| AppError::CoordinatorStopped)
                .and_then(|result| result)
        } else {
            Err(AppError::CoordinatorStopped)
        };
        if let Some(task) = self.task.take() {
            task.join().map_err(|_| AppError::CoordinatorPanicked)?;
        }
        shutdown_result
    }
}

impl Drop for CoordinatorTask {
    fn drop(&mut self) {
        let _ignored = self.commands.send(Command::Shutdown { response: None });
        if let Some(task) = self.task.take() {
            let _ignored = task.join();
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run_coordinator(
    config: &RecordingConfig,
    commands: &Receiver<Command>,
) {
    let mut snapshot = idle_snapshot();
    let mut recorder: Option<SessionRecorder> = None;
    let mut terminal_manifest: Option<SessionManifest> = None;
    let mut terminal_failure = false;

    while let Ok(command) = commands.recv() {
        match command {
            Command::Start { consent, response } => {
                let system_requested = config
                    .additional_tracks
                    .iter()
                    .any(|track| track.kind == TrackKind::System);
                let result = if !consent.permits_recording(system_requested) {
                    Err(AppError::Core(CoreError::ConsentRequired))
                } else if recorder.is_some()
                    || matches!(
                        snapshot.state,
                        SessionState::Preparing
                            | SessionState::Starting
                            | SessionState::Recording
                            | SessionState::Degraded
                            | SessionState::Stopping
                            | SessionState::Finalizing
                    )
                {
                    Err(AppError::Core(CoreError::SessionConflict))
                } else {
                    terminal_manifest = None;
                    terminal_failure = false;
                    match SessionRecorder::start(config.clone()) {
                        Ok(new_recorder) => {
                            snapshot.operation_id = OperationId::new();
                            snapshot.session_id = Some(new_recorder.session_id());
                            snapshot.state = SessionState::Starting;
                            recorder = Some(new_recorder);
                            Ok(snapshot.clone())
                        },
                        Err(error) => Err(AppError::Recording(error)),
                    }
                };
                let _ignored = response.send(result);
            },
            Command::Append { samples, response } => {
                let result = append_samples(
                    &samples,
                    &mut recorder,
                    &mut snapshot,
                    &mut terminal_manifest,
                    &mut terminal_failure,
                );
                let _ignored = response.send(result);
            },
            Command::AppendBlock {
                samples,
                timeline,
                response,
            } => {
                let result = append_timeline_samples(
                    &samples,
                    timeline,
                    &mut recorder,
                    &mut snapshot,
                    &mut terminal_manifest,
                    &mut terminal_failure,
                );
                let _ignored = response.send(result);
            },
            Command::AppendTrack {
                kind,
                samples,
                response,
            } => {
                let result = append_track_samples(
                    kind,
                    &samples,
                    &mut recorder,
                    &mut snapshot,
                    &mut terminal_manifest,
                    &mut terminal_failure,
                );
                let _ignored = response.send(result);
            },
            Command::AppendTrackBlock {
                kind,
                samples,
                timeline,
                response,
            } => {
                let result = append_track_timeline_samples(
                    kind,
                    &samples,
                    timeline,
                    &mut recorder,
                    &mut snapshot,
                    &mut terminal_manifest,
                    &mut terminal_failure,
                );
                let _ignored = response.send(result);
            },
            Command::RecordOverflow {
                source,
                count,
                response,
            } => {
                let result = recorder
                    .as_mut()
                    .ok_or(AppError::Core(CoreError::SessionConflict))
                    .map(|active| active.record_overflow(source, count));
                let _ignored = response.send(result);
            },
            Command::RecordPermission {
                source,
                result,
                response,
            } => {
                let result = recorder
                    .as_mut()
                    .ok_or(AppError::Core(CoreError::SessionConflict))
                    .and_then(|active| {
                        active
                            .record_permission_result(source, &result)
                            .map_err(AppError::Recording)
                    });
                let _ignored = response.send(result);
            },
            Command::RecordDiscontinuity {
                timeline_timestamp_ns,
                response,
            } => {
                let result = recorder
                    .as_mut()
                    .ok_or(AppError::Core(CoreError::SessionConflict))
                    .map(|active| active.record_discontinuity(timeline_timestamp_ns));
                let _ignored = response.send(result);
            },
            Command::RecordGap { gap, response } => {
                let device_lost = gap.reason == "device-lost";
                let result = recorder
                    .as_mut()
                    .ok_or(AppError::Core(CoreError::SessionConflict))
                    .and_then(|active| {
                        active.record_gap(gap);
                        if device_lost {
                            active.mark_degraded().map_err(AppError::Recording)?;
                            snapshot.state = SessionState::Degraded;
                        }
                        Ok(())
                    });
                let _ignored = response.send(result);
            },
            Command::RecordDrift {
                correction,
                response,
            } => {
                let result = recorder
                    .as_mut()
                    .ok_or(AppError::Core(CoreError::SessionConflict))
                    .map(|active| active.record_drift_correction(correction));
                let _ignored = response.send(result);
            },
            Command::MarkRecording { response } => {
                let result = recorder
                    .as_mut()
                    .ok_or(AppError::Core(CoreError::SessionConflict))
                    .and_then(|active| {
                        active.mark_recording().map_err(AppError::Recording)?;
                        snapshot.state = SessionState::Recording;
                        Ok(())
                    });
                let _ignored = response.send(result);
            },
            Command::MarkDegraded { response } => {
                let result = recorder
                    .as_mut()
                    .ok_or(AppError::Core(CoreError::SessionConflict))
                    .and_then(|active| {
                        active.mark_degraded().map_err(AppError::Recording)?;
                        snapshot.state = SessionState::Degraded;
                        Ok(())
                    });
                let _ignored = response.send(result);
            },
            Command::Fail { code, response } => {
                let result = if let Some(mut active) = recorder.take() {
                    snapshot.state = SessionState::Failed;
                    terminal_failure = true;
                    match active.mark_failed_with_code(&code) {
                        Ok(manifest) => {
                            terminal_manifest = Some(manifest);
                            Ok(snapshot.clone())
                        },
                        Err(error) => Err(AppError::Recording(error)),
                    }
                } else if terminal_manifest.is_some() {
                    Ok(snapshot.clone())
                } else {
                    Err(AppError::Core(CoreError::SessionConflict))
                };
                let _ignored = response.send(result);
            },
            Command::Stop {
                cancelled,
                response,
            } => {
                let result = stop_recording(
                    cancelled,
                    &mut recorder,
                    &mut snapshot,
                    &mut terminal_manifest,
                    &mut terminal_failure,
                );
                let _ignored = response.send(result);
            },
            Command::Snapshot { response } => {
                let _ignored = response.send(snapshot.clone());
            },
            Command::Shutdown { response } => {
                let result = shutdown_recorder(recorder.take());
                if let Some(response) = response {
                    let _ignored = response.send(result);
                }
                break;
            },
        }
    }
}

fn stop_recording(
    cancelled: bool,
    recorder: &mut Option<SessionRecorder>,
    snapshot: &mut SessionSnapshot,
    terminal_manifest: &mut Option<SessionManifest>,
    terminal_failure: &mut bool,
) -> Result<SessionSnapshot, AppError> {
    if let Some(mut active) = recorder.take() {
        snapshot.state = SessionState::Finalizing;
        return match active.finalize(cancelled) {
            Ok(manifest) => {
                snapshot.state = manifest.state;
                *terminal_manifest = Some(manifest);
                Ok(snapshot.clone())
            },
            Err(error) => {
                snapshot.state = SessionState::Failed;
                *terminal_failure = true;
                *terminal_manifest = active.mark_failed().ok();
                Err(AppError::Recording(error))
            },
        };
    }
    if terminal_manifest.is_none() {
        return Err(AppError::Core(CoreError::SessionConflict));
    }
    if *terminal_failure {
        Err(AppError::FinalizationFailed)
    } else {
        Ok(snapshot.clone())
    }
}

fn append_samples(
    samples: &[i16],
    recorder: &mut Option<SessionRecorder>,
    snapshot: &mut SessionSnapshot,
    terminal_manifest: &mut Option<SessionManifest>,
    terminal_failure: &mut bool,
) -> Result<(), AppError> {
    let result = recorder
        .as_mut()
        .map_or(Err(AppError::Core(CoreError::SessionConflict)), |active| {
            active.write_samples(samples).map_err(AppError::Recording)
        });
    if result.is_err() && recorder.is_some() {
        transition_active_to_failed(recorder, snapshot, terminal_manifest, terminal_failure);
    }
    result
}

fn append_timeline_samples(
    samples: &[i16],
    timeline: TimelineBlock,
    recorder: &mut Option<SessionRecorder>,
    snapshot: &mut SessionSnapshot,
    terminal_manifest: &mut Option<SessionManifest>,
    terminal_failure: &mut bool,
) -> Result<(), AppError> {
    let result =
        recorder
            .as_mut()
            .map_or(Err(AppError::Core(CoreError::SessionConflict)), |active| {
                active
                    .write_samples_block(samples, timeline)
                    .map_err(AppError::Recording)
            });
    if result.is_err() && recorder.is_some() {
        transition_active_to_failed(recorder, snapshot, terminal_manifest, terminal_failure);
    }
    result
}

fn append_track_samples(
    kind: TrackKind,
    samples: &[i16],
    recorder: &mut Option<SessionRecorder>,
    snapshot: &mut SessionSnapshot,
    terminal_manifest: &mut Option<SessionManifest>,
    terminal_failure: &mut bool,
) -> Result<(), AppError> {
    let result =
        recorder
            .as_mut()
            .map_or(Err(AppError::Core(CoreError::SessionConflict)), |active| {
                active
                    .write_track(kind, samples)
                    .map_err(AppError::Recording)
            });
    if result.is_err() && recorder.is_some() {
        transition_active_to_failed(recorder, snapshot, terminal_manifest, terminal_failure);
    }
    result
}

fn append_track_timeline_samples(
    kind: TrackKind,
    samples: &[i16],
    timeline: TimelineBlock,
    recorder: &mut Option<SessionRecorder>,
    snapshot: &mut SessionSnapshot,
    terminal_manifest: &mut Option<SessionManifest>,
    terminal_failure: &mut bool,
) -> Result<(), AppError> {
    let result =
        recorder
            .as_mut()
            .map_or(Err(AppError::Core(CoreError::SessionConflict)), |active| {
                active
                    .write_track_block(kind, samples, timeline)
                    .map_err(AppError::Recording)
            });
    if result.is_err() && recorder.is_some() {
        transition_active_to_failed(recorder, snapshot, terminal_manifest, terminal_failure);
    }
    result
}

fn transition_active_to_failed(
    recorder: &mut Option<SessionRecorder>,
    snapshot: &mut SessionSnapshot,
    terminal_manifest: &mut Option<SessionManifest>,
    terminal_failure: &mut bool,
) {
    if let Some(mut active) = recorder.take() {
        snapshot.state = SessionState::Failed;
        *terminal_failure = true;
        *terminal_manifest = active.mark_failed().ok();
    }
}

fn shutdown_recorder(recorder: Option<SessionRecorder>) -> Result<(), AppError> {
    recorder.map_or(Ok(()), |mut active| match active.finalize(true) {
        Ok(_) => Ok(()),
        Err(error) => {
            let _failure_snapshot = active.mark_failed();
            Err(AppError::Recording(error))
        },
    })
}

fn idle_snapshot() -> SessionSnapshot {
    SessionSnapshot {
        operation_id: OperationId::new(),
        session_id: None,
        state: SessionState::Idle,
    }
}

/// Application-layer errors with stable codes and redacted display text.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    Core(#[from] CoreError),
    #[error("{0}")]
    Recording(#[from] RecordingError),
    #[error("recorder coordinator stopped")]
    CoordinatorStopped,
    #[error("recorder coordinator failed")]
    CoordinatorPanicked,
    #[error("recording finalization previously failed")]
    FinalizationFailed,
}

impl AppError {
    /// Stable code for every frontend.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Core(error) => error.code(),
            Self::Recording(error) => error.code(),
            Self::FinalizationFailed => "KOE-STORE-FINALIZE-FAILED",
            Self::CoordinatorPanicked | Self::CoordinatorStopped => {
                "KOE-SESSION-COORDINATOR-STOPPED"
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use koe_core::{NetworkPolicy, SessionState};
    use koe_recording::{AudioGap, RecordingConfig, TrackConfig, TrackKind};
    use tempfile::TempDir;

    use super::{RecorderCoordinator, RecordingConsent};

    fn config(root: &TempDir) -> RecordingConfig {
        RecordingConfig {
            data_root: root.path().to_path_buf(),
            samples_per_segment: 4,
            sample_rate: 16_000,
            channels: 1,
            native_sample_format: "signed-16-bit-pcm".to_owned(),
            queue_capacity: 8,
            network_policy: NetworkPolicy::Denied,
            backend: "test".to_owned(),
            source_device_id: "fixture".to_owned(),
            permission_result: "granted".to_owned(),
            additional_tracks: Vec::new(),
        }
    }

    #[test]
    fn requires_fresh_application_consent() {
        let root = TempDir::new().expect("temp");
        let (coordinator, task) = RecorderCoordinator::spawn(config(&root));
        let error = coordinator
            .start(RecordingConsent::default())
            .expect_err("consent must fail");
        assert_eq!(error.code(), "KOE-POLICY-CONSENT-REQUIRED");
        task.shutdown().expect("shutdown");
    }

    #[test]
    fn system_track_requires_separate_consent() {
        let root = TempDir::new().expect("temp");
        let mut recording_config = config(&root);
        recording_config.additional_tracks.push(TrackConfig {
            kind: TrackKind::System,
            sample_rate: 16_000,
            channels: 1,
            samples_per_segment: 4,
            backend: "fixture".to_owned(),
            source_device_id: "system-fixture".to_owned(),
            permission_result: "granted".to_owned(),
            native_sample_format: "signed-16-bit-pcm".to_owned(),
        });
        let (coordinator, task) = RecorderCoordinator::spawn(recording_config);
        let error = coordinator
            .start(RecordingConsent {
                microphone: true,
                system_audio: false,
                storage: true,
            })
            .expect_err("system consent must fail");
        assert_eq!(error.code(), "KOE-POLICY-CONSENT-REQUIRED");
        task.shutdown().expect("shutdown");
    }

    #[test]
    fn device_loss_degrades_without_stopping_remaining_sources() {
        let root = TempDir::new().expect("temp");
        let (coordinator, task) = RecorderCoordinator::spawn(config(&root));
        coordinator
            .start(RecordingConsent {
                microphone: true,
                system_audio: false,
                storage: true,
            })
            .expect("start");
        coordinator
            .record_gap(AudioGap {
                source: TrackKind::Microphone,
                start_us: 10,
                duration_us: 0,
                reason: "device-lost".to_owned(),
            })
            .expect("gap");
        assert_eq!(
            coordinator.snapshot().expect("snapshot").state,
            SessionState::Degraded
        );
        coordinator
            .append(vec![1, 2])
            .expect("remaining source data");
        coordinator.mark_recording().expect("source recovered");
        assert_eq!(
            coordinator.snapshot().expect("snapshot").state,
            SessionState::Recording
        );
        assert_eq!(
            coordinator.stop().expect("stop").state,
            SessionState::Completed
        );
        task.shutdown().expect("shutdown");
    }

    #[test]
    fn asynchronous_failure_code_is_persisted() {
        let root = TempDir::new().expect("temp");
        let (coordinator, task) = RecorderCoordinator::spawn(config(&root));
        let recording = coordinator
            .start(RecordingConsent {
                microphone: true,
                system_audio: false,
                storage: true,
            })
            .expect("start");
        coordinator
            .fail("KOE-AUDIO-PERMISSION-DENIED")
            .expect("persist failure");
        assert_eq!(
            coordinator.snapshot().expect("snapshot").state,
            SessionState::Failed
        );
        task.shutdown().expect("shutdown");
        let session = recording.session_id.expect("session");
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                root.path()
                    .join("sessions")
                    .join(session.to_string())
                    .join("session.json"),
            )
            .expect("manifest"),
        )
        .expect("manifest JSON");
        assert_eq!(manifest["state"], "failed");
        assert_eq!(manifest["failure_code"], "KOE-AUDIO-PERMISSION-DENIED");
    }

    #[test]
    fn owns_one_session_and_idempotently_stops() {
        let root = TempDir::new().expect("temp");
        let (coordinator, task) = RecorderCoordinator::spawn(config(&root));
        let consent = RecordingConsent {
            microphone: true,
            system_audio: false,
            storage: true,
        };
        let recording = coordinator.start(consent).expect("start");
        assert_eq!(recording.state, SessionState::Starting);
        let session = recording.session_id.expect("session");
        let manifest_path = root
            .path()
            .join("sessions")
            .join(session.to_string())
            .join("session.json");
        let starting: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).expect("starting manifest"))
                .expect("manifest JSON");
        assert_eq!(starting["state"], "starting");
        coordinator.mark_recording().expect("streams started");
        let recording_manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).expect("recording manifest"))
                .expect("manifest JSON");
        assert_eq!(recording_manifest["state"], "recording");
        assert!(coordinator.start(consent).is_err());
        coordinator.append(vec![1, 2, 3]).expect("append");
        let completed = coordinator.stop().expect("stop");
        let repeated = coordinator.stop().expect("repeated stop");
        assert_eq!(completed, repeated);
        assert_eq!(completed.state, SessionState::Completed);
        task.shutdown().expect("shutdown");
    }

    #[test]
    fn shutdown_cancels_an_active_session() {
        let root = TempDir::new().expect("temp");
        let (coordinator, task) = RecorderCoordinator::spawn(config(&root));
        coordinator
            .start(RecordingConsent {
                microphone: true,
                system_audio: false,
                storage: true,
            })
            .expect("start");
        task.shutdown().expect("shutdown");

        let recovered = koe_recording::recover_sessions(root.path()).expect("recovery scan");
        assert!(
            recovered.is_empty(),
            "shutdown should finalize cancellation"
        );
    }

    #[test]
    #[cfg(unix)]
    fn finalize_failure_becomes_stable_failed_state() {
        let root = TempDir::new().expect("temp");
        let (coordinator, task) = RecorderCoordinator::spawn(config(&root));
        let recording = coordinator
            .start(RecordingConsent {
                microphone: true,
                system_audio: false,
                storage: true,
            })
            .expect("start");
        let session = recording.session_id.expect("session id");
        std::fs::remove_file(
            root.path()
                .join("sessions")
                .join(session.to_string())
                .join("audio/mic-000001.wav"),
        )
        .expect("remove open WAV name");

        let first = coordinator.stop().expect_err("finalize must fail");
        assert_eq!(first.code(), "KOE-STORE-FINALIZE-FAILED");
        assert_eq!(
            coordinator.snapshot().expect("snapshot").state,
            SessionState::Failed
        );
        let repeated = coordinator.stop().expect_err("failure is stable");
        assert_eq!(repeated.code(), "KOE-STORE-FINALIZE-FAILED");
        task.shutdown().expect("shutdown");
    }

    #[test]
    #[cfg(unix)]
    fn checkpoint_failure_cannot_be_republished_as_cancelled_on_shutdown() {
        let root = TempDir::new().expect("temp");
        let mut recording_config = config(&root);
        recording_config.sample_rate = 1;
        let (coordinator, task) = RecorderCoordinator::spawn(recording_config);
        let recording = coordinator
            .start(RecordingConsent {
                microphone: true,
                system_audio: false,
                storage: true,
            })
            .expect("start");
        let session = recording.session_id.expect("session id");
        let session_dir = root.path().join("sessions").join(session.to_string());
        std::fs::remove_file(session_dir.join("audio/mic-000001.wav"))
            .expect("remove open WAV name");

        let error = coordinator
            .append(vec![1, 2, 3, 4, 5])
            .expect_err("checkpoint must fail");
        assert_eq!(error.code(), "KOE-STORE-FINALIZE-FAILED");
        assert_eq!(
            coordinator.snapshot().expect("snapshot").state,
            SessionState::Failed
        );
        task.shutdown().expect("shutdown");

        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(session_dir.join("session.json")).expect("manifest"),
        )
        .expect("manifest JSON");
        assert_eq!(manifest["state"], "failed");
    }

    #[test]
    #[cfg(unix)]
    fn additional_track_failure_transitions_to_failed() {
        let root = TempDir::new().expect("temp");
        let mut recording_config = config(&root);
        recording_config.sample_rate = 1;
        recording_config.additional_tracks.push(TrackConfig {
            kind: TrackKind::System,
            sample_rate: 1,
            channels: 1,
            samples_per_segment: 10,
            backend: "fixture".to_owned(),
            source_device_id: "system-fixture".to_owned(),
            permission_result: "granted".to_owned(),
            native_sample_format: "signed-16-bit-pcm".to_owned(),
        });
        let (coordinator, task) = RecorderCoordinator::spawn(recording_config);
        let recording = coordinator
            .start(RecordingConsent {
                microphone: true,
                system_audio: true,
                storage: true,
            })
            .expect("start");
        let session = recording.session_id.expect("session");
        std::fs::remove_file(
            root.path()
                .join("sessions")
                .join(session.to_string())
                .join("audio/system-000001.wav"),
        )
        .expect("remove open system WAV name");

        coordinator
            .append_track(TrackKind::System, vec![1, 2, 3, 4, 5])
            .expect_err("system checkpoint must fail");
        assert_eq!(
            coordinator.snapshot().expect("snapshot").state,
            SessionState::Failed
        );
        task.shutdown().expect("shutdown");
    }
}
