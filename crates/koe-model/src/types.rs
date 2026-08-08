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
    pub files: Vec<ModelFile>,
    pub installed_at_unix_ms: u128,
    pub foundry_version: String,
    pub verification: Verification,
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
    /// Optional progress sink; `None` means no caller is observing.
    pub progress: Option<tokio::sync::mpsc::Sender<ModelProgress>>,
    /// Catalog metadata the caller displayed and explicitly accepted. When
    /// present, installation is refused if a second resolution differs.
    pub accepted_descriptor: Option<ModelDescriptor>,
    /// Re-download even when the runtime cache already has the model.
    pub force_redownload: bool,
}

/// Structured progress for one model operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelProgress {
    Resolving,
    Downloading,
    Verifying,
    Installing,
    Done,
}

/// Stable model lifecycle failures. Displays contain no paths or content.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ModelError {
    #[error("the requested model artifact is not available offline")]
    OfflineArtifactMissing,
    #[error("network access is required for this model operation but was not consented")]
    NetworkDenied,
    #[error("model artifact verification failed")]
    VerifyFailed,
    #[error("the model runtime is unavailable")]
    Unavailable,
    #[error("the model is in use and cannot be removed or switched")]
    Busy,
    #[error("the requested model was not found")]
    NotFound,
    #[error("another model install is already running")]
    Conflict,
    #[error("the model operation was cancelled")]
    Cancelled,
    #[error("the resolved model license was not explicitly accepted")]
    LicenseNotAccepted,
    #[error("the model selector is invalid")]
    InvalidSelector,
    #[error("the model identifier is invalid")]
    InvalidId,
    #[error("the model store rejected a path")]
    PathRejected,
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
            Self::OfflineArtifactMissing | Self::NetworkDenied => "KOE-MODEL-OFFLINE-MISSING",
            Self::VerifyFailed => "KOE-MODEL-VERIFY-FAILED",
            Self::Unavailable => "KOE-MODEL-UNAVAILABLE",
            Self::Busy => "KOE-MODEL-BUSY",
            Self::NotFound => "KOE-MODEL-NOT-FOUND",
            Self::Conflict => "KOE-MODEL-CONFLICT",
            Self::Cancelled => "KOE-MODEL-CANCELLED",
            Self::LicenseNotAccepted => "KOE-MODEL-LICENSE-NOT-ACCEPTED",
            Self::InvalidSelector | Self::InvalidId => "KOE-MODEL-INVALID-SELECTOR",
            Self::PathRejected => "KOE-STORE-PATH-REJECTED",
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
            ModelError::LicenseNotAccepted.code(),
            "KOE-MODEL-LICENSE-NOT-ACCEPTED"
        );
    }
}
