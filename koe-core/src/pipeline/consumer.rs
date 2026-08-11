//! Async consumer loop draining the audio broadcast channel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use koe_ffi::TranscriptionHandle;
use tokio::sync::{Mutex as AsyncMutex, broadcast};
use tokio::task::JoinHandle;

use crate::codec::AudioEncoder;

use super::PipelineError;
use super::chunk::AudioChunk;
use super::file_writer::FileWriter;
use super::metrics::PipelineMetrics;

/// Shared state passed into the consumer task.
pub struct ConsumerContext {
    pub encoder: Arc<Mutex<Box<dyn AudioEncoder>>>,
    pub transcription: Arc<TranscriptionHandle>,
    pub writer: Arc<AsyncMutex<FileWriter>>,
    pub metrics: Arc<PipelineMetrics>,
    pub shutdown: Arc<AtomicBool>,
}

/// Spawns the background consumer that encodes audio and feeds transcription.
#[must_use]
pub fn spawn_consumer(
    mut rx: broadcast::Receiver<AudioChunk>,
    ctx: ConsumerContext,
) -> JoinHandle<Result<(), PipelineError>> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(chunk) => {
                    process_chunk(&ctx, chunk).await?;
                },
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    log::warn!("Consumer lagged by {dropped} chunks; audio dropped");
                    #[allow(clippy::useless_conversion)]
                    ctx.metrics
                        .record_drops(u64::try_from(dropped).unwrap_or(u64::MAX));
                },
                Err(broadcast::error::RecvError::Closed) => break,
            }

            if ctx.shutdown.load(Ordering::Relaxed) {
                while let Ok(chunk) = rx.try_recv() {
                    process_chunk(&ctx, chunk).await?;
                }
                break;
            }
        }
        Ok(())
    })
}

async fn process_chunk(
    ctx: &ConsumerContext,
    chunk: AudioChunk,
) -> Result<(), PipelineError> {
    ctx.metrics
        .record_frames(u64::try_from(chunk.frame_count).unwrap_or(0));

    let encoded = {
        let mut encoder = ctx
            .encoder
            .lock()
            .map_err(|_| PipelineError::InvalidState("encoder lock poisoned".to_owned()))?;
        encoder.encode(&chunk.samples)?
    };

    if !encoded.is_empty() {
        let mut writer = ctx.writer.lock().await;
        writer.write(&encoded).await?;
        drop(writer);
    }

    koe_ffi::feed_transcription_audio(Arc::clone(&ctx.transcription), chunk.samples);

    Ok(())
}
