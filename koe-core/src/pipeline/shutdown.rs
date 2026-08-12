//! Graceful and force shutdown for [`super::RecordingPipeline`].
//!
//! Sequence (graceful):
//! 1. Set shutdown flag
//! 2. Stop native capture
//! 3. Drain consumer (budget: [`SHUTDOWN_BUDGET`])
//! 4. Finalize speech analyzer
//! 5. Finalize encoder + flush audio
//! 6. Write transcript
//! 7. Emit [`RecordingSummary`]
//!
//! Force mode skips the drain / ASR finalize wait but still finalizes the
//! encoder so on-disk audio containers stay valid.

use std::sync::atomic::Ordering;
use std::time::Duration;

use koe_ffi::{RecordingState, RecordingSummary, finalize_transcription, stop_capture};

use super::{PipelineError, PipelineState, RecordingPipeline};

/// Wall-clock budget for a graceful shutdown (spec: ≤ 2 s).
pub const SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);

/// How [`RecordingPipeline::stop_with`] finalizes the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownMode {
    /// Drain remaining audio, finalize ASR, then finalize encoder / transcript.
    Graceful,
    /// Abort the consumer quickly; skip ASR finalize. Encoder is still
    /// finalized so container files are not left corrupt.
    Force,
}

impl RecordingPipeline {
    /// Stops capture, drains remaining audio, finalizes outputs, and returns a summary.
    ///
    /// Completes within [`SHUTDOWN_BUDGET`] when possible; on timeout the consumer
    /// is aborted and finalization still runs (same integrity guarantees as a
    /// normal stop for encoder / transcript files).
    ///
    /// # Errors
    ///
    /// Returns [`PipelineError`] when the pipeline is not running, shutdown
    /// steps fail, or finalization fails.
    pub async fn stop(&mut self) -> Result<RecordingSummary, PipelineError> {
        self.stop_with(ShutdownMode::Graceful).await
    }

    /// Force-stops without waiting for a full drain / ASR flush.
    ///
    /// Partial transcript segments may be lost. Encoded audio is still
    /// finalized and flushed so files remain readable.
    ///
    /// # Errors
    ///
    /// Returns [`PipelineError`] when the pipeline is not running or
    /// finalization fails.
    pub async fn force_stop(&mut self) -> Result<RecordingSummary, PipelineError> {
        self.stop_with(ShutdownMode::Force).await
    }

    /// Shared shutdown entry point for graceful and force modes.
    ///
    /// # Errors
    ///
    /// Returns [`PipelineError`] when the pipeline is not running or a
    /// finalization step fails.
    pub async fn stop_with(
        &mut self,
        mode: ShutdownMode,
    ) -> Result<RecordingSummary, PipelineError> {
        if matches!(self.state, PipelineState::Stopped | PipelineState::Idle) {
            return Err(PipelineError::InvalidState(
                "pipeline is not recording".to_owned(),
            ));
        }

        self.shutdown.store(true, Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);
        self.publish_status(RecordingState::Stopping, 0.0, 0.0);

        // Dropping the capture handle drops the broadcast sender so the
        // consumer unblocks (Closed / drain) even if no more audio arrives.
        if let Some(handle) = self.capture_handle.take() {
            stop_capture(handle);
        }

        self.join_consumer(mode).await?;

        if mode == ShutdownMode::Graceful {
            if let Some(handle) = self.transcription_handle.take() {
                finalize_transcription(handle);
            }
        } else {
            // Force: drop without finalize — partials may be lost.
            self.transcription_handle.take();
        }

        let trailer = {
            let mut encoder = self
                .encoder
                .lock()
                .map_err(|_| PipelineError::InvalidState("encoder lock poisoned".to_owned()))?;
            encoder.finalize()?
        };

        let bytes_written = {
            let mut writer = self.file_writer.lock().await;
            if !trailer.is_empty() {
                let written = u64::try_from(trailer.len()).unwrap_or(u64::MAX);
                writer.write(&trailer).await?;
                self.bytes_written.fetch_add(written, Ordering::Relaxed);
            }
            writer.flush().await?;
            writer.bytes_written()
        };
        self.bytes_written.store(bytes_written, Ordering::Relaxed);

        if let Some(transcript_path) = &self.config.transcript_output_path {
            let body = {
                let transcript = self.transcript_fmt.lock().map_err(|_| {
                    PipelineError::InvalidState("transcript lock poisoned".to_owned())
                })?;
                // Trait-object path: `finalize` requires `Sized`; committed
                // output excludes in-flight partials (same as `finalize`).
                transcript.committed_output()
            };
            tokio::fs::write(transcript_path, body).await?;
        }

        let duration_sec = match &self.state {
            PipelineState::Recording { start_time, .. } => start_time.elapsed().as_secs_f64(),
            PipelineState::Paused {
                elapsed_before_pause,
                ..
            } => elapsed_before_pause.as_secs_f64(),
            _ => 0.0,
        };

        let segment_count = self
            .segments
            .lock()
            .map_err(|_| PipelineError::InvalidState("segments lock poisoned".to_owned()))?
            .len();

        self.state = PipelineState::Stopped;
        self.publish_status(RecordingState::Stopped, 0.0, 0.0);

        Ok(RecordingSummary {
            duration_sec,
            bytes_written,
            transcript_segment_count: u64::try_from(segment_count).unwrap_or(u64::MAX),
            dropped_audio_frames: self.drop_counter.load(Ordering::Relaxed),
            format: self.config.audio_format.clone(),
        })
    }

    async fn join_consumer(
        &mut self,
        mode: ShutdownMode,
    ) -> Result<(), PipelineError> {
        let Some(task) = self.consumer_task.take() else {
            return Ok(());
        };

        let budget = match mode {
            ShutdownMode::Graceful => SHUTDOWN_BUDGET,
            // Brief window so an already-idle consumer can exit cleanly;
            // then abort so force stop cannot hang on a stuck encode.
            ShutdownMode::Force => Duration::from_millis(50),
        };

        let abort = task.abort_handle();
        match tokio::time::timeout(budget, task).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(err))) => Err(err),
            Ok(Err(err)) => {
                if err.is_cancelled() {
                    Ok(())
                } else {
                    Err(PipelineError::InvalidState(format!(
                        "consumer task join failed: {err}"
                    )))
                }
            },
            Err(_elapsed) => {
                abort.abort();
                if mode == ShutdownMode::Graceful {
                    log::warn!(
                        "consumer drain exceeded {budget:?}; aborting and continuing finalize"
                    );
                }
                Ok(())
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Instant;

    use koe_ffi::{
        AppInfo, NativeProvider, OutputFormat, Permission, PermissionStatus, RecordingState,
        TranscriptFormat, TranscriptionSegment, register_native_provider,
    };

    use super::*;
    use crate::pipeline::{PipelineConfig, PipelineState};

    struct TestProvider {
        permissions: Vec<(Permission, PermissionStatus)>,
    }

    impl NativeProvider for TestProvider {
        fn check_permission(
            &self,
            permission: Permission,
        ) -> PermissionStatus {
            self.permissions
                .iter()
                .find(|(perm, _)| *perm == permission)
                .map_or(PermissionStatus::NotDetermined, |(_, status)| *status)
        }

        fn request_permission(
            &self,
            permission: Permission,
        ) -> PermissionStatus {
            self.check_permission(permission)
        }

        fn enumerate_apps(&self) -> Vec<AppInfo> {
            Vec::new()
        }
    }

    fn install_provider() {
        register_native_provider(Box::new(TestProvider {
            permissions: vec![(Permission::Microphone, PermissionStatus::Authorized)],
        }));
    }

    fn unique_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "koe-shutdown-{label}-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    fn test_config(output: &Path) -> PipelineConfig {
        PipelineConfig {
            source: koe_ffi::AudioSourceConfig::Microphone,
            output_path: output.to_path_buf(),
            transcript_output_path: None,
            locale: "en-US".into(),
            audio_format: OutputFormat::Wav {
                bits_per_sample: 16,
            },
            transcript_format: TranscriptFormat::Txt,
            enable_aec: false,
            comfort_noise: false,
            monitor: false,
            estimated_duration_hours: None,
        }
    }

    fn assert_valid_wav(path: &Path) {
        let bytes = std::fs::read(path).expect("read wav");
        assert!(
            bytes.len() >= 56,
            "WAV too small: {} bytes",
            bytes.len()
        );
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
    }

    #[tokio::test]
    async fn stop_immediately_is_clean() {
        install_provider();
        let output = unique_path("immediate");
        let mut pipeline = RecordingPipeline::start(test_config(&output))
            .await
            .expect("start");

        let started = Instant::now();
        let summary = pipeline.stop().await.expect("stop");
        assert!(started.elapsed() < SHUTDOWN_BUDGET);
        assert!(matches!(pipeline.state(), PipelineState::Stopped));
        assert!(matches!(
            summary.format,
            OutputFormat::Wav {
                bits_per_sample: 16
            }
        ));
        assert_valid_wav(&output);
        let _ = std::fs::remove_file(output);
    }

    #[tokio::test]
    async fn stop_after_audio_processes_all_frames() {
        install_provider();
        let output = unique_path("drain");
        let mut pipeline = RecordingPipeline::start(test_config(&output))
            .await
            .expect("start");

        // 3 chunks × 2 frames (stereo pairs) = 6 frames.
        let chunks = [
            vec![0.1, -0.1, 0.2, -0.2],
            vec![0.3, -0.3],
            vec![0.4, -0.4, 0.5, -0.5],
        ];
        let expected_frames: u64 = chunks.iter().map(|c| (c.len() / 2) as u64).sum();

        if let Some(handle) = pipeline.capture_handle() {
            for (i, samples) in chunks.iter().enumerate() {
                handle.deliver_audio(samples.clone(), (i as u64 + 1) * 20);
            }
        }

        // Let the consumer pull from the broadcast channel before stop.
        tokio::time::sleep(Duration::from_millis(30)).await;

        let summary = pipeline.stop().await.expect("stop");
        assert!(summary.bytes_written > 0);
        assert_eq!(summary.dropped_audio_frames, 0);
        assert_eq!(
            pipeline.metrics().total_frames_processed,
            expected_frames,
            "all fed frames must be processed before exit"
        );
        assert_valid_wav(&output);
        let _ = std::fs::remove_file(output);
    }

    #[tokio::test]
    async fn force_stop_does_not_corrupt_wav() {
        install_provider();
        let output = unique_path("force");
        let mut pipeline = RecordingPipeline::start(test_config(&output))
            .await
            .expect("start");

        if let Some(handle) = pipeline.capture_handle() {
            for i in 0..16 {
                handle.deliver_audio(vec![0.1, -0.1, 0.2, -0.2], i * 20);
            }
        }

        let started = Instant::now();
        let summary = pipeline.force_stop().await.expect("force_stop");
        assert!(started.elapsed() < SHUTDOWN_BUDGET);
        assert!(summary.bytes_written > 0);
        assert_valid_wav(&output);
        let _ = std::fs::remove_file(output);
    }

    #[tokio::test]
    async fn pause_then_stop_keeps_pre_pause_segments() {
        install_provider();
        let output = unique_path("pause-stop");
        let transcript = output.with_extension("txt");
        let mut config = test_config(&output);
        config.transcript_output_path = Some(transcript.clone());

        let mut pipeline = RecordingPipeline::start(config).await.expect("start");

        if let Some(handle) = pipeline.transcription_handle() {
            handle.deliver_segment(TranscriptionSegment {
                text: "before pause".into(),
                start_ms: 0,
                end_ms: 500,
                is_final: true,
                confidence: 0.9,
            });
        }

        pipeline.pause();
        assert!(pipeline.is_paused());

        if let Some(handle) = pipeline.capture_handle() {
            // Dropped while paused — must not affect transcript.
            handle.deliver_audio(vec![1.0, -1.0], 30);
        }

        let summary = pipeline.stop().await.expect("stop");
        assert_eq!(summary.transcript_segment_count, 1);

        let body = std::fs::read_to_string(&transcript).expect("transcript");
        assert!(body.contains("before pause"));

        let _ = std::fs::remove_file(output);
        let _ = std::fs::remove_file(transcript);
    }

    #[tokio::test]
    async fn double_stop_is_rejected() {
        install_provider();
        let output = unique_path("double");
        let mut pipeline = RecordingPipeline::start(test_config(&output))
            .await
            .expect("start");
        let _ = pipeline.stop().await.expect("first stop");
        let err = pipeline.stop().await.expect_err("second stop");
        assert!(matches!(err, PipelineError::InvalidState(_)));
        let _ = std::fs::remove_file(output);
    }

    #[tokio::test]
    async fn stop_emits_stopping_then_stopped() {
        install_provider();
        let output = unique_path("status");
        let mut pipeline = RecordingPipeline::start(test_config(&output))
            .await
            .expect("start");
        let mut progress = pipeline.subscribe_progress();

        let _ = pipeline.stop().await.expect("stop");

        let mut saw_stopping = false;
        let mut saw_stopped = false;
        while let Ok(status) = progress.try_recv() {
            saw_stopping |= status.state == RecordingState::Stopping;
            saw_stopped |= status.state == RecordingState::Stopped;
        }
        assert!(saw_stopping);
        assert!(saw_stopped);
        let _ = std::fs::remove_file(output);
    }
}
