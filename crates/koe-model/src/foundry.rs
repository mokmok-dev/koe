//! Real adapter over the published `foundry-local` SDK.
//!
//! The SDK talks to the Foundry Local service on localhost. Capability
//! probing is side-effect free; catalog/download/load operations are mapped
//! 1:1 to the SDK. The native live-audio session exposed by the unreleased
//! SDK is not available in the published crate, so
//! [`FoundryLocalAdapter::create_asr_session`] reports
//! [`AdapterError::Unavailable`] (the app treats that as a capability).

use std::{
    fs::{File, OpenOptions},
    num::NonZeroU64,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use fs2::FileExt;

use crate::{
    adapter::{
        AdapterError, ArtifactValidationError, AsrSessionSettings, FoundryAdapter,
        InstalledArtifact, StreamingAsrSession,
    },
    types::{Alias, ModelDescriptor, ModelId, ModelScope, ModelSelector, ModelVersion},
};

/// Maximum directory depth scanned inside the runtime cache.
const MAX_CACHE_DEPTH: usize = 4;
/// Maximum entries examined while locating a model in a shared cache.
const MAX_CACHE_SEARCH_ENTRIES: usize = 100_000;
/// Maximum files scanned for the digest inventory.
const MAX_INVENTORY_FILES: usize = crate::MAX_MANIFEST_FILES;
/// Maximum directories traversed within one selected model inventory.
const MAX_INVENTORY_DIRECTORIES: usize = 4_096;

/// Wraps `foundry_local::FoundryLocalManager` without leaking it.
pub struct FoundryLocalAdapter {
    inner: tokio::sync::Mutex<Option<foundry_local::FoundryLocalManager>>,
    timeout_secs: u64,
}

impl FoundryLocalAdapter {
    /// Creates an adapter; the runtime is probed lazily and SDK operations
    /// use the Foundry-recommended bounded request timeout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::Mutex::new(None),
            timeout_secs: 600,
        }
    }

    /// Creates an adapter with an exact, nonzero SDK request timeout in seconds.
    #[must_use]
    pub fn with_timeout_secs(timeout: NonZeroU64) -> Self {
        Self {
            inner: tokio::sync::Mutex::new(None),
            timeout_secs: timeout.get(),
        }
    }

    async fn manager(
        &self
    ) -> Result<tokio::sync::MutexGuard<'_, Option<foundry_local::FoundryLocalManager>>, AdapterError>
    {
        let mut guard = self.inner.lock().await;
        if guard.is_none() {
            let manager = foundry_local::FoundryLocalManager::builder()
                .timeout_secs(self.timeout_secs)
                .build()
                .await
                .map_err(|_| AdapterError::Unavailable)?;
            *guard = Some(manager);
        }
        Ok(guard)
    }

    async fn remove_cached_artifact(
        &self,
        model: &ModelDescriptor,
        cache_directory: Option<&str>,
    ) -> Result<(), AdapterError> {
        let mut guard = self.manager().await?;
        let manager = guard.as_mut().ok_or(AdapterError::Unavailable)?;
        let cache_location = manager
            .get_cache_location()
            .await
            .map_err(|_| AdapterError::StorageFailed)?;
        if cache_directory.is_some_and(|directory| {
            directory
                .split(['/', '\\'])
                .next_back()
                .is_some_and(|name| {
                    name.eq_ignore_ascii_case(&model.alias.0)
                        && !name.eq_ignore_ascii_case(&model.id.0)
                })
        }) {
            let ambiguous = manager
                .list_cached_models()
                .await
                .map_err(|_| AdapterError::RuntimeFailed)?
                .iter()
                .any(|info| {
                    info.alias.eq_ignore_ascii_case(&model.alias.0)
                        && !info.id.eq_ignore_ascii_case(&model.id.0)
                });
            if ambiguous {
                return Err(AdapterError::InvalidArtifact(
                    ArtifactValidationError::InvalidPath,
                ));
            }
        }
        drop(guard);
        let cache_root = PathBuf::from(&cache_location);
        let _cache_lock = lock_cache(&cache_root).await?;
        let model_id = model.id.0.clone();
        let alias = model.alias.0.clone();
        if let Some(target) =
            removable_cache_directory(&cache_root, &model_id, &alias, cache_directory)?
        {
            tokio::fs::remove_dir_all(target)
                .await
                .map_err(|_| AdapterError::StorageFailed)?;
        }
        Ok(())
    }

    fn descriptor_from_info(info: &foundry_local::models::FoundryModelInfo) -> ModelDescriptor {
        ModelDescriptor {
            id: ModelId::new(info.id.clone()),
            alias: Alias(info.alias.clone()),
            version: ModelVersion::new(info.version.clone()),
            variant: info.runtime.get_alias(),
            provider: info.provider.clone(),
            license_id: info.license.clone(),
            license_description: info.license.clone(),
            source: info.uri.clone(),
            size_mb: u64::try_from(info.file_size_mb).unwrap_or(0),
            task: info.task.clone(),
        }
    }

    fn discover_cache_files(
        model: &ModelDescriptor,
        cache_root: &Path,
    ) -> Result<Vec<crate::adapter::InstalledFile>, AdapterError> {
        Self::discover_cache_files_and_root(&model.id.0, &model.alias.0, cache_root)
            .map(|(files, _root)| files)
    }

    fn discover_cache_files_and_root(
        model_id: &str,
        alias: &str,
        cache_root: &Path,
    ) -> Result<(Vec<crate::adapter::InstalledFile>, Option<PathBuf>), AdapterError> {
        let model_id = sanitize_component(model_id);
        let alias = sanitize_component(alias);
        let roots = [cache_root.join("models"), cache_root.to_path_buf()];
        let mut files = Vec::new();
        let mut total_size = 0_u64;
        let mut directory_count = 0_usize;
        let mut examined_entries = 0_usize;
        let mut matched_root = None;
        for root in roots {
            if !root.is_dir() {
                continue;
            }
            scan_for_model_dir(
                &root,
                cache_root,
                &model_id,
                &alias,
                0,
                &mut files,
                &mut total_size,
                &mut directory_count,
                &mut examined_entries,
                &mut matched_root,
            )?;
            if !files.is_empty() {
                break;
            }
        }
        files.sort_by(|left, right| left.relative_path().cmp(right.relative_path()));
        match (files.is_empty(), matched_root) {
            (true, None) => Err(AdapterError::NotFound),
            (true, Some(_)) => Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::EmptyInventory,
            )),
            (false, root) => Ok((files, root)),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_for_model_dir(
    directory: &Path,
    cache_root: &Path,
    model_id: &str,
    alias: &str,
    depth: usize,
    files: &mut Vec<crate::adapter::InstalledFile>,
    total_size: &mut u64,
    directory_count: &mut usize,
    examined_entries: &mut usize,
    matched_root: &mut Option<PathBuf>,
) -> Result<(), AdapterError> {
    if depth > MAX_CACHE_DEPTH {
        return Ok(());
    }
    let entries = std::fs::read_dir(directory).map_err(|_| AdapterError::RuntimeFailed)?;
    for entry in entries {
        *examined_entries = examined_entries.saturating_add(1);
        if *examined_entries > MAX_CACHE_SEARCH_ENTRIES {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::LimitExceeded,
            ));
        }
        let entry = entry.map_err(|_| AdapterError::RuntimeFailed)?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|_| AdapterError::RuntimeFailed)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if file_type.is_symlink() {
            if name.eq_ignore_ascii_case(model_id) || name.eq_ignore_ascii_case(alias) {
                return Err(AdapterError::InvalidArtifact(
                    ArtifactValidationError::Symlink,
                ));
            }
            continue;
        }
        if file_type.is_dir() {
            if name.eq_ignore_ascii_case(model_id) || name.eq_ignore_ascii_case(alias) {
                if matched_root.replace(path.clone()).is_some() {
                    return Err(AdapterError::InvalidArtifact(
                        ArtifactValidationError::DuplicateEntry,
                    ));
                }
                collect_files(cache_root, &path, files, total_size, 0, directory_count)?;
            } else {
                scan_for_model_dir(
                    &path,
                    cache_root,
                    model_id,
                    alias,
                    depth + 1,
                    files,
                    total_size,
                    directory_count,
                    examined_entries,
                    matched_root,
                )?;
            }
        }
    }
    Ok(())
}

fn collect_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<crate::adapter::InstalledFile>,
    total_size: &mut u64,
    depth: usize,
    directory_count: &mut usize,
) -> Result<(), AdapterError> {
    *directory_count = directory_count.saturating_add(1);
    if depth > MAX_CACHE_DEPTH || *directory_count > MAX_INVENTORY_DIRECTORIES {
        return Err(AdapterError::InvalidArtifact(
            ArtifactValidationError::InvalidPath,
        ));
    }
    let entries = std::fs::read_dir(directory).map_err(|_| AdapterError::RuntimeFailed)?;
    for entry in entries {
        let entry = entry.map_err(|_| AdapterError::RuntimeFailed)?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|_| AdapterError::RuntimeFailed)?;
        if file_type.is_symlink() {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::InvalidPath,
            ));
        }
        if file_type.is_dir() {
            collect_files(root, &path, files, total_size, depth + 1, directory_count)?;
        } else if file_type.is_file() {
            if files.len() >= MAX_INVENTORY_FILES {
                return Err(AdapterError::InvalidArtifact(
                    ArtifactValidationError::LimitExceeded,
                ));
            }
            let metadata = entry.metadata().map_err(|_| AdapterError::RuntimeFailed)?;
            *total_size =
                total_size
                    .checked_add(metadata.len())
                    .ok_or(AdapterError::InvalidArtifact(
                        ArtifactValidationError::LimitExceeded,
                    ))?;
            if *total_size > crate::MAX_ARTIFACT_INVENTORY_BYTES {
                return Err(AdapterError::InvalidArtifact(
                    ArtifactValidationError::LimitExceeded,
                ));
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|_| AdapterError::RuntimeFailed)?;
            files.push(crate::adapter::InstalledFile::try_from_cache_path_blocking(
                root, relative,
            )?);
        } else {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::InvalidPath,
            ));
        }
    }
    Ok(())
}

fn locate_cache_directory(
    cache_root: &Path,
    model_id: &str,
    alias: &str,
) -> Result<Option<PathBuf>, AdapterError> {
    fn scan(
        directory: &Path,
        model_id: &str,
        alias: &str,
        depth: usize,
        examined: &mut usize,
        matched: &mut Option<PathBuf>,
    ) -> Result<(), AdapterError> {
        if depth > MAX_CACHE_DEPTH {
            return Ok(());
        }
        for entry in std::fs::read_dir(directory).map_err(|_| AdapterError::RuntimeFailed)? {
            *examined = examined.saturating_add(1);
            if *examined > MAX_CACHE_SEARCH_ENTRIES {
                return Err(AdapterError::InvalidArtifact(
                    ArtifactValidationError::LimitExceeded,
                ));
            }
            let entry = entry.map_err(|_| AdapterError::RuntimeFailed)?;
            let kind = entry.file_type().map_err(|_| AdapterError::RuntimeFailed)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_target = name.eq_ignore_ascii_case(model_id) || name.eq_ignore_ascii_case(alias);
            if kind.is_symlink() && is_target {
                return Err(AdapterError::InvalidArtifact(
                    ArtifactValidationError::Symlink,
                ));
            }
            if kind.is_dir() {
                if is_target {
                    if matched.replace(entry.path()).is_some() {
                        return Err(AdapterError::InvalidArtifact(
                            ArtifactValidationError::DuplicateEntry,
                        ));
                    }
                } else {
                    scan(&entry.path(), model_id, alias, depth + 1, examined, matched)?;
                }
            }
        }
        Ok(())
    }

    let mut matched = None;
    let mut examined = 0;
    scan(cache_root, model_id, alias, 0, &mut examined, &mut matched)?;
    Ok(matched)
}

async fn lock_cache(cache_root: &Path) -> Result<std::sync::Arc<File>, AdapterError> {
    let path = cache_root.join(".koe-cache.lock");
    tokio::task::spawn_blocking(move || lock_cache_blocking(&path))
        .await
        .map_err(|_| AdapterError::RuntimeFailed)?
        .map(std::sync::Arc::new)
}

fn lock_cache_blocking(path: &Path) -> Result<File, AdapterError> {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(AdapterError::InvalidArtifact(
            ArtifactValidationError::Symlink,
        ));
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|_| AdapterError::StorageFailed)?;
    file.lock_exclusive()
        .map_err(|_| AdapterError::StorageFailed)?;
    Ok(file)
}

fn removable_cache_directory(
    cache_root: &Path,
    model_id: &str,
    alias: &str,
    cache_directory: Option<&str>,
) -> Result<Option<PathBuf>, AdapterError> {
    let canonical_root = cache_root
        .canonicalize()
        .map_err(|_| AdapterError::StorageFailed)?;
    let model_id = sanitize_component(model_id);
    let alias = sanitize_component(alias);
    let candidates = if let Some(directory) = cache_directory {
        let components = directory.split(['/', '\\']).collect::<Vec<_>>();
        if components
            .iter()
            .any(|component| matches!(*component, "" | "." | ".."))
            || !components.last().is_some_and(|component| {
                component.eq_ignore_ascii_case(&model_id) || component.eq_ignore_ascii_case(&alias)
            })
        {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::InvalidPath,
            ));
        }
        vec![cache_root.join(directory)]
    } else {
        vec![locate_cache_directory(cache_root, &model_id, &alias)?.ok_or(AdapterError::NotFound)?]
    };
    let mut targets = Vec::new();
    for candidate in candidates {
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => targets.push(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(_) => return Err(AdapterError::StorageFailed),
        }
    }
    targets.sort();
    targets.dedup();
    let [target] = targets.as_slice() else {
        return if targets.is_empty() {
            Err(AdapterError::StorageFailed)
        } else {
            Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::InvalidPath,
            ))
        };
    };
    let metadata = std::fs::symlink_metadata(target).map_err(|_| AdapterError::StorageFailed)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AdapterError::InvalidArtifact(
            ArtifactValidationError::InvalidPath,
        ));
    }
    let canonical_target = target
        .canonicalize()
        .map_err(|_| AdapterError::StorageFailed)?;
    let canonical_models = cache_root.join("models").canonicalize().ok();
    if canonical_target == canonical_root
        || canonical_models.as_ref() == Some(&canonical_target)
        || !canonical_target.starts_with(&canonical_root)
        || !canonical_target.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name.eq_ignore_ascii_case(&model_id) || name.eq_ignore_ascii_case(&alias)
        })
    {
        return Err(AdapterError::InvalidArtifact(
            ArtifactValidationError::InvalidPath,
        ));
    }
    Ok(Some(canonical_target))
}

async fn cleanup_failed_install(
    cache_root: &Path,
    model: &ModelDescriptor,
) -> Result<(), AdapterError> {
    let target = match removable_cache_directory(cache_root, &model.id.0, &model.alias.0, None) {
        Ok(target) => target,
        Err(AdapterError::NotFound) => None,
        Err(error) => return Err(error),
    };
    if let Some(target) = target {
        tokio::fs::remove_dir_all(target)
            .await
            .map_err(|_| AdapterError::StorageFailed)?;
    }
    Ok(())
}

fn descriptor_matches_info(
    model: &ModelDescriptor,
    info: &foundry_local::models::FoundryModelInfo,
) -> bool {
    info.id.eq_ignore_ascii_case(&model.id.0)
        && info.alias.eq_ignore_ascii_case(&model.alias.0)
        && info.version == model.version.0
        && info
            .runtime
            .get_alias()
            .eq_ignore_ascii_case(&model.variant)
}

fn sanitize_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

impl Default for FoundryLocalAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
#[allow(clippy::significant_drop_tightening)]
impl FoundryAdapter for FoundryLocalAdapter {
    fn backend_name(&self) -> &'static str {
        "foundry-local"
    }

    async fn list_catalog(&mut self) -> Result<Vec<ModelDescriptor>, AdapterError> {
        let mut guard = self.manager().await?;
        let manager = guard.as_mut().ok_or(AdapterError::Unavailable)?;
        manager
            .list_catalog_models()
            .await
            .map(|models| models.iter().map(Self::descriptor_from_info).collect())
            .map_err(|_| AdapterError::CatalogFailed)
    }

    async fn resolve(
        &mut self,
        selector: &ModelSelector,
    ) -> Result<ModelDescriptor, AdapterError> {
        let mut guard = self.manager().await?;
        let manager = guard.as_mut().ok_or(AdapterError::Unavailable)?;
        let key = match selector {
            ModelSelector::Alias(alias) => alias.clone(),
            ModelSelector::Id(id) => id.0.clone(),
        };
        manager
            .get_model_info(&key, true)
            .await
            .map(|info| Self::descriptor_from_info(&info))
            .map_err(|_| AdapterError::NotFound)
    }

    async fn latest_version(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<ModelVersion, AdapterError> {
        let mut guard = self.manager().await?;
        let manager = guard.as_mut().ok_or(AdapterError::Unavailable)?;
        let catalog = manager
            .list_catalog_models()
            .await
            .map_err(|_| AdapterError::CatalogFailed)?;
        catalog
            .iter()
            .filter(|info| {
                info.id.eq_ignore_ascii_case(&model.id.0)
                    || info.alias.eq_ignore_ascii_case(&model.alias.0)
            })
            .map(|info| info.version.clone())
            .max()
            .map(ModelVersion::new)
            .ok_or(AdapterError::NotFound)
    }

    async fn list_installed(&mut self) -> Result<Vec<ModelDescriptor>, AdapterError> {
        let mut guard = self.manager().await?;
        let manager = guard.as_mut().ok_or(AdapterError::Unavailable)?;
        manager
            .list_cached_models()
            .await
            .map(|models| models.iter().map(Self::descriptor_from_info).collect())
            .map_err(|_| AdapterError::RuntimeFailed)
    }

    async fn list_loaded(&mut self) -> Result<Vec<ModelId>, AdapterError> {
        let mut guard = self.manager().await?;
        let manager = guard.as_mut().ok_or(AdapterError::Unavailable)?;
        manager
            .list_loaded_models()
            .await
            .map(|models| {
                models
                    .iter()
                    .map(|info| ModelId::new(info.id.clone()))
                    .collect()
            })
            .map_err(|_| AdapterError::RuntimeFailed)
    }

    async fn install(
        &mut self,
        model: &ModelDescriptor,
        cancel: &tokio_util::sync::CancellationToken,
        force: bool,
    ) -> Result<InstalledArtifact, AdapterError> {
        if cancel.is_cancelled() {
            return Err(AdapterError::DownloadFailed);
        }
        let mut guard = self.manager().await?;
        let manager = guard.as_mut().ok_or(AdapterError::Unavailable)?;
        let cache_location = manager
            .get_cache_location()
            .await
            .map_err(|_| AdapterError::RuntimeFailed)?;
        let cache_root = PathBuf::from(&cache_location);
        let cache_lock = lock_cache(&cache_root).await?;
        let preexisting =
            locate_cache_directory(&cache_root, &model.id.0, &model.alias.0)?.is_some();
        let Ok(downloaded) = manager.download_model(&model.id.0, None, force).await else {
            drop(guard);
            if !preexisting {
                cleanup_failed_install(&cache_root, model).await?;
            }
            return Err(AdapterError::DownloadFailed);
        };
        let outcome = async {
            if !descriptor_matches_info(model, &downloaded) {
                return Err(AdapterError::RuntimeFailed);
            }
            let cached = manager
                .list_cached_models()
                .await
                .map_err(|_| AdapterError::RuntimeFailed)?;
            if cached
                .iter()
                .filter(|info| descriptor_matches_info(model, info))
                .count()
                != 1
            {
                return Err(AdapterError::RuntimeFailed);
            }
            if cancel.is_cancelled() {
                return Err(AdapterError::DownloadFailed);
            }
            let inventory_root = cache_root.clone();
            let inventory_model = model.clone();
            let files = tokio::task::spawn_blocking(move || {
                Self::discover_cache_files(&inventory_model, &inventory_root)
            })
            .await
            .map_err(|_| AdapterError::RuntimeFailed)??;
            if cancel.is_cancelled() {
                return Err(AdapterError::DownloadFailed);
            }
            Ok(files)
        }
        .await;
        drop(guard);
        match outcome {
            Ok(files) => Ok(InstalledArtifact {
                cache_root,
                model_id: model.id.clone(),
                files,
                created_by_operation: !preexisting,
                operation_lease: Some(cache_lock),
            }),
            Err(error) => {
                if !preexisting {
                    cleanup_failed_install(&cache_root, model).await?;
                }
                Err(error)
            },
        }
    }

    async fn inspect_local_artifact(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<InstalledArtifact, AdapterError> {
        let mut guard = self.manager().await?;
        let manager = guard.as_mut().ok_or(AdapterError::Unavailable)?;
        let cached = manager
            .list_cached_models()
            .await
            .map_err(|_| AdapterError::RuntimeFailed)?;
        if cached
            .iter()
            .filter(|info| descriptor_matches_info(model, info))
            .count()
            != 1
        {
            return Err(AdapterError::NotFound);
        }
        let cache_root = PathBuf::from(
            manager
                .get_cache_location()
                .await
                .map_err(|_| AdapterError::RuntimeFailed)?,
        );
        let cache_lock = lock_cache(&cache_root).await?;
        drop(guard);
        let inventory_root = cache_root.clone();
        let inventory_model = model.clone();
        let files = tokio::runtime::Handle::try_current()
            .map_err(|_| AdapterError::RuntimeFailed)?
            .spawn_blocking(move || Self::discover_cache_files(&inventory_model, &inventory_root))
            .await
            .map_err(|_| AdapterError::RuntimeFailed)??;
        Ok(InstalledArtifact {
            cache_root,
            model_id: model.id.clone(),
            files,
            created_by_operation: false,
            operation_lease: Some(cache_lock),
        })
    }

    async fn load(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<(), AdapterError> {
        let mut guard = self.manager().await?;
        let manager = guard.as_mut().ok_or(AdapterError::Unavailable)?;
        manager
            .load_model(&model.id.0, Some(600))
            .await
            .map(|_| ())
            .map_err(|_| AdapterError::RuntimeFailed)
    }

    async fn unload(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<(), AdapterError> {
        let mut guard = self.manager().await?;
        let manager = guard.as_mut().ok_or(AdapterError::Unavailable)?;
        manager
            .unload_model(&model.id.0, false)
            .await
            .map_err(|_| AdapterError::RuntimeFailed)
    }

    async fn remove_from_cache(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<(), AdapterError> {
        self.remove_cached_artifact(model, None).await
    }

    async fn remove_artifact_from_cache(
        &mut self,
        model: &ModelDescriptor,
        cache_directory: Option<&str>,
    ) -> Result<(), AdapterError> {
        self.remove_cached_artifact(model, cache_directory).await
    }

    async fn create_asr_session(
        &mut self,
        _model: &ModelDescriptor,
        _settings: &AsrSessionSettings,
    ) -> Result<Box<dyn StreamingAsrSession>, AdapterError> {
        // The native live-audio session is not in the published SDK; the
        // capability is reported as unavailable until the SDK ships it.
        Err(AdapterError::Unavailable)
    }

    fn offline_scopes(&self) -> Vec<ModelScope> {
        vec![ModelScope::Installed, ModelScope::Loaded]
    }
}
