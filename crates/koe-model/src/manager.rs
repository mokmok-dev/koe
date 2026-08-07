//! Model manager: policy enforcement and install/load/unload/remove lifecycle.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
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

/// Maximum bytes hashed for one file and for an entire artifact inventory.
const MAX_INVENTORY_FILE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_INVENTORY_TOTAL_BYTES: u64 = 16 * 1024 * 1024 * 1024;

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
        let lifecycles = store
            .installed_manifests()?
            .into_iter()
            .map(|(id, _manifest)| (id, ModelLifecycle::persisted_installed()))
            .collect();
        Ok(Self {
            store,
            adapter: Mutex::new(adapter),
            state: RwLock::new(ManagerState {
                loaded: BTreeMap::new(),
                lifecycles,
            }),
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

    /// Resolves metadata for a specifically consented install operation.
    /// This is the only catalog access allowed when the manager's frozen
    /// session policy is [`NetworkPolicy::Denied`].
    ///
    /// # Errors
    ///
    /// Returns a policy, cancellation, or normalized adapter error.
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
        if options.policy != NetworkPolicy::ModelInstallOnly {
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
        send_progress(options, ModelProgress::Resolving);
        let cancel = options.cancel.clone();
        let descriptor = {
            let mut adapter = self.adapter.lock().await;
            check_cancel(&cancel)?;
            adapter.resolve(selector).await.map_err(map_adapter_error)?
        };
        if let Some(accepted) = &options.accepted_descriptor
            && accepted != &descriptor
        {
            return Err(ModelError::LicenseNotAccepted);
        }
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
        send_progress(options, ModelProgress::Downloading);
        check_cancel(&cancel)?;
        let artifact = {
            let mut adapter = self.adapter.lock().await;
            match adapter
                .install(&descriptor, &cancel, options.force_redownload)
                .await
            {
                Ok(artifact) if cancel.is_cancelled() => {
                    // Some SDK downloads are not cooperatively cancellable.
                    // Wait for them to return, then remove only a cache entry
                    // this operation proved it created. Pre-existing shared
                    // cache content is never cancellation cleanup.
                    if artifact.created_by_install {
                        let _ignored = adapter.remove_from_cache(&descriptor).await;
                    }
                    return Err(ModelError::Cancelled);
                },
                Ok(artifact) => artifact,
                Err(_error) if cancel.is_cancelled() => {
                    // The adapter owns any unpublished staging left by a
                    // failed install. Without an ownership-bearing artifact,
                    // deleting a shared cache entry would be unsafe.
                    return Err(ModelError::Cancelled);
                },
                Err(error) => return Err(map_adapter_error(error)),
            }
        };
        self.transition(&installed_id, ModelState::Verifying)
            .await?;
        send_progress(options, ModelProgress::Verifying);
        let files = inventory_from_artifact(&artifact, &descriptor, &cancel)?;
        check_cancel(&cancel)?;
        let verification = self.store.verify_inventory(&descriptor, &files)?;
        check_cancel(&cancel)?;
        send_progress(options, ModelProgress::Installing);
        // Publication is the completion boundary: cancellation observed
        // before this point wins; once the immutable manifest is published,
        // the operation is reported as completed.
        check_cancel(&cancel)?;
        let id = self
            .store
            .publish_manifest(installed_id, &descriptor, files, verification)?;
        self.transition(&id, ModelState::Installed).await?;
        send_progress(options, ModelProgress::Done);
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
        let load_result = {
            let mut adapter = self.adapter.lock().await;
            let result = async {
                let artifact = adapter
                    .inspect_local_artifact(&descriptor)
                    .await
                    .map_err(map_adapter_error)?;
                let current = inventory_from_artifact(
                    &artifact,
                    &descriptor,
                    &tokio_util::sync::CancellationToken::new(),
                )?;
                if current != manifest.files {
                    return Err(ModelError::VerifyFailed);
                }
                adapter.load(&descriptor).await.map_err(map_adapter_error)
            }
            .await;
            if result.is_err() {
                // A runtime can fail after partially loading. Best-effort
                // unload keeps the retry state aligned with the manager.
                let _ignored = adapter.unload(&descriptor).await;
            }
            result
        };
        if let Err(error) = load_result {
            self.transition(installed, ModelState::Installed).await?;
            return Err(error);
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
        // `self` drops exactly once after finalization and releases the model
        // reference through `Drop`; releasing here as well would underflow the
        // reference count and make the model permanently busy.
        inner.finish().await
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        (self.release)();
    }
}

/// Hashes artifact files into the manifest inventory, rejecting escape paths.
fn inventory_from_artifact(
    artifact: &crate::adapter::InstalledArtifact,
    descriptor: &ModelDescriptor,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<ModelFile>, ModelError> {
    inventory_from_artifact_with_limits(
        artifact,
        descriptor,
        cancel,
        MAX_INVENTORY_FILE_BYTES,
        MAX_INVENTORY_TOTAL_BYTES,
    )
}

fn inventory_from_artifact_with_limits(
    artifact: &crate::adapter::InstalledArtifact,
    descriptor: &ModelDescriptor,
    cancel: &tokio_util::sync::CancellationToken,
    max_file_bytes: u64,
    max_total_bytes: u64,
) -> Result<Vec<ModelFile>, ModelError> {
    if artifact.model_id != descriptor.id {
        return Err(ModelError::VerifyFailed);
    }
    let (root, artifact_root) = canonical_artifact_roots(artifact)?;
    if artifact.files.is_empty() {
        return Err(ModelError::VerifyFailed);
    }
    let mut files = Vec::new();
    let mut paths = BTreeSet::new();
    let mut total_size = 0_u64;
    for reported in &artifact.files {
        check_cancel(cancel)?;
        if relative_path_escapes(&reported.relative_path)
            || !paths.insert(reported.relative_path.replace('\\', "/"))
        {
            return Err(ModelError::PathRejected);
        }
        let link_metadata = std::fs::symlink_metadata(&reported.absolute_path)
            .map_err(|_| ModelError::StoreFailed)?;
        if link_metadata.file_type().is_symlink() || !link_metadata.is_file() {
            return Err(ModelError::PathRejected);
        }
        let canonical = reported
            .absolute_path
            .canonicalize()
            .map_err(|_| ModelError::StoreFailed)?;
        if !canonical.starts_with(&artifact_root) {
            return Err(ModelError::PathRejected);
        }
        let relative = canonical
            .strip_prefix(&root)
            .map_err(|_| ModelError::PathRejected)?;
        if normalized_relative(relative) != reported.relative_path.replace('\\', "/") {
            return Err(ModelError::PathRejected);
        }
        let expected_metadata =
            std::fs::metadata(&canonical).map_err(|_| ModelError::StoreFailed)?;
        let mut input = File::open(&canonical).map_err(|_| ModelError::StoreFailed)?;
        let metadata = input.metadata().map_err(|_| ModelError::StoreFailed)?;
        let reopened_path = reported
            .absolute_path
            .canonicalize()
            .map_err(|_| ModelError::StoreFailed)?;
        if reopened_path != canonical
            || !same_file(&canonical, &input, &expected_metadata, &metadata)
        {
            return Err(ModelError::PathRejected);
        }
        let size = metadata.len();
        if !safe_regular_file(&input, &metadata) {
            return Err(ModelError::PathRejected);
        }
        if size > max_file_bytes {
            return Err(ModelError::StoreFailed);
        }
        total_size = total_size
            .checked_add(size)
            .filter(|total| *total <= max_total_bytes)
            .ok_or(ModelError::StoreFailed)?;
        let mut digest = Sha256::new();
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut hashed = 0_u64;
        loop {
            check_cancel(cancel)?;
            let count = input
                .read(&mut buffer)
                .map_err(|_| ModelError::StoreFailed)?;
            if count == 0 {
                break;
            }
            hashed = hashed
                .checked_add(u64::try_from(count).map_err(|_| ModelError::StoreFailed)?)
                .filter(|value| *value <= size)
                .ok_or(ModelError::StoreFailed)?;
            digest.update(&buffer[..count]);
        }
        if hashed != size {
            return Err(ModelError::StoreFailed);
        }
        check_cancel(cancel)?;
        let after_metadata = input.metadata().map_err(|_| ModelError::StoreFailed)?;
        let after_path = reported
            .absolute_path
            .canonicalize()
            .map_err(|_| ModelError::StoreFailed)?;
        if after_path != canonical
            || !same_file(&canonical, &input, &metadata, &after_metadata)
            || after_metadata.len() != size
        {
            return Err(ModelError::PathRejected);
        }
        files.push(ModelFile {
            path: reported.relative_path.clone(),
            sha256: hex_encode(&digest.finalize()),
            size,
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn canonical_artifact_roots(
    artifact: &crate::adapter::InstalledArtifact
) -> Result<(PathBuf, PathBuf), ModelError> {
    let root = canonical_directory(&artifact.cache_root)?;
    let artifact_root = canonical_directory(&artifact.artifact_root)?;
    if artifact_root == root || !artifact_root.starts_with(&root) {
        return Err(ModelError::PathRejected);
    }
    Ok((root, artifact_root))
}

fn canonical_directory(path: &Path) -> Result<PathBuf, ModelError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| ModelError::StoreFailed)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ModelError::PathRejected);
    }
    path.canonicalize().map_err(|_| ModelError::StoreFailed)
}

fn normalized_relative(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(unix)]
fn safe_regular_file(
    _file: &File,
    metadata: &std::fs::Metadata,
) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    metadata.is_file() && metadata.nlink() == 1
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn safe_regular_file(
    file: &File,
    metadata: &std::fs::Metadata,
) -> bool {
    use std::{
        mem::{size_of, zeroed},
        os::windows::io::AsRawHandle as _,
    };
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{FILE_STANDARD_INFO, FileStandardInfo, GetFileInformationByHandleEx},
    };

    // SAFETY: the output buffer matches FileStandardInfo and the borrowed
    // handle remains valid for the duration of the call.
    let mut information: FILE_STANDARD_INFO = unsafe { zeroed() };
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileStandardInfo,
            (&raw mut information).cast(),
            u32::try_from(size_of::<FILE_STANDARD_INFO>()).unwrap_or(0),
        )
    };
    metadata.is_file() && result != 0 && information.NumberOfLinks == 1
}

#[cfg(not(any(unix, windows)))]
fn safe_regular_file(
    _file: &File,
    metadata: &std::fs::Metadata,
) -> bool {
    metadata.is_file()
}

#[cfg(unix)]
fn same_file(
    _path: &Path,
    _opened: &File,
    left: &std::fs::Metadata,
    right: &std::fs::Metadata,
) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_file(
    path: &Path,
    opened: &File,
    _left: &std::fs::Metadata,
    _right: &std::fs::Metadata,
) -> bool {
    File::open(path)
        .ok()
        .and_then(|identity| {
            Some(
                same_file::Handle::from_file(identity).ok()?
                    == same_file::Handle::from_file(opened.try_clone().ok()?).ok()?,
            )
        })
        .unwrap_or(false)
}

#[cfg(not(any(unix, windows)))]
fn same_file(
    _path: &Path,
    _opened: &File,
    left: &std::fs::Metadata,
    right: &std::fs::Metadata,
) -> bool {
    left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.is_file() == right.is_file()
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
) {
    if let Some(tx) = &options.progress {
        // Progress is observational: a slow or departed observer must never
        // turn a successfully published model into an operation failure.
        let _ignored = tx.try_send(phase);
    }
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
