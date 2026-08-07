//! Signed update metadata types and stable update failures.
//!
//! The format is TUF-flavored: a signed payload carries a monotonically
//! increasing `version`, an `expiry`, a `platform` binding and hash-bound
//! targets. Verification never consults the network; fetches happen out of
//! band and the caller hands verified local files to [`crate::UpdateStore`].

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Payload schema accepted by this koe release.
pub const METADATA_SCHEMA: u32 = 1;

/// Maximum encoded signed metadata size accepted from disk (1 MiB).
pub const MAX_METADATA_BYTES: u64 = 1024 * 1024;
/// Maximum inventory entries in one platform release.
pub const MAX_TARGETS: usize = 256;
/// Maximum detached signatures retained in one document.
pub const MAX_SIGNATURES: usize = 8;
/// Maximum individual release artifact size (8 GiB).
pub const MAX_TARGET_SIZE: u64 = 8 * 1024 * 1024 * 1024;

/// Update metadata role implemented by this release.
pub const TARGETS_ROLE: &str = "targets";

/// One hash-bound release artifact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateTarget {
    /// Relative artifact path inside the release layout. Never absolute.
    pub path: String,
    /// Lowercase hex SHA-256 of the artifact bytes.
    pub sha256: String,
    /// Exact artifact size in bytes.
    pub size: u64,
}

/// Signed update payload for the `targets` role.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateMetadata {
    pub schema_version: u32,
    /// Must be `targets`. Root rotation is deliberately not advertised until
    /// threshold/rotation semantics are implemented.
    pub role: String,
    /// Monotonic metadata version; drives replay protection.
    pub version: u64,
    /// Unix seconds after which this metadata must not be accepted.
    pub expires_at_unix_s: u64,
    /// App version deployed by this update, used as the version directory
    /// name after sanitization.
    pub app_version: String,
    /// Canonical rustc target triple (e.g. `x86_64-apple-darwin`) that must
    /// match the running binary.
    pub platform: String,
    /// Relative path of the one executable target that may be activated.
    /// Other targets are inventory-only and can never become the application.
    pub install_target: String,
    /// Every release artifact covered by this metadata, hash-bound.
    pub targets: Vec<UpdateTarget>,
}

/// One detached signature over the canonical payload bytes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureEntry {
    /// Must equal `targets` and `UpdateMetadata::role`.
    pub role: String,
    /// Lowercase hex of the compressed 32-byte Ed25519 verifying key.
    pub key_id: String,
    /// Lowercase hex of the 64-byte Ed25519 signature.
    pub signature: String,
}

/// Verifier input: payload plus one or more detached signatures.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedUpdate {
    pub payload: UpdateMetadata,
    pub signatures: Vec<SignatureEntry>,
}

/// Durable update state under the app-owned data root.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateState {
    pub schema_version: u32,
    /// Sanitized app version currently active, if any.
    pub current: Option<String>,
    /// Previous app version retained for rollback, if any.
    pub previous: Option<String>,
    /// Highest verified metadata version (replay protection watermark).
    pub last_verified_version: u64,
    pub last_verified_at_unix_ms: u128,
    /// Last accepted signed document. Kept in the same authoritative atomic
    /// state record so status and replay state cannot diverge after a crash.
    pub last_verified_metadata: Option<SignedUpdate>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            current: None,
            previous: None,
            last_verified_version: 0,
            last_verified_at_unix_ms: 0,
            last_verified_metadata: None,
        }
    }
}

/// Machine-readable view of the update store.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UpdateStatus {
    pub current_version: Option<String>,
    pub previous_version: Option<String>,
    /// Every safely named, fully published version directory.
    pub installed_versions: Vec<String>,
    pub last_verified_update_version: u64,
    /// Metadata summary loaded from `metadata.json`, if present.
    pub last_verified_app_version: Option<String>,
    pub last_verified_expires_at_unix_s: Option<u64>,
    pub last_verified_platform: Option<String>,
    pub last_verified_at_unix_ms: Option<u128>,
}

/// Stable update failures. Displays contain no paths, keys or content.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum UpdateError {
    #[error("update metadata schema is not supported by this release")]
    UnsupportedSchema,
    #[error("update metadata signature is invalid")]
    SignatureInvalid,
    #[error("update metadata is malformed or has an unsupported role")]
    InvalidMetadata,
    #[error("update metadata has expired")]
    MetadataExpired,
    #[error("update metadata version is not newer than the last verified version")]
    Replay,
    #[error("update metadata targets a different platform")]
    PlatformMismatch,
    #[error("update artifact size does not match the signed metadata")]
    TargetSizeMismatch,
    #[error("update artifact digest does not match the signed metadata")]
    TargetDigestMismatch,
    #[error("no signed metadata matches the provided artifact")]
    TargetNotFound,
    #[error("installed update artifact is corrupt")]
    ArtifactCorrupt,
    #[error("no update metadata or state exists in the store")]
    Missing,
    #[error("no previous version is available for rollback")]
    NoPrevious,
    #[error("an update with this app version is already installed")]
    Conflict,
    #[error("the update public key is invalid")]
    InvalidKey,
    #[error("the update app version is not a safe directory name")]
    InvalidVersion,
    #[error("the requested update version was not found")]
    NotFound,
    #[error("the update store rejected a path")]
    PathRejected,
    #[error("the update store failed")]
    StoreFailed,
}

impl UpdateError {
    /// Stable code for CLI, UI and MCP.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::UnsupportedSchema => "KOE-UPDATE-UNSUPPORTED-SCHEMA",
            Self::SignatureInvalid => "KOE-UPDATE-SIGNATURE-INVALID",
            Self::InvalidMetadata => "KOE-UPDATE-METADATA-INVALID",
            Self::MetadataExpired => "KOE-UPDATE-EXPIRED",
            Self::Replay => "KOE-UPDATE-REPLAY",
            Self::PlatformMismatch => "KOE-UPDATE-PLATFORM-MISMATCH",
            Self::TargetSizeMismatch => "KOE-UPDATE-TARGET-SIZE-MISMATCH",
            Self::TargetDigestMismatch | Self::ArtifactCorrupt => {
                "KOE-UPDATE-TARGET-DIGEST-MISMATCH"
            },
            Self::TargetNotFound => "KOE-UPDATE-TARGET-NOT-FOUND",
            Self::Missing => "KOE-UPDATE-MISSING",
            Self::NoPrevious => "KOE-UPDATE-NO-PREVIOUS",
            Self::Conflict => "KOE-UPDATE-CONFLICT",
            Self::InvalidKey => "KOE-UPDATE-INVALID-KEY",
            Self::InvalidVersion => "KOE-UPDATE-INVALID-VERSION",
            Self::NotFound => "KOE-UPDATE-NOT-FOUND",
            Self::PathRejected => "KOE-STORE-PATH-REJECTED",
            Self::StoreFailed => "KOE-UPDATE-STORE-FAILED",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::UpdateError;

    #[test]
    fn stable_codes_cover_security_boundaries() {
        assert_eq!(
            UpdateError::SignatureInvalid.code(),
            "KOE-UPDATE-SIGNATURE-INVALID"
        );
        assert_eq!(UpdateError::MetadataExpired.code(), "KOE-UPDATE-EXPIRED");
        assert_eq!(UpdateError::Replay.code(), "KOE-UPDATE-REPLAY");
        assert_eq!(
            UpdateError::TargetDigestMismatch.code(),
            "KOE-UPDATE-TARGET-DIGEST-MISMATCH"
        );
        assert_eq!(
            UpdateError::PlatformMismatch.code(),
            "KOE-UPDATE-PLATFORM-MISMATCH"
        );
        assert_eq!(UpdateError::PathRejected.code(), "KOE-STORE-PATH-REJECTED");
    }
}
