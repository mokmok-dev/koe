//! Foundry Local adapter isolation, model lifecycle, cache store and offline ASR.
//!
//! Milestone 3 (`spec/08-roadmap.md`) keeps the foundry API, catalog and
//! hardware variants behind [`FoundryAdapter`]. Application code only sees
//! [`ModelManager`], [`StreamingAsrSession`] and the local model store.
//!
//! The offline contract is enforced at the manager boundary: `Denied` policy
//! never touches the adapter, and a missing local artifact becomes
//! [`ModelError::OfflineArtifactMissing`] instead of an implicit download.

mod adapter;
mod benchmark;
mod fixture;
#[cfg(feature = "foundry-local")]
mod foundry;
mod lifecycle;
mod manager;
mod store;
mod types;

pub use adapter::{
    AsrError, AsrEvent, AsrSessionSettings, FinalTranscript, FoundryAdapter, InstalledArtifact,
    InstalledFile, Pcm16Mono16k, StreamingAsrSession,
};
pub use benchmark::{BenchmarkBaseline, BenchmarkReport, word_error_rate};
pub use fixture::{FixtureAsrSession, FixtureFoundryAdapter, fixture_transcribe};
#[cfg(feature = "foundry-local")]
pub use foundry::FoundryLocalAdapter;
pub use lifecycle::{ModelLifecycle, ModelState};
pub use manager::{KoeModelManager, ModelManager};
pub use store::{AllowlistEntry, DigestAllowlist, ModelStore};
pub use types::{
    Alias, InstallOptions, InstalledModel, InstalledModelId, LoadedModel, LoadedModelId,
    ModelDescriptor, ModelError, ModelFile, ModelId, ModelManifest, ModelProgress, ModelScope,
    ModelSelector, ModelVersion, Verification,
};

/// Pinned SDK version recorded in every model manifest.
pub const FOUNDRY_SDK_VERSION: &str = "foundry-local-sdk-0.1.0";
