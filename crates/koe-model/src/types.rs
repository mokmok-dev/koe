//! Stable model domain types shared by the manager, CLI and MCP.

use std::{fmt, str::FromStr};

use koe_core::NetworkPolicy;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Catalog stable model identifier. Never user-controlled text.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ModelId(pub String);

impl ModelId {
    /// Wraps a catalog-supplied stable identifier.
    #[must_use]
    pub const fn new(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Display for ModelId {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Human-friendly catalog alias, e.g. `nemotron-3.5-asr-streaming-0.6b`.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Alias(pub String);

impl fmt::Display for Alias {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Catalog model version string.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ModelVersion(pub String);

impl ModelVersion {
    /// Wraps a catalog-supplied version string.
    #[must_use]
    pub const fn new(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Display for ModelVersion {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Which model population an operation addresses.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelScope {
    /// The online catalog. Requires an explicit network-enabled policy.
    Catalog,
    /// Models already present in the local cache.
    Installed,
    /// Models currently loaded into the local runtime.
    Loaded,
}

/// Selector accepted by resolve/install: either an alias or a stable id.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelSelector {
    Alias(String),
    Id(ModelId),
}

impl ModelSelector {
    /// Normalized selector text (lowercase) used for `Deref`-style lookups.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Alias(alias) => alias.to_ascii_lowercase(),
            Self::Id(id) => id.0.to_ascii_lowercase(),
        }
    }
}

impl FromStr for ModelSelector {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.trim();
        if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
            return Err(ModelError::InvalidSelector);
        }
        let selector = if trimmed.starts_with("id:")
            && let Some(suffix) = trimmed.strip_prefix("id:")
            && !suffix.is_empty()
        {
            Self::Id(ModelId::new(suffix.to_owned()))
        } else {
            Self::Alias(trimmed.to_owned())
        };
        Ok(selector)
    }
}

/// Catalog view of one model, including the resolved hardware variant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelDescriptor {
    pub id: ModelId,
    pub alias: Alias,
    pub version: ModelVersion,
    /// Resolved execution-provider variant: `cpu`, `cuda`, `qnn`, `webgpu`.
    pub variant: String,
    /// Publisher/provider name, e.g. `AzureFoundry`.
    pub provider: String,
    pub license_id: String,
    pub license_description: String,
    pub source: String,
    pub size_mb: u64,
    pub task: String,
}

/// One file of an installed model with its digest inventory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelFile {
    /// Relative path inside the model artifact, never absolute.
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

/// Whether publisher-verified digests could be checked.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verification {
    /// Every file matched the app-managed allowlist.
    Verified,
    /// No allowlist exists yet; the runtime is trusted as a boundary.
    RuntimeOnly,
    /// A digest mismatch or unknown file was found and quarantined.
    Quarantined,
}

/// Immutable per-model manifest recorded after a successful install.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ModelManifest {
    pub schema_version: u32,
    pub model_id: ModelId,
    pub alias: Alias,
    pub version: ModelVersion,
    pub variant: String,
    pub provider: String,
    pub license_id: String,
    pub license_description: String,
    pub source: String,
    /// Adapter-cache directory relative to its root, when discoverable.
    #[serde(default)]
    pub cache_directory: Option<String>,
    pub files: Vec<ModelFile>,
    pub installed_at_unix_ms: u128,
    pub foundry_version: String,
    pub verification: Verification,
}

impl ModelManifest {
    /// Creates a schema-v1 value for downstream mocks and tests.
    ///
    /// [`crate::ModelStore::publish_manifest`] does not accept this value; it
    /// independently constructs an authoritative persisted manifest from a
    /// descriptor, inventory, and verification status.
    #[must_use]
    pub fn external(
        descriptor: &ModelDescriptor,
        files: Vec<ModelFile>,
        verification: Verification,
    ) -> Self {
        Self {
            schema_version: 1,
            model_id: descriptor.id.clone(),
            alias: descriptor.alias.clone(),
            version: descriptor.version.clone(),
            variant: descriptor.variant.clone(),
            provider: descriptor.provider.clone(),
            license_id: descriptor.license_id.clone(),
            license_description: descriptor.license_description.clone(),
            source: descriptor.source.clone(),
            cache_directory: None,
            files,
            installed_at_unix_ms: 0,
            foundry_version: "external".to_owned(),
            verification,
        }
    }
}

/// Durable koe-owned identifier for one installed model copy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct InstalledModelId(Uuid);

impl InstalledModelId {
    /// Creates a new identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Parses the canonical UUID representation.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::InvalidId`] when `value` is not a UUID.
    pub fn parse(value: &str) -> Result<Self, ModelError> {
        Uuid::parse_str(value)
            .map(Self)
            .map_err(|_| ModelError::InvalidId)
    }

    /// Raw UUID for the store layout.
    #[must_use]
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for InstalledModelId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for InstalledModelId {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Handle for a model loaded into the runtime.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct LoadedModelId(Uuid);

impl LoadedModelId {
    /// Creates a new handle.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for LoadedModelId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for LoadedModelId {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Result of a completed install.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InstalledModel {
    pub id: InstalledModelId,
    pub descriptor: ModelDescriptor,
    pub manifest: ModelManifest,
}

/// Result of a successful load.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LoadedModel {
    pub id: LoadedModelId,
    pub installed: InstalledModelId,
    pub descriptor: ModelDescriptor,
}

/// Explicit per-operation network consent for model operations.
///
/// `Denied` refuses any catalog access or download and is the default for
/// recording sessions. `ModelInstallOnly` is set by an explicit user action
/// such as `koe models install --network`.
#[derive(Clone, Debug)]
pub struct InstallOptions {
    pub policy: NetworkPolicy,
    /// Cancellation token checked before and after each long adapter call.
    pub cancel: tokio_util::sync::CancellationToken,
    /// Optional lossy progress sink; `None` means no caller is observing.
    ///
    /// Events are best-effort and may be dropped when the bounded channel is
    /// full or closed. Callers must use the install result, not `Done`, as the
    /// authoritative terminal signal and should drain progress concurrently.
    pub progress: Option<tokio::sync::mpsc::Sender<ModelProgress>>,
    /// Catalog metadata observed by the caller before installation.
    ///
    /// This binds the install to an expected second resolution; it does not
    /// itself represent license acceptance or authorize network access.
    pub expected_descriptor: Option<ModelDescriptor>,
    /// Request a re-download even when the runtime cache has the model.
    ///
    /// Runtimes without an atomic force/replace primitive return
    /// [`ModelError::ForceRedownloadUnsupported`] and preserve the cache.
    pub force_redownload: bool,
}

/// Best-effort progress for one model operation.
///
/// Delivery is lossy; [`Self::Done`] is informational rather than guaranteed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelProgress {
    Resolving,
    Downloading,
    Verifying,
    Installing,
    Done,
}

/// Cause retained when a failed replacement invalidates the prior registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ReplacementFailure {
    Cancelled,
    Verification,
    Storage,
    Unavailable,
    NotFound,
    Internal,
}

/// Classified adapter artifact validation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ModelArtifactFailure {
    InvalidPath,
    MissingFile,
    Symlink,
    HardLink,
    EmptyInventory,
    DuplicateEntry,
    LimitExceeded,
}

impl fmt::Display for ModelArtifactFailure {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPath => "invalid artifact path",
            Self::MissingFile => "artifact file missing",
            Self::Symlink => "artifact path contains a symbolic link",
            Self::HardLink => "artifact file has multiple hard links",
            Self::EmptyInventory => "artifact inventory empty",
            Self::DuplicateEntry => "duplicate artifact entry",
            Self::LimitExceeded => "artifact inventory limit exceeded",
        })
    }
}

/// Classified reason model cache cleanup was incomplete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RemovalFailure {
    Unavailable,
    NotFound,
    Verification,
    Storage,
    Internal,
}

impl fmt::Display for RemovalFailure {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "runtime unavailable",
            Self::NotFound => "cache artifact not found",
            Self::Verification => "cache path validation failed",
            Self::Storage => "cache storage operation failed",
            Self::Internal => "internal cleanup failure",
        })
    }
}

/// Reason a caller-provided manifest failed structural validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ManifestValidationError {
    InvalidIdentity,
    EmptyInventory,
    TooManyFiles,
    InvalidPath,
    InvalidDigest,
    ArtifactSizeLimit,
    SerializedSizeLimit,
}

impl fmt::Display for ReplacementFailure {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        formatter.write_str(match self {
            Self::Cancelled => "operation cancelled",
            Self::Verification => "artifact verification failed",
            Self::Storage => "storage operation failed",
            Self::Unavailable => "runtime unavailable",
            Self::NotFound => "model not found",
            Self::Internal => "internal operation failed",
        })
    }
}

impl fmt::Display for ManifestValidationError {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidIdentity => "invalid identity field",
            Self::EmptyInventory => "empty artifact inventory",
            Self::TooManyFiles => "artifact file limit exceeded",
            Self::InvalidPath => "invalid artifact path",
            Self::InvalidDigest => "invalid artifact digest",
            Self::ArtifactSizeLimit => "artifact inventory size limit exceeded",
            Self::SerializedSizeLimit => "serialized manifest size limit exceeded",
        })
    }
}

/// Stable model lifecycle failures. Displays contain no paths or content.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum ModelError {
    #[error("the requested model artifact is not available offline")]
    OfflineArtifactMissing,
    /// Consent/policy failure with code `KOE-MODEL-NETWORK-DENIED`.
    ///
    /// This was separated from `KOE-MODEL-OFFLINE-MISSING` before the first
    /// stable release; see the changelog migration note.
    #[error("network access is required for this model operation but was not consented")]
    NetworkDenied,
    #[error("model artifact verification failed")]
    VerifyFailed,
    #[error("model artifact structure is invalid: {0}")]
    InvalidArtifact(ModelArtifactFailure),
    #[error("model manifest input is invalid: {0}")]
    InvalidManifest(ManifestValidationError),
    #[error("the expected model digest is invalid")]
    InvalidDigest,
    #[error("the model runtime is unavailable")]
    Unavailable,
    #[error("the model is in use and cannot be removed or switched")]
    Busy,
    #[error("the requested model was not found")]
    NotFound,
    #[error("another model install is already running")]
    Conflict,
    #[error("the model store is already open by another owner")]
    StoreLocked,
    #[error("multiple persisted registrations exist for one runtime model")]
    DuplicateRegistrations,
    #[error("the requested model capability is unsupported")]
    Unsupported,
    #[error("the installed model manifest is not corrupt")]
    NotCorrupt,
    #[error("the model operation was cancelled")]
    Cancelled,
    #[error("the resolved model license did not match the expected license ID")]
    LicenseMismatch,
    #[error("the resolved model descriptor changed after it was reported")]
    DescriptorChanged,
    #[error(
        "the ASR session settings are invalid; chunk_ms must be between 1 and 60000 and push_queue_capacity must be between 1 and 4096"
    )]
    InvalidSettings,
    #[error("safe force-redownload is not supported by this model runtime")]
    ForceRedownloadUnsupported,
    #[error("model registration {id} was removed but cache cleanup was incomplete: {cause}")]
    RemovalIncomplete {
        id: InstalledModelId,
        cause: RemovalFailure,
    },
    #[error("model replacement invalidated installation {id}: {cause}")]
    ReplacementInvalidated {
        id: InstalledModelId,
        cause: ReplacementFailure,
    },
    #[error("the model selector is invalid")]
    InvalidSelector,
    #[error("the model identifier is invalid")]
    InvalidId,
    #[error("the model store rejected a path")]
    PathRejected,
    #[error("installed model manifest {0} is corrupt")]
    CorruptManifest(InstalledModelId),
    #[error("the model store failed")]
    StoreFailed,
    #[error("invalid model transition")]
    InvalidTransition,
    #[error("internal model error")]
    Internal,
}

impl ModelError {
    /// Stable code for CLI, UI and MCP.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::OfflineArtifactMissing => "KOE-MODEL-OFFLINE-MISSING",
            Self::NetworkDenied => "KOE-MODEL-NETWORK-DENIED",
            Self::VerifyFailed => "KOE-MODEL-VERIFY-FAILED",
            Self::InvalidArtifact(_) => "KOE-MODEL-INVALID-ARTIFACT",
            Self::InvalidManifest(_) => "KOE-MODEL-INVALID-MANIFEST",
            Self::InvalidDigest => "KOE-MODEL-INVALID-DIGEST",
            Self::Unavailable => "KOE-MODEL-UNAVAILABLE",
            Self::Busy => "KOE-MODEL-BUSY",
            Self::NotFound => "KOE-MODEL-NOT-FOUND",
            Self::Conflict => "KOE-MODEL-CONFLICT",
            Self::StoreLocked => "KOE-MODEL-STORE-LOCKED",
            Self::DuplicateRegistrations => "KOE-MODEL-DUPLICATE-REGISTRATIONS",
            Self::Unsupported => "KOE-MODEL-UNSUPPORTED",
            Self::NotCorrupt => "KOE-MODEL-NOT-CORRUPT",
            Self::Cancelled => "KOE-MODEL-CANCELLED",
            Self::LicenseMismatch => "KOE-MODEL-LICENSE-MISMATCH",
            Self::DescriptorChanged => "KOE-MODEL-DESCRIPTOR-CHANGED",
            Self::InvalidSettings => "KOE-MODEL-ASR-INVALID-SETTINGS",
            Self::ForceRedownloadUnsupported => "KOE-MODEL-FORCE-REDOWNLOAD-UNSUPPORTED",
            Self::RemovalIncomplete { .. } => "KOE-MODEL-REMOVAL-INCOMPLETE",
            Self::ReplacementInvalidated { .. } => "KOE-MODEL-REPLACEMENT-INVALIDATED",
            Self::InvalidSelector | Self::InvalidId => "KOE-MODEL-INVALID-SELECTOR",
            Self::PathRejected => "KOE-STORE-PATH-REJECTED",
            Self::CorruptManifest(_) => "KOE-MODEL-MANIFEST-CORRUPT",
            Self::StoreFailed | Self::InvalidTransition | Self::Internal => {
                "KOE-MODEL-STORE-FAILED"
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ModelError, ModelSelector};

    #[test]
    fn selector_accepts_alias_and_explicit_id() {
        let alias = "nemotron-3.5-asr-streaming-0.6b"
            .parse::<ModelSelector>()
            .expect("alias");
        assert!(matches!(alias, ModelSelector::Alias(_)));
        let id = "id:AzureFoundryLocal/Model"
            .parse::<ModelSelector>()
            .expect("id");
        assert!(matches!(id, ModelSelector::Id(_)));
    }

    #[test]
    fn selector_rejects_empty_and_control_text() {
        assert_eq!(
            "".parse::<ModelSelector>(),
            Err(ModelError::InvalidSelector)
        );
        assert_eq!(
            "\u{1b}[31m".parse::<ModelSelector>(),
            Err(ModelError::InvalidSelector)
        );
    }

    #[test]
    fn offline_missing_has_stable_code() {
        assert_eq!(
            ModelError::OfflineArtifactMissing.code(),
            "KOE-MODEL-OFFLINE-MISSING"
        );
        assert_eq!(ModelError::VerifyFailed.code(), "KOE-MODEL-VERIFY-FAILED");
        assert_eq!(ModelError::Unavailable.code(), "KOE-MODEL-UNAVAILABLE");
        assert_eq!(
            ModelError::LicenseMismatch.code(),
            "KOE-MODEL-LICENSE-MISMATCH"
        );
        assert_eq!(
            ModelError::DescriptorChanged.code(),
            "KOE-MODEL-DESCRIPTOR-CHANGED"
        );
        assert_eq!(
            ModelError::InvalidSettings.code(),
            "KOE-MODEL-ASR-INVALID-SETTINGS"
        );
        assert_eq!(
            ModelError::ForceRedownloadUnsupported.code(),
            "KOE-MODEL-FORCE-REDOWNLOAD-UNSUPPORTED"
        );
    }
}
