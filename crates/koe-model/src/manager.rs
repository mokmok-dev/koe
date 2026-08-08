//! Model manager: policy enforcement and install/load/unload/remove lifecycle.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufReader, Read},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[cfg(not(unix))]
use std::fs::OpenOptions;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use async_trait::async_trait;
use koe_core::NetworkPolicy;
use serde::{Deserialize, Serialize};
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
        RemovalFailure, ReplacementFailure,
    },
};

/// Per-entry installed-model diagnostics for fail-closed UIs.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[non_exhaustive]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
pub enum InstalledModelDiagnostic {
    Valid(Box<InstalledModel>),
    Corrupt(InstalledModelId),
}

/// The spec's async model manager port.
#[async_trait]
pub trait ModelManager: Send + Sync {
    /// Lists healthy and corrupt installed entries independently.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Unsupported`] by default or path/store errors from
    /// implementations that support diagnostics.
    async fn inspect_installed_models(&self) -> Result<Vec<InstalledModelDiagnostic>, ModelError> {
        Err(ModelError::Unsupported)
    }

    /// Lists models by scope. `Catalog` requires an explicit network policy.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::CorruptManifest`] with the repairable installation
    /// id, or a model error for offline policy/adapter failure. When supported,
    /// call [`Self::remove_corrupt_installation`] to repair corruption.
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
    /// copy, [`ModelError::CorruptManifest`] for a repairable local entry, and
    /// adapter errors otherwise.
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
    /// [`ModelError::VerifyFailed`] on digest mismatch. A failed replacement
    /// returns [`ModelError::ReplacementInvalidated`] with its cause because
    /// shared cache mutation requires invalidating the previous registration.
    ///
    /// # Panics
    ///
    /// Implementations may panic when polled outside an entered Tokio runtime.
    async fn install(
        &self,
        selector: &ModelSelector,
        options: &InstallOptions,
    ) -> Result<InstalledModel, ModelError>;
    /// Loads an installed model into the local runtime.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::NotFound`] for unknown ids,
    /// [`ModelError::Busy`] when the same runtime model is already loaded from
    /// another installation, and adapter errors.
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
    /// Removes a corrupt registration identified by
    /// [`ModelError::CorruptManifest`]. Adapter-owned cache artifacts remain
    /// and may be reused by a later install.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Unsupported`] when the capability is absent,
    /// [`ModelError::Busy`] when loaded, [`ModelError::NotCorrupt`] if the
    /// manifest is valid, and store/path errors when repair cannot be completed
    /// safely.
    async fn remove_corrupt_installation(
        &self,
        _installed: &InstalledModelId,
    ) -> Result<(), ModelError> {
        Err(ModelError::Unsupported)
    }
    /// Removes an installed model. Refused while loaded or referenced.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Busy`] when active. If cache cleanup fails, the
    /// registration stays removed and [`ModelError::RemovalIncomplete`]
    /// reports partial success; retrying this id then returns `NotFound`.
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
struct StagedRemoval {
    store: ModelStore,
    path: PathBuf,
    committed: bool,
}

impl StagedRemoval {
    const fn new(
        store: ModelStore,
        path: PathBuf,
    ) -> Self {
        Self {
            store,
            path,
            committed: false,
        }
    }

    fn commit(&mut self) -> Result<(), ModelError> {
        self.store.commit_removal(&self.path)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for StagedRemoval {
    fn drop(&mut self) {
        if !self.committed {
            let _ignored = self.store.commit_removal(&self.path);
        }
    }
}

struct LoadingLifecycleGuard {
    state: Arc<RwLock<ManagerState>>,
    adapter: Arc<Mutex<Box<dyn FoundryAdapter>>>,
    runtime_gate: Arc<Mutex<()>>,
    installed: InstalledModelId,
    descriptor: ModelDescriptor,
    committed: bool,
    runtime_call_started: bool,
}

impl LoadingLifecycleGuard {
    fn new(
        state: Arc<RwLock<ManagerState>>,
        adapter: Arc<Mutex<Box<dyn FoundryAdapter>>>,
        runtime_gate: Arc<Mutex<()>>,
        installed: InstalledModelId,
        descriptor: ModelDescriptor,
    ) -> Self {
        Self {
            state,
            adapter,
            runtime_gate,
            installed,
            descriptor,
            committed: false,
            runtime_call_started: false,
        }
    }

    const fn mark_runtime_call_started(&mut self) {
        self.runtime_call_started = true;
    }

    const fn clear_runtime_call(&mut self) {
        self.runtime_call_started = false;
    }

    const fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for LoadingLifecycleGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if self.runtime_call_started {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let state = Arc::clone(&self.state);
                let adapter = Arc::clone(&self.adapter);
                let runtime_gate = Arc::clone(&self.runtime_gate);
                let installed = self.installed;
                let descriptor = self.descriptor.clone();
                runtime.spawn(async move {
                    let _gate = runtime_gate.lock().await;
                    if adapter.lock().await.unload(&descriptor).await.is_ok() {
                        state
                            .write()
                            .await
                            .lifecycles
                            .insert(installed, ModelLifecycle::persisted_installed());
                    }
                });
            }
            return;
        }
        if let Ok(mut state) = self.state.try_write() {
            state
                .lifecycles
                .insert(self.installed, ModelLifecycle::persisted_installed());
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let state = Arc::clone(&self.state);
            let installed = self.installed;
            runtime.spawn(async move {
                state
                    .write()
                    .await
                    .lifecycles
                    .insert(installed, ModelLifecycle::persisted_installed());
            });
        }
    }
}

struct UnloadingGuard {
    state: Arc<RwLock<ManagerState>>,
    adapter: Arc<Mutex<Box<dyn FoundryAdapter>>>,
    runtime_gate: Arc<Mutex<()>>,
    loaded: LoadedModelId,
    installed: InstalledModelId,
    descriptor: ModelDescriptor,
    armed: bool,
    runtime_unloaded: bool,
}

impl UnloadingGuard {
    fn new(
        state: Arc<RwLock<ManagerState>>,
        adapter: Arc<Mutex<Box<dyn FoundryAdapter>>>,
        runtime_gate: Arc<Mutex<()>>,
        loaded: LoadedModelId,
        installed: InstalledModelId,
        descriptor: ModelDescriptor,
    ) -> Self {
        Self {
            state,
            adapter,
            runtime_gate,
            loaded,
            installed,
            descriptor,
            armed: true,
            runtime_unloaded: false,
        }
    }

    const fn mark_runtime_unloaded(&mut self) {
        self.runtime_unloaded = true;
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for UnloadingGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let state = Arc::clone(&self.state);
            let adapter = Arc::clone(&self.adapter);
            let runtime_gate = Arc::clone(&self.runtime_gate);
            let loaded = self.loaded;
            let installed = self.installed;
            let descriptor = self.descriptor.clone();
            let runtime_unloaded = self.runtime_unloaded;
            runtime.spawn(async move {
                let _gate = runtime_gate.lock().await;
                let runtime_unloaded = if runtime_unloaded {
                    true
                } else {
                    let mut adapter = adapter.lock().await;
                    if adapter.unload(&descriptor).await.is_ok() {
                        true
                    } else {
                        adapter.list_loaded().await.is_ok_and(|loaded| {
                            !loaded
                                .iter()
                                .any(|id| id.0.eq_ignore_ascii_case(&descriptor.id.0))
                        })
                    }
                };
                let mut state = state.write().await;
                if runtime_unloaded {
                    state.loaded.remove(&loaded);
                    state
                        .lifecycles
                        .insert(installed, ModelLifecycle::persisted_installed());
                } else {
                    state
                        .lifecycles
                        .insert(installed, ModelLifecycle::persisted_ready());
                }
            });
        }
    }
}

pub struct KoeModelManager {
    store: ModelStore,
    adapter: Arc<Mutex<Box<dyn FoundryAdapter>>>,
    state: Arc<RwLock<ManagerState>>,
    install_gate: Mutex<()>,
    runtime_gate: Arc<Mutex<()>>,
    default_policy: NetworkPolicy,
}

impl KoeModelManager {
    /// Opens the model store and wraps an adapter.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::StoreLocked`] while another independent manager
    /// owns `data_root`, or another store error when the root is rejected.
    pub fn new(
        data_root: impl Into<PathBuf>,
        allowlist: DigestAllowlist,
        adapter: Box<dyn FoundryAdapter>,
        default_policy: NetworkPolicy,
    ) -> Result<Self, ModelError> {
        let data_root = data_root.into();
        let store = ModelStore::open(&data_root, allowlist)?;
        Ok(Self::from_store(store, adapter, default_policy))
    }

    /// Wraps an already-open store. Kept private so lifecycle coordination
    /// cannot be split across independent managers for the same store.
    #[must_use]
    fn from_store(
        store: ModelStore,
        adapter: Box<dyn FoundryAdapter>,
        default_policy: NetworkPolicy,
    ) -> Self {
        Self {
            store,
            adapter: Arc::new(Mutex::new(adapter)),
            state: Arc::new(RwLock::new(ManagerState::default())),
            install_gate: Mutex::new(()),
            runtime_gate: Arc::new(Mutex::new(())),
            default_policy,
        }
    }

    async fn cleanup_created_artifact(
        &self,
        descriptor: &ModelDescriptor,
        created_by_operation: bool,
    ) -> Result<(), ModelError> {
        if !created_by_operation {
            return Ok(());
        }
        self.adapter
            .lock()
            .await
            .remove_from_cache(descriptor)
            .await
            .map_err(map_adapter_error)
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
    /// Returns [`ModelError::CorruptManifest`] for a repairable entry,
    /// [`ModelError::PathRejected`] for unsafe entries, or a store error when
    /// manifests cannot be read. Use [`Self::inspect_installed_models_sync`]
    /// for per-entry diagnostics while this manager owns the store lock.
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

    /// Synchronously lists healthy and corrupt installed entries independently.
    ///
    /// # Errors
    ///
    /// Returns path/store errors that cannot be attributed to one manifest.
    pub fn inspect_installed_models_sync(
        &self
    ) -> Result<Vec<InstalledModelDiagnostic>, ModelError> {
        inspect_installed_store(&self.store)
    }

    /// Finds the installed id matching an alias or stable id, if any.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::CorruptManifest`] for a repairable entry,
    /// [`ModelError::PathRejected`] for unsafe entries, or
    /// [`ModelError::StoreFailed`] for filesystem failures.
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
    /// Returns [`ModelError::NotFound`] for unknown ids or
    /// [`ModelError::CorruptManifest`] with the id accepted by
    /// [`Self::remove_corrupt_installation`].
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

    /// Removes a corrupt registration identified by
    /// [`ModelError::CorruptManifest`]. Adapter-owned cache artifacts remain
    /// and may be reused by a later install.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Busy`] when loaded,
    /// [`ModelError::NotCorrupt`] when the manifest is valid, and store/path
    /// errors when repair cannot be completed safely.
    pub async fn remove_corrupt_installation(
        &self,
        installed: &InstalledModelId,
    ) -> Result<(), ModelError> {
        let _runtime_gate = self.runtime_gate.lock().await;
        if self
            .state
            .read()
            .await
            .loaded
            .values()
            .any(|record| record.installed == *installed)
        {
            return Err(ModelError::Busy);
        }
        self.store.remove_corrupt_manifest(installed)
    }

    /// The default network policy frozen at construction.
    #[must_use]
    pub const fn policy(&self) -> NetworkPolicy {
        self.default_policy
    }

    /// Lists catalog metadata for an explicitly consented operation.
    ///
    /// # Errors
    ///
    /// Returns a policy, cancellation, or adapter error.
    pub async fn list_catalog_for(
        &self,
        policy: NetworkPolicy,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Vec<ModelDescriptor>, ModelError> {
        if policy != NetworkPolicy::ModelInstallOnly {
            return Err(ModelError::NetworkDenied);
        }
        check_cancel(cancel)?;
        let models = self
            .adapter
            .lock()
            .await
            .list_catalog()
            .await
            .map_err(map_adapter_error)?;
        check_cancel(cancel)?;
        Ok(models)
    }

    /// Resolves catalog metadata for an explicitly consented install.
    ///
    /// # Errors
    ///
    /// Returns a policy, cancellation, or adapter error.
    pub async fn resolve_for_install(
        &self,
        selector: &ModelSelector,
        policy: NetworkPolicy,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<ModelDescriptor, ModelError> {
        if policy != NetworkPolicy::ModelInstallOnly {
            return Err(ModelError::NetworkDenied);
        }
        check_cancel(cancel)?;
        let descriptor = self
            .adapter
            .lock()
            .await
            .resolve(selector)
            .await
            .map_err(map_adapter_error)?;
        check_cancel(cancel)?;
        Ok(descriptor)
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

    /// Reports whether the selected runtime can atomically replace an
    /// already-cached model for [`InstallOptions::force_redownload`].
    pub async fn supports_cached_force_redownload(&self) -> bool {
        self.adapter.lock().await.supports_cached_force_redownload()
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
}

#[async_trait]
#[allow(clippy::significant_drop_tightening, clippy::too_many_lines)]
impl ModelManager for KoeModelManager {
    #[allow(clippy::use_self)]
    async fn inspect_installed_models(&self) -> Result<Vec<InstalledModelDiagnostic>, ModelError> {
        self.inspect_installed_models_sync()
    }

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
        tokio::runtime::Handle::try_current().map_err(|_| ModelError::Internal)?;
        if options.policy != NetworkPolicy::ModelInstallOnly {
            return Err(ModelError::NetworkDenied);
        }
        let _gate = self.install_gate.lock().await;
        let installed_id = InstalledModelId::new();
        send_progress(options, ModelProgress::Resolving);
        let cancel = options.cancel.clone();
        let descriptor = {
            let mut adapter = self.adapter.lock().await;
            check_cancel(&cancel)?;
            adapter.resolve(selector).await.map_err(map_adapter_error)?
        };
        check_cancel(&cancel)?;
        if let Some(expected) = &options.expected_descriptor
            && expected != &descriptor
        {
            return Err(ModelError::DescriptorChanged);
        }
        let _runtime_gate = self.runtime_gate.lock().await;
        check_cancel(&cancel)?;
        {
            let state = self.state.read().await;
            if state.loaded.values().any(|record| {
                record
                    .descriptor
                    .id
                    .0
                    .eq_ignore_ascii_case(&descriptor.id.0)
            }) {
                check_cancel(&cancel)?;
                return Err(ModelError::Busy);
            }
        }
        let mut matching = self
            .store
            .installed_manifests()?
            .into_iter()
            .filter(|(_id, candidate)| candidate.model_id.0.eq_ignore_ascii_case(&descriptor.id.0))
            .collect::<Vec<_>>();
        if matching.len() > 1 {
            check_cancel(&cancel)?;
            return Err(ModelError::DuplicateRegistrations);
        }
        let existing = matching.pop();
        if !options.force_redownload
            && let Some((id, manifest)) = &existing
            && manifest.version == descriptor.version
        {
            check_cancel(&cancel)?;
            let id = *id;
            let persisted = manifest.clone();
            let artifact = {
                let mut adapter = self.adapter.lock().await;
                match adapter.inspect_local_artifact(&descriptor).await {
                    Ok(artifact) => artifact,
                    Err(
                        crate::adapter::AdapterError::NotFound
                        | crate::adapter::AdapterError::InvalidArtifact(_),
                    ) => {
                        self.store.remove_manifest(&id)?;
                        return Err(ModelError::ReplacementInvalidated {
                            id,
                            cause: ReplacementFailure::Verification,
                        });
                    },
                    Err(error) => return Err(map_adapter_error(error)),
                }
            };
            check_cancel(&cancel)?;
            let inventory_descriptor = descriptor.clone();
            let inventory_cancel = cancel.clone();
            let files = match tokio::runtime::Handle::try_current()
                .map_err(|_| ModelError::Internal)?
                .spawn_blocking(move || {
                    inventory_from_artifact(&artifact, &inventory_descriptor, &inventory_cancel)
                })
                .await
                .map_err(|_| ModelError::Internal)?
            {
                Ok(files) => files,
                Err(error) if is_artifact_invalidation(&error) => {
                    self.store.remove_manifest(&id)?;
                    return Err(ModelError::ReplacementInvalidated {
                        id,
                        cause: ReplacementFailure::Verification,
                    });
                },
                Err(error) => return Err(error),
            };
            check_cancel(&cancel)?;
            if !inventories_equivalent(&files, &persisted.files) {
                self.store.remove_manifest(&id)?;
                return Err(ModelError::ReplacementInvalidated {
                    id,
                    cause: ReplacementFailure::Verification,
                });
            }
            let relocated = !inventory_paths_equal(&files, &persisted.files);
            let verification = match self.store.verify_inventory(&descriptor, &files) {
                Ok(verification) => verification,
                Err(error) => {
                    self.store.remove_manifest(&id)?;
                    return Err(replacement_error(error, Some(id)));
                },
            };
            let manifest = if relocated || verification != persisted.verification {
                self.store
                    .update_manifest_inventory(&id, files, verification)
            } else {
                self.store.load_manifest(&id)
            }?;
            return Ok(InstalledModel {
                id,
                descriptor: descriptor_from_manifest(&manifest),
                manifest,
            });
        }
        send_progress(options, ModelProgress::Downloading);
        check_cancel(&cancel)?;
        let replacement_id = existing.as_ref().map(|(id, _manifest)| *id);
        if let Some(id) = replacement_id {
            // The adapter cache is shared by runtime model id. Invalidate stale
            // metadata before any replacement mutation can begin.
            self.store
                .remove_manifest(&id)
                .map_err(|error| replacement_error(error, replacement_id))?;
        }
        let mut artifact = {
            let mut adapter = self.adapter.lock().await;
            adapter
                .install(
                    &descriptor,
                    &cancel,
                    options.force_redownload || replacement_id.is_some(),
                )
                .await
                .map_err(|error| {
                    let error = if cancel.is_cancelled()
                        && matches!(error, crate::adapter::AdapterError::DownloadFailed)
                    {
                        ModelError::Cancelled
                    } else {
                        map_adapter_error(error)
                    };
                    replacement_error(error, replacement_id)
                })?
        };
        let created_by_operation = artifact.created_by_operation();
        if cancel.is_cancelled() {
            artifact.release_operation_lease();
            self.cleanup_created_artifact(&descriptor, created_by_operation)
                .await
                .map_err(|error| replacement_error(error, replacement_id))?;
            return Err(replacement_error(ModelError::Cancelled, replacement_id));
        }
        send_progress(options, ModelProgress::Verifying);
        let inventory_descriptor = descriptor.clone();
        let inventory_cancel = cancel.clone();
        let inventory_artifact = artifact.clone();
        let inventory_result = tokio::runtime::Handle::try_current()
            .map_err(|_| ModelError::Internal)?
            .spawn_blocking(move || {
                inventory_from_artifact(
                    &inventory_artifact,
                    &inventory_descriptor,
                    &inventory_cancel,
                )
            })
            .await
            .map_err(|_| ModelError::Internal)
            .and_then(|result| result);
        let files = match inventory_result {
            Ok(files) => files,
            Err(error) => {
                artifact.release_operation_lease();
                self.cleanup_created_artifact(&descriptor, created_by_operation)
                    .await
                    .map_err(|cleanup| replacement_error(cleanup, replacement_id))?;
                return Err(replacement_error(error, replacement_id));
            },
        };
        if cancel.is_cancelled() {
            artifact.release_operation_lease();
            self.cleanup_created_artifact(&descriptor, created_by_operation)
                .await
                .map_err(|error| replacement_error(error, replacement_id))?;
            return Err(replacement_error(ModelError::Cancelled, replacement_id));
        }
        let verification = match self.store.verify_inventory(&descriptor, &files) {
            Ok(verification) => verification,
            Err(error) => {
                artifact.release_operation_lease();
                self.cleanup_created_artifact(&descriptor, created_by_operation)
                    .await
                    .map_err(|cleanup| replacement_error(cleanup, replacement_id))?;
                return Err(replacement_error(error, replacement_id));
            },
        };
        if cancel.is_cancelled() {
            artifact.release_operation_lease();
            self.cleanup_created_artifact(&descriptor, created_by_operation)
                .await
                .map_err(|error| replacement_error(error, replacement_id))?;
            return Err(replacement_error(ModelError::Cancelled, replacement_id));
        }
        send_progress(options, ModelProgress::Installing);
        let target_id = replacement_id.unwrap_or(installed_id);
        let id = match self
            .store
            .publish_manifest(target_id, &descriptor, files, verification)
        {
            Ok(id) => id,
            Err(error) => {
                artifact.release_operation_lease();
                self.cleanup_created_artifact(&descriptor, created_by_operation)
                    .await
                    .map_err(|cleanup| replacement_error(cleanup, replacement_id))?;
                return Err(replacement_error(error, replacement_id));
            },
        };
        let manifest = match self.store.load_manifest(&id) {
            Ok(manifest) => manifest,
            Err(error) => {
                self.store
                    .remove_manifest(&id)
                    .map_err(|cleanup| replacement_error(cleanup, replacement_id))?;
                artifact.release_operation_lease();
                self.cleanup_created_artifact(&descriptor, created_by_operation)
                    .await
                    .map_err(|cleanup| replacement_error(cleanup, replacement_id))?;
                return Err(replacement_error(error, replacement_id));
            },
        };
        self.state
            .write()
            .await
            .lifecycles
            .insert(id, ModelLifecycle::persisted_installed());
        send_progress(options, ModelProgress::Done);
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
        let _runtime_gate = self.runtime_gate.lock().await;
        let manifest = self.store.load_manifest(installed)?;
        let descriptor = descriptor_from_manifest(&manifest);
        {
            let mut state = self.state.write().await;
            if let Some((id, record)) = state
                .loaded
                .iter()
                .find(|(_id, record)| record.installed == *installed)
            {
                return Ok(LoadedModel {
                    id: *id,
                    installed: *installed,
                    descriptor: record.descriptor.clone(),
                });
            }
            if state.loaded.values().any(|record| {
                record
                    .descriptor
                    .id
                    .0
                    .eq_ignore_ascii_case(&descriptor.id.0)
            }) {
                return Err(ModelError::Busy);
            }
            let lifecycle = state
                .lifecycles
                .entry(*installed)
                .or_insert_with(ModelLifecycle::persisted_installed);
            if lifecycle.state() == ModelState::Loading {
                return Err(ModelError::Busy);
            }
            if !lifecycle.state().allows(ModelState::Loading) {
                return Err(ModelError::InvalidTransition);
            }
            lifecycle.transition(ModelState::Loading)?;
        }
        let mut loading_guard = LoadingLifecycleGuard::new(
            Arc::clone(&self.state),
            Arc::clone(&self.adapter),
            Arc::clone(&self.runtime_gate),
            *installed,
            descriptor.clone(),
        );
        let artifact = {
            let mut adapter = self.adapter.lock().await;
            adapter
                .inspect_local_artifact(&descriptor)
                .await
                .map_err(map_adapter_error)?
        };
        let inventory_descriptor = descriptor.clone();
        let current = tokio::runtime::Handle::try_current()
            .map_err(|_| ModelError::Internal)?
            .spawn_blocking(move || {
                inventory_from_artifact(
                    &artifact,
                    &inventory_descriptor,
                    &tokio_util::sync::CancellationToken::new(),
                )
            })
            .await
            .map_err(|_| ModelError::Internal)??;
        if !inventory_paths_equal(&current, &manifest.files)
            || !inventories_equivalent(&current, &manifest.files)
        {
            return Err(ModelError::VerifyFailed);
        }
        let mut adapter = self.adapter.lock().await;
        let mut state = self.state.write().await;
        loading_guard.mark_runtime_call_started();
        if let Err(error) = adapter.load(&descriptor).await.map_err(map_adapter_error) {
            let unload_succeeded = adapter.unload(&descriptor).await.is_ok();
            let runtime_loaded = adapter.list_loaded().await.ok().map(|loaded| {
                loaded
                    .iter()
                    .any(|id| id.0.eq_ignore_ascii_case(&descriptor.id.0))
            });
            if unload_succeeded || runtime_loaded == Some(false) {
                loading_guard.clear_runtime_call();
                return Err(error);
            }
            if runtime_loaded.is_none() {
                return Err(error);
            }
            // The adapter reported an error but its authoritative runtime list
            // confirms the model is loaded, so publish that recovered state.
        }
        let loaded_id = LoadedModelId::new();
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
        loading_guard.commit();
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
        let _runtime_gate = self.runtime_gate.lock().await;
        let (installed, descriptor) = {
            let state = self.state.read().await;
            let record = state.loaded.get(loaded).ok_or(ModelError::NotFound)?;
            if record.references.load(Ordering::Acquire) != 0 {
                return Err(ModelError::Busy);
            }
            let lifecycle = state
                .lifecycles
                .get(&record.installed)
                .ok_or(ModelError::InvalidTransition)?;
            if !lifecycle.state().allows(ModelState::Unloading) {
                return Err(ModelError::InvalidTransition);
            }
            (record.installed, record.descriptor.clone())
        };
        self.state
            .write()
            .await
            .lifecycles
            .entry(installed)
            .or_insert_with(ModelLifecycle::new)
            .transition(ModelState::Unloading)?;
        let mut unloading_guard = UnloadingGuard::new(
            Arc::clone(&self.state),
            Arc::clone(&self.adapter),
            Arc::clone(&self.runtime_gate),
            *loaded,
            installed,
            descriptor.clone(),
        );
        let unload_error = {
            let mut adapter = self.adapter.lock().await;
            adapter
                .unload(&descriptor)
                .await
                .err()
                .map(map_adapter_error)
        };
        if let Some(error) = unload_error {
            let runtime_unloaded =
                self.adapter
                    .lock()
                    .await
                    .list_loaded()
                    .await
                    .is_ok_and(|loaded| {
                        !loaded
                            .iter()
                            .any(|id| id.0.eq_ignore_ascii_case(&descriptor.id.0))
                    });
            let mut state = self.state.write().await;
            if runtime_unloaded {
                state.loaded.remove(loaded);
                state
                    .lifecycles
                    .insert(installed, ModelLifecycle::persisted_installed());
            } else {
                state
                    .lifecycles
                    .insert(installed, ModelLifecycle::persisted_ready());
            }
            unloading_guard.disarm();
            return Err(error);
        }
        unloading_guard.mark_runtime_unloaded();
        let mut state = self.state.write().await;
        state.loaded.remove(loaded);
        state
            .lifecycles
            .entry(installed)
            .or_insert_with(ModelLifecycle::new)
            .transition(ModelState::Installed)?;
        unloading_guard.disarm();
        Ok(())
    }

    async fn remove_corrupt_installation(
        &self,
        installed: &InstalledModelId,
    ) -> Result<(), ModelError> {
        let _runtime_gate = self.runtime_gate.lock().await;
        if self
            .state
            .read()
            .await
            .loaded
            .values()
            .any(|record| record.installed == *installed)
        {
            return Err(ModelError::Busy);
        }
        self.store.remove_corrupt_manifest(installed)
    }

    async fn remove(
        &self,
        installed: &InstalledModelId,
    ) -> Result<(), ModelError> {
        let _install_gate = self.install_gate.lock().await;
        let _runtime_gate = self.runtime_gate.lock().await;
        let manifest = self.store.load_manifest(installed)?;
        let descriptor = descriptor_from_manifest(&manifest);
        {
            let state = self.state.read().await;
            if state.loaded.values().any(|record| {
                record.installed == *installed
                    || record
                        .descriptor
                        .id
                        .0
                        .eq_ignore_ascii_case(&descriptor.id.0)
            }) {
                return Err(ModelError::Busy);
            }
        }
        {
            let mut state = self.state.write().await;
            let lifecycle = state
                .lifecycles
                .entry(*installed)
                .or_insert_with(ModelLifecycle::persisted_installed);
            if !lifecycle.state().allows(ModelState::Removing) {
                return Err(ModelError::InvalidTransition);
            }
        }
        let shared_registration_remains =
            self.store
                .installed_manifests()?
                .into_iter()
                .any(|(id, manifest)| {
                    id != *installed && manifest.model_id.0.eq_ignore_ascii_case(&descriptor.id.0)
                });
        if shared_registration_remains {
            self.store.remove_manifest(installed)?;
        } else {
            let staged = self.store.stage_removal(installed)?;
            let mut staged = StagedRemoval::new(self.store.clone(), staged);
            let removal = {
                let mut adapter = self.adapter.lock().await;
                adapter
                    .remove_artifact_from_cache(&descriptor, manifest.cache_directory.as_deref())
                    .await
                    .map_err(map_adapter_error)
            };
            if let Err(error) = removal {
                staged.commit()?;
                return Err(ModelError::RemovalIncomplete {
                    id: *installed,
                    cause: removal_failure(&error),
                });
            }
            staged.commit()?;
        }
        let mut state = self.state.write().await;
        state.lifecycles.remove(installed);
        Ok(())
    }

    async fn create_asr_session(
        &self,
        installed: &InstalledModelId,
        settings: &AsrSessionSettings,
    ) -> Result<Box<dyn StreamingAsrSession>, ModelError> {
        let _runtime_gate = self.runtime_gate.lock().await;
        let (descriptor, references) = {
            let state = self.state.read().await;
            let record = state
                .loaded
                .values()
                .find(|record| record.installed == *installed)
                .ok_or(ModelError::NotFound)?;
            let lifecycle = state
                .lifecycles
                .get(installed)
                .ok_or(ModelError::InvalidTransition)?;
            if !lifecycle.state().allows(ModelState::InUse) {
                return Err(ModelError::Busy);
            }
            (record.descriptor.clone(), Arc::clone(&record.references))
        };
        let inner = {
            let mut adapter = self.adapter.lock().await;
            adapter
                .create_asr_session(&descriptor, settings)
                .await
                .map_err(map_adapter_error)?
        };
        let mut state = self.state.write().await;
        state
            .lifecycles
            .entry(*installed)
            .or_insert_with(ModelLifecycle::new)
            .transition(ModelState::InUse)?;
        references.fetch_add(1, Ordering::AcqRel);
        let release: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            references.fetch_sub(1, Ordering::AcqRel);
        });
        Ok(Box::new(SessionGuard::new(inner, release)))
    }
}

/// Session wrapper that releases the model reference when finished/dropped.
struct SessionGuard {
    inner: Option<Box<dyn StreamingAsrSession>>,
    release: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl SessionGuard {
    fn new(
        inner: Box<dyn StreamingAsrSession>,
        release: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            inner: Some(inner),
            release: Some(release),
        }
    }

    fn release(&mut self) {
        if let Some(release) = self.release.take() {
            release();
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
        self.release();
        result
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        drop(self.inner.take());
        self.release();
    }
}

/// Hashes artifact files into the manifest inventory, rejecting escape paths.
#[allow(clippy::too_many_lines)]
fn inventory_from_artifact(
    artifact: &crate::adapter::InstalledArtifact,
    descriptor: &ModelDescriptor,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<ModelFile>, ModelError> {
    check_cancel(cancel)?;
    if artifact.model_id != descriptor.id {
        return Err(ModelError::VerifyFailed);
    }
    if artifact.files.is_empty() || artifact.files.len() > crate::MAX_MANIFEST_FILES {
        return Err(ModelError::VerifyFailed);
    }
    let cache_root = artifact
        .cache_root
        .canonicalize()
        .map_err(|_| ModelError::StoreFailed)?;
    let mut total_size = 0_u64;
    let mut canonical_paths = BTreeSet::new();
    let mut files = Vec::with_capacity(artifact.files.len());
    for file in &artifact.files {
        check_cancel(cancel)?;
        if relative_path_escapes(file.relative_path()) {
            return Err(ModelError::PathRejected);
        }
        let metadata =
            std::fs::symlink_metadata(file.absolute_path()).map_err(|_| ModelError::StoreFailed)?;
        let initial_platform_identity = platform_path_identity(&cache_root, file.relative_path())?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || artifact_has_multiple_links(&metadata)
            || platform_identity_has_multiple_links(&initial_platform_identity)
        {
            return Err(ModelError::PathRejected);
        }
        let absolute_path = file
            .absolute_path()
            .canonicalize()
            .map_err(|_| ModelError::StoreFailed)?;
        let expected_path = cache_root
            .join(file.relative_path())
            .canonicalize()
            .map_err(|_| ModelError::StoreFailed)?;
        if absolute_path != expected_path
            || !absolute_path.starts_with(&cache_root)
            || !canonical_paths.insert(absolute_path.clone())
        {
            return Err(ModelError::PathRejected);
        }
        let size = metadata.len();
        total_size = total_size
            .checked_add(size)
            .ok_or(ModelError::StoreFailed)?;
        if total_size > crate::MAX_ARTIFACT_INVENTORY_BYTES {
            return Err(ModelError::VerifyFailed);
        }
        let opened = open_artifact_file(&cache_root, file.relative_path())?;
        let opened_metadata = opened.metadata().map_err(|_| ModelError::StoreFailed)?;
        let opened_platform_identity = platform_file_identity(&opened)?;
        if !opened_metadata.is_file()
            || opened_metadata.len() != size
            || artifact_has_multiple_links(&opened_metadata)
            || !same_file_identity(&metadata, &opened_metadata)
        {
            return Err(ModelError::VerifyFailed);
        }
        let read_limit = size.checked_add(1).ok_or(ModelError::StoreFailed)?;
        let mut reader = BufReader::new(opened).take(read_limit);
        let mut hasher = Sha256::new();
        let mut streamed = 0_u64;
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            check_cancel(cancel)?;
            let read = reader
                .read(&mut buffer)
                .map_err(|_| ModelError::StoreFailed)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            streamed = streamed
                .checked_add(u64::try_from(read).map_err(|_| ModelError::StoreFailed)?)
                .ok_or(ModelError::StoreFailed)?;
        }
        if streamed != size {
            return Err(ModelError::VerifyFailed);
        }
        let opened = reader.into_inner().into_inner();
        let final_opened_metadata = opened.metadata().map_err(|_| ModelError::StoreFailed)?;
        let final_path_metadata =
            std::fs::symlink_metadata(file.absolute_path()).map_err(|_| ModelError::StoreFailed)?;
        let final_platform_identity = platform_path_identity(&cache_root, file.relative_path())?;
        let final_path = file
            .absolute_path()
            .canonicalize()
            .map_err(|_| ModelError::StoreFailed)?;
        if final_path_metadata.file_type().is_symlink()
            || artifact_has_multiple_links(&final_path_metadata)
            || artifact_has_multiple_links(&final_opened_metadata)
            || !same_file_identity(&metadata, &final_path_metadata)
            || !same_file_identity(&opened_metadata, &final_opened_metadata)
            || !same_file_identity(&final_path_metadata, &final_opened_metadata)
            || platform_identity_has_multiple_links(&opened_platform_identity)
            || platform_identity_has_multiple_links(&final_platform_identity)
            || initial_platform_identity != opened_platform_identity
            || initial_platform_identity != final_platform_identity
            || final_path != absolute_path
        {
            return Err(ModelError::VerifyFailed);
        }
        let sha256 = hex_encode(&hasher.finalize());
        files.push(ModelFile {
            path: file.relative_path().to_owned(),
            sha256,
            size,
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    if files.windows(2).any(|pair| pair[0].path == pair[1].path) {
        return Err(ModelError::VerifyFailed);
    }
    Ok(files)
}

fn relative_path_escapes(relative: &str) -> bool {
    use std::path::Component;

    if relative.starts_with('/')
        || relative.starts_with('\\')
        || relative.as_bytes().get(1) == Some(&b':')
        || PathBuf::from(relative)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return true;
    }
    relative
        .split(['/', '\\'])
        .any(|component| matches!(component, "." | ".." | ""))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PlatformArtifactIdentity {
    device: u64,
    inode: u64,
    links: u64,
}

#[cfg(windows)]
fn platform_metadata_identity(metadata: &cap_std::fs::Metadata) -> PlatformArtifactIdentity {
    use cap_fs_ext::MetadataExt;
    PlatformArtifactIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        links: metadata.nlink(),
    }
}

#[cfg(windows)]
fn platform_path_identity(
    cache_root: &Path,
    relative_path: &str,
) -> Result<PlatformArtifactIdentity, ModelError> {
    let directory = cap_std::fs::Dir::open_ambient_dir(cache_root, cap_std::ambient_authority())
        .map_err(|_| ModelError::StoreFailed)?;
    let metadata = directory
        .metadata(relative_path)
        .map_err(|_| ModelError::StoreFailed)?;
    Ok(platform_metadata_identity(&metadata))
}

#[cfg(not(windows))]
#[allow(clippy::unnecessary_wraps)]
fn platform_path_identity(
    _cache_root: &Path,
    _relative_path: &str,
) -> Result<PlatformArtifactIdentity, ModelError> {
    Ok(PlatformArtifactIdentity::default())
}

#[cfg(windows)]
fn platform_file_identity(file: &File) -> Result<PlatformArtifactIdentity, ModelError> {
    let file = cap_std::fs::File::from_std(file.try_clone().map_err(|_| ModelError::StoreFailed)?);
    let metadata = file.metadata().map_err(|_| ModelError::StoreFailed)?;
    Ok(platform_metadata_identity(&metadata))
}

#[cfg(not(windows))]
#[allow(clippy::unnecessary_wraps)]
fn platform_file_identity(_file: &File) -> Result<PlatformArtifactIdentity, ModelError> {
    Ok(PlatformArtifactIdentity::default())
}

#[cfg(windows)]
const fn platform_identity_has_multiple_links(identity: &PlatformArtifactIdentity) -> bool {
    identity.links > 1
}

#[cfg(not(windows))]
const fn platform_identity_has_multiple_links(_identity: &PlatformArtifactIdentity) -> bool {
    false
}

#[cfg(unix)]
fn artifact_has_multiple_links(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() > 1
}

#[cfg(windows)]
const fn artifact_has_multiple_links(_metadata: &std::fs::Metadata) -> bool {
    false
}

#[cfg(not(any(unix, windows)))]
const fn artifact_has_multiple_links(_metadata: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn same_file_identity(
    left: &std::fs::Metadata,
    right: &std::fs::Metadata,
) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
const fn same_file_identity(
    _left: &std::fs::Metadata,
    _right: &std::fs::Metadata,
) -> bool {
    true
}

#[cfg(not(any(unix, windows)))]
const fn same_file_identity(
    _left: &std::fs::Metadata,
    _right: &std::fs::Metadata,
) -> bool {
    true
}

#[cfg(unix)]
fn open_artifact_file(
    cache_root: &Path,
    relative_path: &str,
) -> Result<File, ModelError> {
    use rustix::fs::{Mode, OFlags, open, openat};

    let mut directory = open(
        cache_root,
        OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY,
        Mode::empty(),
    )
    .map_err(|_| ModelError::PathRejected)?;
    let mut components = relative_path.split('/').peekable();
    while let Some(component) = components.next() {
        let final_component = components.peek().is_none();
        let flags = if final_component {
            OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::RDONLY
        } else {
            OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY
        };
        let opened = openat(&directory, component, flags, Mode::empty())
            .map_err(|_| ModelError::PathRejected)?;
        if final_component {
            return Ok(File::from(opened));
        }
        directory = opened;
    }
    Err(ModelError::PathRejected)
}

#[cfg(not(unix))]
fn open_artifact_file(
    cache_root: &Path,
    relative_path: &str,
) -> Result<File, ModelError> {
    let mut options = OpenOptions::new();
    options.read(true);
    set_no_follow(&mut options);
    options
        .open(cache_root.join(relative_path))
        .map_err(|_| ModelError::PathRejected)
}

#[cfg(not(unix))]
fn set_no_follow(options: &mut OpenOptions) {
    #[cfg(windows)]
    options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
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
) {
    if let Some(tx) = &options.progress {
        let _ignored = tx.try_send(phase);
    }
}

fn inspect_installed_store(
    store: &ModelStore
) -> Result<Vec<InstalledModelDiagnostic>, ModelError> {
    store
        .inspect_installed_manifests()?
        .into_iter()
        .map(|entry| match entry {
            crate::store::InstalledManifestEntry::Valid { id, manifest } => {
                let manifest = *manifest;
                Ok(InstalledModelDiagnostic::Valid(Box::new(InstalledModel {
                    id,
                    descriptor: descriptor_from_manifest(&manifest),
                    manifest,
                })))
            },
            crate::store::InstalledManifestEntry::Corrupt { id } => {
                Ok(InstalledModelDiagnostic::Corrupt(id))
            },
        })
        .collect()
}

const fn is_artifact_invalidation(error: &ModelError) -> bool {
    matches!(
        error,
        ModelError::VerifyFailed
            | ModelError::InvalidArtifact(_)
            | ModelError::InvalidDigest
            | ModelError::PathRejected
    )
}

const fn removal_failure(error: &ModelError) -> RemovalFailure {
    match error {
        ModelError::Unavailable => RemovalFailure::Unavailable,
        ModelError::NotFound => RemovalFailure::NotFound,
        ModelError::PathRejected | ModelError::VerifyFailed | ModelError::InvalidArtifact(_) => {
            RemovalFailure::Verification
        },
        ModelError::StoreFailed => RemovalFailure::Storage,
        _ => RemovalFailure::Internal,
    }
}

const fn replacement_error(
    error: ModelError,
    replacement_id: Option<InstalledModelId>,
) -> ModelError {
    let Some(id) = replacement_id else {
        return error;
    };
    let cause = match error {
        ModelError::Cancelled => ReplacementFailure::Cancelled,
        ModelError::VerifyFailed
        | ModelError::InvalidArtifact(_)
        | ModelError::InvalidDigest
        | ModelError::InvalidManifest(_) => ReplacementFailure::Verification,
        ModelError::StoreFailed | ModelError::PathRejected | ModelError::CorruptManifest(_) => {
            ReplacementFailure::Storage
        },
        ModelError::Unavailable => ReplacementFailure::Unavailable,
        ModelError::NotFound => ReplacementFailure::NotFound,
        _ => ReplacementFailure::Internal,
    };
    ModelError::ReplacementInvalidated { id, cause }
}

fn inventory_paths_equal(
    current: &[ModelFile],
    persisted: &[ModelFile],
) -> bool {
    let current = current
        .iter()
        .map(|file| &file.path)
        .collect::<BTreeSet<_>>();
    let persisted = persisted
        .iter()
        .map(|file| &file.path)
        .collect::<BTreeSet<_>>();
    current == persisted
}

fn inventories_equivalent(
    current: &[ModelFile],
    persisted: &[ModelFile],
) -> bool {
    if current.len() != persisted.len() {
        return false;
    }
    let mut current = current.iter().collect::<Vec<_>>();
    let mut persisted = persisted.iter().collect::<Vec<_>>();
    current.sort_by(|left, right| left.path.cmp(&right.path));
    persisted.sort_by(|left, right| left.path.cmp(&right.path));
    current.iter().zip(persisted).all(|(current, persisted)| {
        current.sha256 == persisted.sha256
            && current.size == persisted.size
            && (current.path == persisted.path
                || current
                    .path
                    .strip_suffix(&persisted.path)
                    .is_some_and(|prefix| prefix.ends_with('/')))
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
