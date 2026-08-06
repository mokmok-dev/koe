//! Ports isolating the Foundry SDK from the application core.
//!
//! [`FoundryAdapter`] owns all catalog, cache and runtime interactions. The
//! application never touches a foundry handle; it only sees
//! [`StreamingAsrSession`] instances created through the manager.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::types::{ModelDescriptor, ModelError, ModelId, ModelScope, ModelSelector, ModelVersion};

/// One artifact file reported by an adapter after install.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledFile {
    /// Absolute path inside the adapter-owned cache root.
    pub absolute_path: PathBuf,
    /// Path expressed relative to the cache root.
    pub relative_path: String,
    pub size: u64,
    pub sha256: String,
}

/// Result of a completed adapter install.
#[derive(Clone, Debug)]
pub struct InstalledArtifact {
    /// Root that owns every reported file, used for path validation.
    pub cache_root: PathBuf,
    pub model_id: ModelId,
    pub files: Vec<InstalledFile>,
}

/// Bounded live-session configuration frozen at creation.
#[derive(Clone, Debug)]
pub struct AsrSessionSettings {
    pub chunk_ms: u64,
    /// Optional BCP-47 language hint.
    pub language: Option<String>,
    pub push_queue_capacity: usize,
}

impl Default for AsrSessionSettings {
    fn default() -> Self {
        Self {
            // Default chunk size is decided after the benchmark baseline; the
            // specification candidate set is 80/160/560/1120 ms.
            chunk_ms: 160,
            language: None,
            push_queue_capacity: 100,
        }
    }
}

/// Canonical ASR input: signed 16-bit mono PCM at 16 kHz.
#[derive(Clone, Debug, Default)]
pub struct Pcm16Mono16k {
    pub samples: Vec<i16>,
    /// Position on the session-microsecond timeline.
    pub session_start_us: u64,
}

/// A streaming transcription event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AsrEvent {
    /// Stable segment id kept across interim revisions.
    pub segment_id: uuid::Uuid,
    pub text: String,
    /// Nesting of the timeline in microseconds.
    pub start_us: u64,
    pub end_us: u64,
    pub is_final: bool,
}

/// Materialized transcript returned by [`StreamingAsrSession::finish`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FinalTranscript {
    pub events: Vec<AsrEvent>,
}

/// Live audio streaming ASR session created by the model manager.
#[async_trait]
pub trait StreamingAsrSession: Send {
    /// Appends canonical PCM, honoring a bounded internal push queue.
    ///
    /// # Errors
    ///
    /// Returns an ASR failure when the session is not started, the queue is
    /// closed, or the native runtime fails.
    async fn append(
        &mut self,
        chunk: Pcm16Mono16k,
    ) -> Result<(), AsrError>;

    /// Returns the next pending transcription event, if any.
    ///
    /// The manager and benchmark runner use this to observe final events as
    /// they arrive instead of waiting for [`Self::finish`].
    ///
    /// # Errors
    ///
    /// Returns an ASR failure when result delivery fails.
    async fn poll_results(&mut self) -> Result<Option<AsrEvent>, AsrError>;

    /// Flushes remaining audio and returns the full transcript.
    ///
    /// # Errors
    ///
    /// Returns an ASR failure when the runtime fails during finalization.
    async fn finish(self: Box<Self>) -> Result<FinalTranscript, AsrError>;
}

/// Stable stream/ASR errors without content or paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AsrError {
    #[error("the ASR runtime is unavailable")]
    Unavailable,
    #[error("the ASR session is not active")]
    SessionNotActive,
    #[error("the ASR push queue is full")]
    Backpressure,
    #[error("the ASR runtime failed")]
    RuntimeFailed,
    #[error("the ASR input format is unsupported")]
    InvalidInput,
}

impl AsrError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Unavailable => "KOE-MODEL-UNAVAILABLE",
            Self::SessionNotActive | Self::Backpressure | Self::RuntimeFailed => {
                "KOE-MODEL-ASR-FAILED"
            },
            Self::InvalidInput => "KOE-MODEL-ASR-INVALID-INPUT",
        }
    }
}

/// Adapter failures normalized into stable model errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AdapterError {
    #[error("foundry runtime unavailable")]
    Unavailable,
    #[error("foundry catalog operation failed")]
    CatalogFailed,
    #[error("foundry download failed")]
    DownloadFailed,
    #[error("foundry runtime operation failed")]
    RuntimeFailed,
    #[error("model not found in foundry catalog/cache")]
    NotFound,
}

/// Port implemented by the foundry SDK and by test fixtures.
#[async_trait]
pub trait FoundryAdapter: Send + Sync {
    /// Stable backend label for capability reports.
    fn backend_name(&self) -> &'static str;

    /// Lists the online catalog. Never called under a `Denied` policy.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the service is unreachable or the
    /// catalog call fails.
    async fn list_catalog(&mut self) -> Result<Vec<ModelDescriptor>, AdapterError>;

    /// Resolves an alias or stable id from the catalog.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the model is not found or unreachable.
    async fn resolve(
        &mut self,
        selector: &ModelSelector,
    ) -> Result<ModelDescriptor, AdapterError>;

    /// Compares catalog versions; returns the newest for a descriptor.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the catalog cannot be queried.
    async fn latest_version(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<ModelVersion, AdapterError>;

    /// Lists models present in the local runtime cache.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the runtime is unreachable.
    async fn list_installed(&mut self) -> Result<Vec<ModelDescriptor>, AdapterError>;

    /// Lists model ids currently loaded into the runtime.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the runtime is unreachable.
    async fn list_loaded(&mut self) -> Result<Vec<ModelId>, AdapterError>;

    /// Downloads the model into the runtime cache. Long-running; checks the
    /// cancellation token before and after the SDK call.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the download fails.
    async fn install(
        &mut self,
        model: &ModelDescriptor,
        cancel: &tokio_util::sync::CancellationToken,
        force: bool,
    ) -> Result<InstalledArtifact, AdapterError>;

    /// Loads the model into the local runtime.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the runtime cannot load the model.
    async fn load(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<(), AdapterError>;

    /// Unloads the model from the local runtime.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the runtime cannot unload.
    async fn unload(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<(), AdapterError>;

    /// Removes the model from the local runtime cache.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when removal fails.
    async fn remove_from_cache(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<(), AdapterError>;

    /// Creates a live streaming ASR session for a loaded model.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the runtime is unavailable.
    async fn create_asr_session(
        &mut self,
        model: &ModelDescriptor,
        settings: &AsrSessionSettings,
    ) -> Result<Box<dyn StreamingAsrSession>, AdapterError>;

    /// Queries which scopes resolve locally without the online catalog.
    fn offline_scopes(&self) -> Vec<ModelScope>;

    /// Number of outbound adapter calls attempted, for offline-enforcement
    /// diagnostics. The default adapter reports `0`; test adapters override.
    fn outbound_attempts(&self) -> usize {
        0
    }
}

/// Converts adapter failures into stable manager errors.
#[must_use]
pub const fn map_adapter_error(error: AdapterError) -> ModelError {
    match error {
        AdapterError::Unavailable => ModelError::Unavailable,
        AdapterError::NotFound => ModelError::NotFound,
        AdapterError::CatalogFailed
        | AdapterError::DownloadFailed
        | AdapterError::RuntimeFailed => ModelError::Internal,
    }
}
