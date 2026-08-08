//! Ports isolating the Foundry SDK from the application core.
//!
//! [`FoundryAdapter`] owns all catalog, cache and runtime interactions. The
//! application never touches a foundry handle; it only sees
//! [`StreamingAsrSession`] instances created through the manager.

use std::{
    fmt,
    path::{Component, Path, PathBuf},
};

use async_trait::async_trait;

use crate::types::{
    ModelArtifactFailure, ModelDescriptor, ModelError, ModelId, ModelScope, ModelSelector,
    ModelVersion,
};

/// Maximum aggregate artifact bytes verified during one installation (16 GiB).
pub const MAX_ARTIFACT_INVENTORY_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// One artifact file reported by an adapter after install.
///
/// The manager rejects symlinks, requires `absolute_path` to equal the
/// canonical `cache_root.join(&relative_path)`, and derives authoritative size
/// and SHA-256 metadata in one streamed verification pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledFile {
    cache_root: PathBuf,
    absolute_path: PathBuf,
    relative_path: String,
}

impl InstalledFile {
    /// Validates one cache-root-relative regular-file claim.
    ///
    /// This performs blocking canonicalization and metadata I/O. Async adapter
    /// implementations should call it from [`tokio::task::spawn_blocking`],
    /// preferably once for a batch of paths.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::InvalidArtifact`] when the path escapes the
    /// cache root, is not a regular file, or has multiple hard links.
    pub fn try_from_cache_path_blocking(
        cache_root: &Path,
        relative_path: impl Into<PathBuf>,
    ) -> Result<Self, AdapterError> {
        let relative_path = relative_path.into();
        let relative_text = relative_path.to_str().ok_or(AdapterError::InvalidArtifact(
            ArtifactValidationError::InvalidPath,
        ))?;
        if relative_path.as_os_str().is_empty()
            || relative_path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            || cfg!(unix) && relative_text.contains('\\')
            || cfg!(windows) && relative_text.contains(':')
        {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::InvalidPath,
            ));
        }
        let normalized_relative = relative_path
            .components()
            .map(|component| match component {
                Component::Normal(value) => value.to_str().ok_or(AdapterError::InvalidArtifact(
                    ArtifactValidationError::InvalidPath,
                )),
                _ => Err(AdapterError::InvalidArtifact(
                    ArtifactValidationError::InvalidPath,
                )),
            })
            .collect::<Result<Vec<_>, _>>()?
            .join("/");
        let cache_root = cache_root
            .canonicalize()
            .map_err(|_| AdapterError::InvalidArtifact(ArtifactValidationError::InvalidPath))?;
        reject_symlink_components(&cache_root, &relative_path)?;
        let absolute_path = cache_root.join(&relative_path);
        let metadata = std::fs::symlink_metadata(&absolute_path)
            .map_err(|_| AdapterError::InvalidArtifact(ArtifactValidationError::MissingFile))?;
        if metadata.file_type().is_symlink() {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::Symlink,
            ));
        }
        if !metadata.is_file() {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::MissingFile,
            ));
        }
        if has_multiple_links(&metadata) || path_has_multiple_links(&cache_root, &relative_path)? {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::HardLink,
            ));
        }
        let absolute_path = absolute_path
            .canonicalize()
            .map_err(|_| AdapterError::InvalidArtifact(ArtifactValidationError::InvalidPath))?;
        if !absolute_path.starts_with(&cache_root) {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::InvalidPath,
            ));
        }
        Ok(Self {
            cache_root,
            absolute_path,
            relative_path: normalized_relative,
        })
    }

    /// Validates a batch of relative paths with one cache-root canonicalization.
    ///
    /// This performs blocking filesystem I/O and should run in
    /// [`tokio::task::spawn_blocking`] from async adapters.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::InvalidArtifact`] when any path is invalid.
    pub fn try_batch_from_cache_paths_blocking<I, P>(
        cache_root: &Path,
        relative_paths: I,
    ) -> Result<Vec<Self>, AdapterError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let cache_root = cache_root
            .canonicalize()
            .map_err(|_| AdapterError::InvalidArtifact(ArtifactValidationError::InvalidPath))?;
        let files = relative_paths
            .into_iter()
            .take(crate::MAX_MANIFEST_FILES + 1)
            .map(|relative| {
                let relative = relative.into();
                Self::from_canonical_root(&cache_root, &relative)
            })
            .collect::<Result<Vec<_>, _>>()?;
        if files.len() > crate::MAX_MANIFEST_FILES {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::LimitExceeded,
            ));
        }
        Ok(files)
    }

    fn from_canonical_root(
        cache_root: &Path,
        relative_path: &Path,
    ) -> Result<Self, AdapterError> {
        let relative_text = relative_path.to_str().ok_or(AdapterError::InvalidArtifact(
            ArtifactValidationError::InvalidPath,
        ))?;
        if relative_path.as_os_str().is_empty()
            || relative_path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            || cfg!(unix) && relative_text.contains('\\')
            || cfg!(windows) && relative_text.contains(':')
        {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::InvalidPath,
            ));
        }
        let normalized_relative = relative_path
            .components()
            .map(|component| match component {
                Component::Normal(value) => value.to_str().ok_or(AdapterError::InvalidArtifact(
                    ArtifactValidationError::InvalidPath,
                )),
                _ => Err(AdapterError::InvalidArtifact(
                    ArtifactValidationError::InvalidPath,
                )),
            })
            .collect::<Result<Vec<_>, _>>()?
            .join("/");
        reject_symlink_components(cache_root, relative_path)?;
        let absolute_path = cache_root.join(relative_path);
        let metadata = std::fs::symlink_metadata(&absolute_path)
            .map_err(|_| AdapterError::InvalidArtifact(ArtifactValidationError::MissingFile))?;
        if metadata.file_type().is_symlink() {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::Symlink,
            ));
        }
        if !metadata.is_file() {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::MissingFile,
            ));
        }
        if has_multiple_links(&metadata) || path_has_multiple_links(cache_root, relative_path)? {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::HardLink,
            ));
        }
        let absolute_path = absolute_path
            .canonicalize()
            .map_err(|_| AdapterError::InvalidArtifact(ArtifactValidationError::InvalidPath))?;
        if !absolute_path.starts_with(cache_root) {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::InvalidPath,
            ));
        }
        Ok(Self {
            cache_root: cache_root.to_path_buf(),
            absolute_path,
            relative_path: normalized_relative,
        })
    }

    #[must_use]
    pub fn absolute_path(&self) -> &Path {
        &self.absolute_path
    }

    #[must_use]
    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }
}

#[cfg(windows)]
fn path_has_multiple_links(
    cache_root: impl AsRef<Path>,
    relative_path: impl AsRef<Path>,
) -> Result<bool, AdapterError> {
    use cap_fs_ext::MetadataExt;
    let directory =
        cap_std::fs::Dir::open_ambient_dir(cache_root.as_ref(), cap_std::ambient_authority())
            .map_err(|_| AdapterError::RuntimeFailed)?;
    let metadata = directory
        .metadata(relative_path.as_ref())
        .map_err(|_| AdapterError::RuntimeFailed)?;
    Ok(metadata.nlink() > 1)
}

#[cfg(not(windows))]
#[allow(clippy::unnecessary_wraps)]
fn path_has_multiple_links(
    _cache_root: impl AsRef<Path>,
    _relative_path: impl AsRef<Path>,
) -> Result<bool, AdapterError> {
    Ok(false)
}

#[cfg(unix)]
fn has_multiple_links(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() > 1
}

#[cfg(windows)]
const fn has_multiple_links(_metadata: &std::fs::Metadata) -> bool {
    false
}

#[cfg(not(any(unix, windows)))]
const fn has_multiple_links(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn reject_symlink_components(
    cache_root: &Path,
    relative_path: &Path,
) -> Result<(), AdapterError> {
    let mut current = cache_root.to_path_buf();
    for component in relative_path.components() {
        let Component::Normal(component) = component else {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::InvalidPath,
            ));
        };
        current.push(component);
        let metadata = std::fs::symlink_metadata(&current)
            .map_err(|_| AdapterError::InvalidArtifact(ArtifactValidationError::MissingFile))?;
        if metadata.file_type().is_symlink() {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::Symlink,
            ));
        }
    }
    Ok(())
}

/// Named components returned when deconstructing an installed artifact.
#[derive(Debug)]
#[non_exhaustive]
pub struct InstalledArtifactParts {
    pub cache_root: PathBuf,
    pub model_id: ModelId,
    pub files: Vec<InstalledFile>,
    /// Whether cancellation may remove the whole operation-created artifact.
    pub created_by_install: bool,
}

/// Result of a completed adapter install.
///
/// `model_id` must match the descriptor passed to [`FoundryAdapter::install`].
/// File paths must be unique. The manager derives sizes and digests and rejects
/// inventories exceeding [`MAX_ARTIFACT_INVENTORY_BYTES`].
#[derive(Clone, Debug)]
pub struct InstalledArtifact {
    /// Canonicalizable root that owns every reported file.
    pub(crate) cache_root: PathBuf,
    /// Identity of the installed descriptor.
    pub(crate) model_id: ModelId,
    /// Complete, duplicate-free inventory of installed regular files.
    pub(crate) files: Vec<InstalledFile>,
    /// Whether this install operation created a previously absent cache entry.
    pub(crate) created_by_operation: bool,
    /// Keeps adapter-specific cache coordination alive through verification.
    pub(crate) operation_lease: Option<std::sync::Arc<dyn Send + Sync + fmt::Debug>>,
}

impl InstalledArtifact {
    /// Builds and validates an artifact from cache-root-relative paths without
    /// blocking the async executor.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::InvalidArtifact`] when path validation fails,
    /// or [`AdapterError::RuntimeFailed`] if the blocking task cannot finish.
    ///
    pub async fn try_from_cache_paths<I, P>(
        cache_root: PathBuf,
        model_id: ModelId,
        relative_paths: I,
    ) -> Result<Self, AdapterError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let relative_paths = relative_paths
            .into_iter()
            .take(crate::MAX_MANIFEST_FILES + 1)
            .map(Into::into)
            .collect::<Vec<_>>();
        if relative_paths.len() > crate::MAX_MANIFEST_FILES {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::LimitExceeded,
            ));
        }
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| AdapterError::RuntimeFailed)?;
        runtime
            .spawn_blocking(move || {
                let files = InstalledFile::try_batch_from_cache_paths_blocking(
                    &cache_root,
                    relative_paths,
                )?;
                Self::try_new(cache_root, model_id, files)
            })
            .await
            .map_err(|_| AdapterError::RuntimeFailed)?
    }

    /// Builds a newly created artifact and marks it for safe cancellation cleanup.
    ///
    /// # Errors
    ///
    /// Returns the same validation/runtime errors as
    /// [`Self::try_from_cache_paths`].
    pub async fn try_from_created_cache_paths<I, P>(
        cache_root: PathBuf,
        model_id: ModelId,
        relative_paths: I,
    ) -> Result<Self, AdapterError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        Self::try_from_cache_paths(cache_root, model_id, relative_paths)
            .await
            .map(Self::mark_created_by_install)
    }

    /// Creates an artifact after validating its inventory-level invariants.
    ///
    /// Build each path claim with [`InstalledFile::try_from_cache_path_blocking`]. The
    /// manager performs the sole authoritative streamed hashing pass.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::InvalidArtifact`] for an empty, oversized, or
    /// duplicate inventory, or when files belong to another cache root.
    pub fn try_new(
        cache_root: impl Into<PathBuf>,
        model_id: ModelId,
        files: Vec<InstalledFile>,
    ) -> Result<Self, AdapterError> {
        if files.is_empty() {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::EmptyInventory,
            ));
        }
        if files.len() > crate::MAX_MANIFEST_FILES {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::LimitExceeded,
            ));
        }
        let cache_root = cache_root
            .into()
            .canonicalize()
            .map_err(|_| AdapterError::InvalidArtifact(ArtifactValidationError::InvalidPath))?;
        if files.iter().any(|file| file.cache_root != cache_root) {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::InvalidPath,
            ));
        }
        let mut absolute_paths = std::collections::BTreeSet::new();
        let mut relative_paths = std::collections::BTreeSet::new();
        if files.iter().any(|file| {
            !absolute_paths.insert(file.absolute_path.as_path())
                || !relative_paths.insert(file.relative_path.as_str())
        }) {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::DuplicateEntry,
            ));
        }
        Ok(Self {
            cache_root,
            model_id,
            files,
            created_by_operation: false,
            operation_lease: None,
        })
    }

    /// Creates a newly downloaded artifact from prevalidated file claims.
    ///
    /// # Errors
    ///
    /// Returns the same inventory validation errors as [`Self::try_new`].
    pub fn try_new_created(
        cache_root: impl Into<PathBuf>,
        model_id: ModelId,
        files: Vec<InstalledFile>,
    ) -> Result<Self, AdapterError> {
        Self::try_new(cache_root, model_id, files).map(Self::mark_created_by_install)
    }

    #[must_use]
    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    #[must_use]
    pub const fn model_id(&self) -> &ModelId {
        &self.model_id
    }

    #[must_use]
    pub fn files(&self) -> &[InstalledFile] {
        &self.files
    }

    pub(crate) const fn created_by_operation(&self) -> bool {
        self.created_by_operation
    }

    pub(crate) fn release_operation_lease(&mut self) {
        self.operation_lease = None;
    }

    /// Reports whether cancellation may remove this operation-created artifact.
    #[must_use]
    pub const fn was_created_by_install(&self) -> bool {
        self.created_by_operation
    }

    #[must_use]
    const fn mark_created_by_install(mut self) -> Self {
        self.created_by_operation = true;
        self
    }

    #[must_use]
    pub fn into_parts(self) -> InstalledArtifactParts {
        InstalledArtifactParts {
            cache_root: self.cache_root,
            model_id: self.model_id,
            files: self.files,
            created_by_install: self.created_by_operation,
        }
    }
}

/// Largest caller-side chunk target accepted for one live session.
pub const MAX_ASR_CHUNK_MS: u64 = 60_000;
/// Largest SDK push queue accepted for one live session.
pub const MAX_ASR_PUSH_QUEUE_CAPACITY: usize = 4_096;

/// Bounded live-session configuration frozen at creation.
#[derive(Clone, Debug)]
pub struct AsrSessionSettings {
    /// Caller-side target duration and per-append upper bound.
    ///
    /// The manager does not rechunk input: callers must construct canonical
    /// chunks at this duration. A shorter final chunk is accepted, while a
    /// larger append returns [`AsrError::InvalidInput`]. Must be in
    /// `1..=MAX_ASR_CHUNK_MS`.
    pub chunk_ms: u64,
    /// Optional BCP-47 language hint.
    pub language: Option<String>,
    /// SDK push queue size. Must be in `1..=MAX_ASR_PUSH_QUEUE_CAPACITY`.
    pub push_queue_capacity: usize,
}

impl AsrSessionSettings {
    /// Validates settings before a model reference or runtime session is acquired.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::InvalidSettings`] for a chunk duration outside
    /// `1..=MAX_ASR_CHUNK_MS` or a push queue capacity outside
    /// `1..=MAX_ASR_PUSH_QUEUE_CAPACITY`.
    pub const fn validate(&self) -> Result<(), ModelError> {
        if self.chunk_ms == 0
            || self.chunk_ms > MAX_ASR_CHUNK_MS
            || self.push_queue_capacity == 0
            || self.push_queue_capacity > MAX_ASR_PUSH_QUEUE_CAPACITY
        {
            return Err(ModelError::InvalidSettings);
        }
        Ok(())
    }
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
    /// Manager-created sessions also release their runtime reference here and
    /// unload the model after the final concurrent session.
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

/// Redacted reason an adapter artifact failed structural validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ArtifactValidationError {
    InvalidPath,
    MissingFile,
    Symlink,
    HardLink,
    EmptyInventory,
    DuplicateEntry,
    LimitExceeded,
}

impl fmt::Display for ArtifactValidationError {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPath => "invalid cache-relative path",
            Self::MissingFile => "artifact file is missing",
            Self::Symlink => "artifact path contains a symbolic link",
            Self::HardLink => "artifact file has multiple hard links",
            Self::EmptyInventory => "artifact inventory is empty",
            Self::DuplicateEntry => "artifact inventory contains a duplicate",
            Self::LimitExceeded => "artifact inventory limit exceeded",
        })
    }
}

/// Adapter failures normalized into stable model errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum AdapterError {
    #[error("foundry runtime unavailable")]
    Unavailable,
    #[error("foundry catalog operation failed")]
    CatalogFailed,
    #[error("foundry download failed")]
    DownloadFailed,
    #[error("foundry runtime operation failed")]
    RuntimeFailed,
    #[error("foundry cache storage operation failed")]
    StorageFailed,
    #[error("model not found in foundry catalog/cache")]
    NotFound,
    #[error("adapter reported an invalid model artifact: {0}")]
    InvalidArtifact(ArtifactValidationError),
    #[error("ASR session settings are invalid")]
    InvalidSettings,
    #[error("safe force-redownload is unsupported by the runtime")]
    ForceRedownloadUnsupported,
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
    /// Every success, including an already-cached model, must return a
    /// complete non-empty inventory accepted by [`InstalledArtifact::try_new`].
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the download fails or a safe forced
    /// replacement is unsupported. Implementations must not remove a usable
    /// cache entry before a forced replacement succeeds.
    async fn install(
        &mut self,
        model: &ModelDescriptor,
        cancel: &tokio_util::sync::CancellationToken,
        force: bool,
    ) -> Result<InstalledArtifact, AdapterError>;

    /// Re-inspects an already cached artifact before runtime loading.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the local artifact cannot be inspected.
    async fn inspect_local_artifact(
        &mut self,
        model: &ModelDescriptor,
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

    /// Removes a specific validated cache directory when the adapter supports
    /// persisted artifact locations. `cache_directory` is slash-normalized,
    /// traversal-free, relative to the same cache root returned in the
    /// installation artifact, and identifies the model-owned directory rather
    /// than an individual file. `None` denotes a legacy manifest: implementations
    /// must locate the model safely or return an error, never silently succeed
    /// while artifacts remain. Defaults to [`Self::remove_from_cache`].
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the location is unsafe, cannot be found,
    /// or cannot be removed.
    async fn remove_artifact_from_cache(
        &mut self,
        model: &ModelDescriptor,
        _cache_directory: Option<&str>,
    ) -> Result<(), AdapterError> {
        self.remove_from_cache(model).await
    }

    /// Creates a live streaming ASR session for a loaded model.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when settings are invalid or the runtime is unavailable.
    async fn create_asr_session(
        &mut self,
        model: &ModelDescriptor,
        settings: &AsrSessionSettings,
    ) -> Result<Box<dyn StreamingAsrSession>, AdapterError>;

    /// Queries which scopes resolve locally without the online catalog.
    fn offline_scopes(&self) -> Vec<ModelScope>;

    /// Whether this runtime can atomically replace an already-cached model.
    ///
    /// The default is `false`, so adding this query does not require existing
    /// adapters to implement a new method. Callers should check it before
    /// exposing force-redownload as an available action.
    fn supports_cached_force_redownload(&self) -> bool {
        false
    }

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
        AdapterError::InvalidArtifact(reason) => ModelError::InvalidArtifact(match reason {
            ArtifactValidationError::InvalidPath => ModelArtifactFailure::InvalidPath,
            ArtifactValidationError::MissingFile => ModelArtifactFailure::MissingFile,
            ArtifactValidationError::Symlink => ModelArtifactFailure::Symlink,
            ArtifactValidationError::HardLink => ModelArtifactFailure::HardLink,
            ArtifactValidationError::EmptyInventory => ModelArtifactFailure::EmptyInventory,
            ArtifactValidationError::DuplicateEntry => ModelArtifactFailure::DuplicateEntry,
            ArtifactValidationError::LimitExceeded => ModelArtifactFailure::LimitExceeded,
        }),
        AdapterError::InvalidSettings => ModelError::InvalidSettings,
        AdapterError::ForceRedownloadUnsupported => ModelError::ForceRedownloadUnsupported,
        AdapterError::CatalogFailed
        | AdapterError::DownloadFailed
        | AdapterError::RuntimeFailed => ModelError::Internal,
        AdapterError::StorageFailed => ModelError::StoreFailed,
    }
}
