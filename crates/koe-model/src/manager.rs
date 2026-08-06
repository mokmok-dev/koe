//! Model manager: policy enforcement and install/load/unload/remove lifecycle.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use koe_core::NetworkPolicy;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};

use crate::{
    adapter::{
        AsrError, AsrEvent, AsrSessionSettings, FinalTranscript, FoundryAdapter, Pcm16Mono16k,
        StreamingAsrSession, map_adapter_error,
    },
    benchmark::{BenchmarkBaseline, BenchmarkReport, run_chunk_baseline},
    fixture::hex_encode,
    lifecycle::{ModelLifecycle, ModelState},
    store::{DigestAllowlist, ModelStore},
    types::{
        InstallOptions, InstalledModel, InstalledModelId, LoadedModel, LoadedModelId,
        ModelDescriptor, ModelError, ModelFile, ModelProgress, ModelScope, ModelSelector,
    },
};

/// Maximum artifact bytes hashed for the digest inventory.
const MAX_INVENTORY_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// The spec's async model manager port.
#[async_trait]
pub trait ModelManager: Send + Sync {
    /// Lists models by scope. `Catalog` requires an explicit network policy.
    ///
    /// # Errors
    ///
    /// Returns a model error for offline policy or adapter failure.
    async fn list(
        &self,
        scope: ModelScope,
    ) -> Result<Vec<ModelDescriptor>, ModelError>;
    /// Resolves a selector. Offline falls back to the local store and never
    /// touches the adapter.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::OfflineArtifactMissing`] offline without a local
    /// copy and adapter errors otherwise.
    async fn resolve(
        &self,
        selector: &ModelSelector,
    ) -> Result<ModelDescriptor, ModelError>;
    /// Installs a model after explicit network consent.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::NetworkDenied`] under a `Denied` policy,
    /// [`ModelError::Busy`] while another version is active, and
    /// [`ModelError::VerifyFailed`] on digest mismatch.
    async fn install(
        &self,
        selector: &ModelSelector,
        options: &InstallOptions,
    ) -> Result<InstalledModel, ModelError>;
    /// Loads an installed model into the local runtime.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::NotFound`] for unknown ids and adapter errors.
    async fn load(
        &self,
        installed: &InstalledModelId,
    ) -> Result<LoadedModel, ModelError>;
    /// Unloads a loaded model. Refused while an ASR session references it.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Busy`] when references remain.
    async fn unload(
        &self,
        loaded: &LoadedModelId,
    ) -> Result<(), ModelError>;
    /// Removes an installed model. Refused while loaded or referenced.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Busy`] when active and adapter/store errors.
    async fn remove(
        &self,
        installed: &InstalledModelId,
    ) -> Result<(), ModelError>;
    /// Creates a live ASR session for a loaded model.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::NotFound`] when the model is not loaded.
    async fn create_asr_session(
        &self,
        installed: &InstalledModelId,
        settings: &AsrSessionSettings,
    ) -> Result<Box<dyn StreamingAsrSession>, ModelError>;
}

struct LoadedRecord {
    installed: InstalledModelId,
    descriptor: ModelDescriptor,
    references: Arc<AtomicUsize>,
}

#[derive(Default)]
struct ManagerState {
    loaded: BTreeMap<LoadedModelId, LoadedRecord>,
    lifecycles: BTreeMap<InstalledModelId, ModelLifecycle>,
}

/// Concrete manager owning the store, adapter and per-model lifecycle.
pub struct KoeModelManager {
    store: ModelStore,
    adapter: Mutex<Box<dyn FoundryAdapter>>,
    state: RwLock<ManagerState>,
    install_gate: Mutex<()>,
    default_policy: NetworkPolicy,
}

impl KoeModelManager {
    /// Opens the model store and wraps an adapter.
    ///
    /// # Errors
    ///
    /// Returns a store error when `data_root` is rejected.
    pub fn new(
        data_root: impl Into<PathBuf>,
        allowlist: DigestAllowlist,
        adapter: Box<dyn FoundryAdapter>,
        default_policy: NetworkPolicy,
    ) -> Result<Self, ModelError> {
        let data_root = data_root.into();
        let store = ModelStore::open(&data_root, allowlist)?;
        Ok(Self {
            store,
            adapter: Mutex::new(adapter),
            state: RwLock::new(ManagerState::default()),
            install_gate: Mutex::new(()),
            default_policy,
        })
    }

    /// Runs a chunk-size benchmark and persists the baseline.
    ///
    /// # Errors
    ///
    /// Returns a model error when the model is not loaded or the session
    /// cannot run.
    pub async fn run_benchmark(
        &self,
        installed: &InstalledModelId,
        settings: &AsrSessionSettings,
        audio: &[i16],
        reference: &str,
    ) -> Result<BenchmarkBaseline, ModelError> {
        let manifest = self.store.load_manifest(installed)?;
        let session = self.create_asr_session(installed, settings).await?;
        let baseline = run_chunk_baseline(
            session,
            &manifest.model_id.0,
            &manifest.version.0,
            audio,
            settings.chunk_ms,
            reference,
        )
        .await
        .map_err(|_| ModelError::Unavailable)?;
        let mut report = self.store.load_benchmarks(installed)?;
        report.baselines.push(baseline.clone());
        self.store.save_benchmarks(installed, &report)?;
        Ok(baseline)
    }

    /// Returns the recorded benchmark report for one installed model.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::NotFound`] for unknown ids.
    pub fn benchmarks(
        &self,
        installed: &InstalledModelId,
    ) -> Result<BenchmarkReport, ModelError> {
        self.store.load_benchmarks(installed)
    }

    /// Lists installed models with their manifests for rich CLI display.
    ///
    /// # Errors
    ///
    /// Returns a store error when the manifests cannot be read.
    pub fn installed_models(&self) -> Result<Vec<InstalledModel>, ModelError> {
        Ok(self
            .store
            .installed_manifests()?
            .into_iter()
            .map(|(id, manifest)| InstalledModel {
                id,
                descriptor: descriptor_from_manifest(&manifest),
                manifest,
            })
            .collect())
    }

    /// Finds the installed id matching an alias or stable id, if any.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::StoreFailed`] for filesystem failures.
    pub fn installed_id_for(
        &self,
        selector: &ModelSelector,
    ) -> Result<Option<InstalledModelId>, ModelError> {
        self.store.find_installed(selector)
    }

    /// Loads one installed model with its manifest for display.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::NotFound`] for unknown ids.
    pub fn installed_model(
        &self,
        installed: &InstalledModelId,
    ) -> Result<InstalledModel, ModelError> {
        let manifest = self.store.load_manifest(installed)?;
        Ok(InstalledModel {
            id: *installed,
            descriptor: descriptor_from_manifest(&manifest),
            manifest,
        })
    }

    /// The default network policy frozen at construction.
    #[must_use]
    pub const fn policy(&self) -> NetworkPolicy {
        self.default_policy
    }

    /// Adapter backend label for capability reporting.
    ///
    /// # Errors
    ///
    /// Only fails if the adapter mutex is poisoned after a panic.
    pub async fn backend_name(&self) -> Result<&'static str, ModelError> {
        let backend = self.adapter.lock().await;
        Ok(backend.backend_name())
    }

    /// Counts outbound adapter attempts (diagnostic offline-enforcement hook).
    ///
    /// # Errors
    ///
    /// Only fails if the adapter mutex is poisoned after a panic.
    pub async fn adapter_outbound_attempts(&self) -> Result<usize, ModelError> {
        let backend = self.adapter.lock().await;
        Ok(backend.outbound_attempts())
    }

    #[allow(clippy::significant_drop_tightening)]
    async fn transition(
        &self,
        installed: &InstalledModelId,
        next: ModelState,
    ) -> Result<(), ModelError> {
        let mut state = self.state.write().await;
        let lifecycle = state
            .lifecycles
            .entry(*installed)
            .or_insert_with(ModelLifecycle::new);
        lifecycle.transition(next)
    }
}

#[async_trait]
#[allow(clippy::significant_drop_tightening)]
impl ModelManager for KoeModelManager {
    async fn list(
        &self,
        scope: ModelScope,
    ) -> Result<Vec<ModelDescriptor>, ModelError> {
        match scope {
            ModelScope::Catalog => {
                if self.default_policy == NetworkPolicy::Denied {
                    return Err(ModelError::NetworkDenied);
                }
                let mut adapter = self.adapter.lock().await;
                adapter.list_catalog().await.map_err(map_adapter_error)
            },
            ModelScope::Installed => self.store.list_descriptors(),
            ModelScope::Loaded => {
                let state = self.state.read().await;
                let mut descriptors = state
                    .loaded
                    .values()
                    .map(|record| record.descriptor.clone())
                    .collect::<Vec<_>>();
                descriptors.sort_by(|left, right| left.id.cmp(&right.id));
                Ok(descriptors)
            },
        }
    }

    async fn resolve(
        &self,
        selector: &ModelSelector,
    ) -> Result<ModelDescriptor, ModelError> {
        if let Some(id) = self.store.find_installed(selector)? {
            let manifest = self.store.load_manifest(&id)?;
            return Ok(descriptor_from_manifest(&manifest));
        }
        if self.default_policy == NetworkPolicy::Denied {
            return Err(ModelError::OfflineArtifactMissing);
        }
        let mut adapter = self.adapter.lock().await;
        adapter.resolve(selector).await.map_err(map_adapter_error)
    }

    async fn install(
        &self,
        selector: &ModelSelector,
        options: &InstallOptions,
    ) -> Result<InstalledModel, ModelError> {
        if options.policy == NetworkPolicy::Denied {
            return Err(ModelError::NetworkDenied);
        }
        let _gate = self.install_gate.lock().await;
        // Reject installing a different version while the catalog model is
        // loaded or referenced by an active session.
        let key = selector.key();
        {
            let state = self.state.read().await;
            for record in state.loaded.values() {
                if record.descriptor.id.0.to_ascii_lowercase() == key
                    || record.descriptor.alias.0.to_ascii_lowercase() == key
                {
                    return Err(ModelError::Busy);
                }
            }
        }

        let installed_id = InstalledModelId::new();
        self.transition(&installed_id, ModelState::Resolving)
            .await?;
        send_progress(options, ModelProgress::Resolving)?;
        let cancel = options.cancel.clone();
        let descriptor = {
            let mut adapter = self.adapter.lock().await;
            check_cancel(&cancel)?;
            adapter.resolve(selector).await.map_err(map_adapter_error)?
        };
        // Idempotent install: the identical version is already registered.
        let resolved_id = descriptor.id.0.clone();
        let resolved_version = descriptor.version.0.clone();
        if !options.force_redownload
            && let Some((id, manifest)) =
                self.store
                    .installed_manifests()?
                    .into_iter()
                    .find(|(_id, candidate)| {
                        candidate.model_id.0 == resolved_id
                            && candidate.version.0 == resolved_version
                    })
        {
            return Ok(InstalledModel {
                id,
                descriptor: descriptor_from_manifest(&manifest),
                manifest,
            });
        }
        self.transition(&installed_id, ModelState::Downloading)
            .await?;
        send_progress(options, ModelProgress::Downloading)?;
        check_cancel(&cancel)?;
        let artifact = {
            let mut adapter = self.adapter.lock().await;
            adapter
                .install(&descriptor, &cancel, options.force_redownload)
                .await
                .map_err(|error| {
                    if cancel.is_cancelled() {
                        ModelError::Cancelled
                    } else {
                        map_adapter_error(error)
                    }
                })?
        };
        check_cancel(&cancel)?;
        self.transition(&installed_id, ModelState::Verifying)
            .await?;
        send_progress(options, ModelProgress::Verifying)?;
        let files = inventory_from_artifact(&artifact)?;
        let verification = self.store.verify_inventory(&descriptor, &files)?;
        let id = self
            .store
            .publish_manifest(installed_id, &descriptor, files, verification)?;
        self.transition(&id, ModelState::Installed).await?;
        send_progress(options, ModelProgress::Done)?;
        let manifest = self.store.load_manifest(&id)?;
        Ok(InstalledModel {
            id,
            descriptor,
            manifest,
        })
    }

    async fn load(
        &self,
        installed: &InstalledModelId,
    ) -> Result<LoadedModel, ModelError> {
        let manifest = self.store.load_manifest(installed)?;
        let descriptor = descriptor_from_manifest(&manifest);
        let existing = {
            let state = self.state.read().await;
            state
                .loaded
                .iter()
                .find(|(_id, record)| record.installed == *installed)
                .map(|(id, record)| LoadedModel {
                    id: *id,
                    installed: *installed,
                    descriptor: record.descriptor.clone(),
                })
        };
        if let Some(loaded) = existing {
            return Ok(loaded);
        }
        self.transition(installed, ModelState::Loading).await?;
        {
            let mut adapter = self.adapter.lock().await;
            adapter.load(&descriptor).await.map_err(map_adapter_error)?;
        }
        let loaded_id = LoadedModelId::new();
        let mut state = self.state.write().await;
        state.loaded.insert(
            loaded_id,
            LoadedRecord {
                installed: *installed,
                descriptor: descriptor.clone(),
                references: Arc::new(AtomicUsize::new(0)),
            },
        );
        state
            .lifecycles
            .entry(*installed)
            .or_insert_with(ModelLifecycle::new)
            .transition(ModelState::Ready)
            .map_err(|_| ModelError::Internal)?;
        Ok(LoadedModel {
            id: loaded_id,
            installed: *installed,
            descriptor,
        })
    }

    async fn unload(
        &self,
        loaded: &LoadedModelId,
    ) -> Result<(), ModelError> {
        let (installed, descriptor) = {
            let state = self.state.read().await;
            let record = state.loaded.get(loaded).ok_or(ModelError::NotFound)?;
            if record.references.load(Ordering::Acquire) != 0 {
                return Err(ModelError::Busy);
            }
            (record.installed, record.descriptor.clone())
        };
        self.transition(&installed, ModelState::Unloading).await?;
        let result = {
            let mut adapter = self.adapter.lock().await;
            adapter.unload(&descriptor).await.map_err(map_adapter_error)
        };
        self.state.write().await.loaded.remove(loaded);
        result?;
        self.transition(&installed, ModelState::Installed).await?;
        Ok(())
    }

    async fn remove(
        &self,
        installed: &InstalledModelId,
    ) -> Result<(), ModelError> {
        let _gate = self.install_gate.lock().await;
        let manifest = self.store.load_manifest(installed)?;
        let descriptor = descriptor_from_manifest(&manifest);
        {
            let state = self.state.read().await;
            if state
                .loaded
                .values()
                .any(|record| record.installed == *installed)
            {
                return Err(ModelError::Busy);
            }
        }
        self.transition(installed, ModelState::Removing).await?;
        {
            let mut adapter = self.adapter.lock().await;
            adapter
                .remove_from_cache(&descriptor)
                .await
                .map_err(map_adapter_error)?;
        }
        self.store.remove_manifest(installed)?;
        self.transition(installed, ModelState::Absent).await?;
        Ok(())
    }

    async fn create_asr_session(
        &self,
        installed: &InstalledModelId,
        settings: &AsrSessionSettings,
    ) -> Result<Box<dyn StreamingAsrSession>, ModelError> {
        let (descriptor, references) = {
            let state = self.state.read().await;
            let record = state
                .loaded
                .values()
                .find(|record| record.installed == *installed)
                .ok_or(ModelError::NotFound)?;
            (record.descriptor.clone(), Arc::clone(&record.references))
        };
        self.transition(installed, ModelState::InUse).await?;
        let mut adapter = self.adapter.lock().await;
        let inner = adapter
            .create_asr_session(&descriptor, settings)
            .await
            .map_err(map_adapter_error)?;
        drop(adapter);
        references.fetch_add(1, Ordering::AcqRel);
        let release: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ignored = references.fetch_sub(1, Ordering::AcqRel);
        });
        Ok(Box::new(SessionGuard::new(inner, release)))
    }
}

/// Session wrapper that releases the model reference when finished/dropped.
struct SessionGuard {
    inner: Option<Box<dyn StreamingAsrSession>>,
    release: Arc<dyn Fn() + Send + Sync>,
}

impl SessionGuard {
    fn new(
        inner: Box<dyn StreamingAsrSession>,
        release: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            inner: Some(inner),
            release,
        }
    }
}

#[async_trait]
impl StreamingAsrSession for SessionGuard {
    async fn append(
        &mut self,
        chunk: Pcm16Mono16k,
    ) -> Result<(), AsrError> {
        self.inner
            .as_mut()
            .ok_or(AsrError::SessionNotActive)?
            .append(chunk)
            .await
    }

    async fn poll_results(&mut self) -> Result<Option<AsrEvent>, AsrError> {
        self.inner
            .as_mut()
            .ok_or(AsrError::SessionNotActive)?
            .poll_results()
            .await
    }

    async fn finish(mut self: Box<Self>) -> Result<FinalTranscript, AsrError> {
        let inner = self.inner.take().ok_or(AsrError::SessionNotActive)?;
        let result = inner.finish().await;
        (self.release)();
        result
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        (self.release)();
    }
}

/// Hashes artifact files into the manifest inventory, rejecting escape paths.
fn inventory_from_artifact(
    artifact: &crate::adapter::InstalledArtifact
) -> Result<Vec<ModelFile>, ModelError> {
    let mut files = Vec::new();
    for file in &artifact.files {
        if relative_path_escapes(&file.relative_path) {
            return Err(ModelError::PathRejected);
        }
        let size = std::fs::metadata(&file.absolute_path)
            .map_err(|_| ModelError::StoreFailed)?
            .len();
        if size > MAX_INVENTORY_BYTES {
            return Err(ModelError::StoreFailed);
        }
        let bytes = std::fs::read(&file.absolute_path).map_err(|_| ModelError::StoreFailed)?;
        files.push(ModelFile {
            path: file.relative_path.clone(),
            sha256: hex_encode(&Sha256::digest(&bytes)),
            size,
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn relative_path_escapes(relative: &str) -> bool {
    if relative.starts_with('/') || relative.starts_with('\\') {
        return true;
    }
    relative
        .split(['/', '\\'])
        .any(|component| matches!(component, ".." | ""))
}

fn check_cancel(cancel: &tokio_util::sync::CancellationToken) -> Result<(), ModelError> {
    if cancel.is_cancelled() {
        Err(ModelError::Cancelled)
    } else {
        Ok(())
    }
}

fn send_progress(
    options: &InstallOptions,
    phase: ModelProgress,
) -> Result<(), ModelError> {
    options.progress.as_ref().map_or(Ok(()), |tx| {
        tx.try_send(phase).map_err(|_| ModelError::Internal)
    })
}

fn descriptor_from_manifest(manifest: &crate::types::ModelManifest) -> ModelDescriptor {
    ModelDescriptor {
        id: manifest.model_id.clone(),
        alias: manifest.alias.clone(),
        version: manifest.version.clone(),
        variant: manifest.variant.clone(),
        provider: manifest.provider.clone(),
        license_id: manifest.license_id.clone(),
        license_description: manifest.license_description.clone(),
        source: manifest.source.clone(),
        size_mb: 0,
        task: "automatic-speech-recognition".to_owned(),
    }
}

#[cfg(test)]
mod tests;
