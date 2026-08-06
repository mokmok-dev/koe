//! Real adapter over the published `foundry-local` SDK.
//!
//! The SDK talks to the Foundry Local service on localhost. Capability
//! probing is side-effect free; catalog/download/load operations are mapped
//! 1:1 to the SDK. The native live-audio session exposed by the unreleased
//! SDK is not available in the published crate, so
//! [`FoundryLocalAdapter::create_asr_session`] reports
//! [`AdapterError::Unavailable`] (the app treats that as a capability).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use sha2::Digest;

use crate::{
    adapter::{
        AdapterError, AsrSessionSettings, FoundryAdapter, InstalledArtifact, StreamingAsrSession,
    },
    types::{Alias, ModelDescriptor, ModelId, ModelScope, ModelSelector, ModelVersion},
};

/// Maximum directory depth scanned inside the runtime cache.
const MAX_CACHE_DEPTH: usize = 4;
/// Maximum files scanned for the digest inventory.
const MAX_INVENTORY_FILES: usize = 1_024;

/// Wraps `foundry_local::FoundryLocalManager` without leaking it.
pub struct FoundryLocalAdapter {
    inner: tokio::sync::Mutex<Option<foundry_local::FoundryLocalManager>>,
    timeout_secs: u64,
}

impl FoundryLocalAdapter {
    /// Creates an adapter; the runtime is probed lazily.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::Mutex::new(None),
            timeout_secs: 600,
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
        let model_id = sanitize_component(&model.id.0);
        let alias = sanitize_component(&model.alias.0);
        let roots = [cache_root.join("models"), cache_root.to_path_buf()];
        let mut files = Vec::new();
        for root in roots {
            if !root.is_dir() {
                continue;
            }
            scan_for_model_dir(&root, &model_id, &alias, 0, &mut files)?;
            if !files.is_empty() {
                break;
            }
        }
        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(files)
    }
}

fn scan_for_model_dir(
    directory: &Path,
    model_id: &str,
    alias: &str,
    depth: usize,
    files: &mut Vec<crate::adapter::InstalledFile>,
) -> Result<(), AdapterError> {
    if depth > MAX_CACHE_DEPTH || files.len() >= MAX_INVENTORY_FILES {
        return Ok(());
    }
    let entries = std::fs::read_dir(directory).map_err(|_| AdapterError::RuntimeFailed)?;
    for entry in entries {
        if files.len() >= MAX_INVENTORY_FILES {
            break;
        }
        let entry = entry.map_err(|_| AdapterError::RuntimeFailed)?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|_| AdapterError::RuntimeFailed)?;
        if file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if file_type.is_dir() {
            if name.to_ascii_lowercase() == model_id || name.to_ascii_lowercase() == alias {
                collect_files(&path, &path, files)?;
            } else {
                scan_for_model_dir(&path, model_id, alias, depth + 1, files)?;
            }
        }
    }
    Ok(())
}

fn collect_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<crate::adapter::InstalledFile>,
) -> Result<(), AdapterError> {
    if files.len() >= MAX_INVENTORY_FILES {
        return Ok(());
    }
    let entries = std::fs::read_dir(directory).map_err(|_| AdapterError::RuntimeFailed)?;
    for entry in entries {
        if files.len() >= MAX_INVENTORY_FILES {
            break;
        }
        let entry = entry.map_err(|_| AdapterError::RuntimeFailed)?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|_| AdapterError::RuntimeFailed)?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_files(root, &path, files)?;
        } else if file_type.is_file() {
            let bytes = std::fs::read(&path).map_err(|_| AdapterError::RuntimeFailed)?;
            let sha256 = crate::fixture::hex_encode(&sha2::Sha256::digest(&bytes));
            let relative = path
                .strip_prefix(root)
                .map_err(|_| AdapterError::RuntimeFailed)?
                .to_string_lossy()
                .replace('\\', "/");
            files.push(crate::adapter::InstalledFile {
                absolute_path: path,
                relative_path: relative,
                size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                sha256,
            });
        }
    }
    Ok(())
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
        manager
            .download_model(&model.id.0, None, force)
            .await
            .map_err(|_| AdapterError::DownloadFailed)?;
        if cancel.is_cancelled() {
            return Err(AdapterError::DownloadFailed);
        }
        let cache_location = manager
            .get_cache_location()
            .await
            .map_err(|_| AdapterError::RuntimeFailed)?;
        let cache_root = PathBuf::from(&cache_location);
        let files = Self::discover_cache_files(model, &cache_root)
            .map_err(|_| AdapterError::RuntimeFailed)?;
        Ok(InstalledArtifact {
            cache_root,
            model_id: model.id.clone(),
            files,
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
        // The published SDK does not expose `remove_from_cache`; remove the
        // validated model directory under the SDK-reported cache root.
        let mut guard = self.manager().await?;
        let manager = guard.as_mut().ok_or(AdapterError::Unavailable)?;
        let cache_location = manager
            .get_cache_location()
            .await
            .map_err(|_| AdapterError::RuntimeFailed)?;
        let cache_root = PathBuf::from(&cache_location);
        let model_dir = cache_root
            .join("models")
            .join(sanitize_component(&model.id.0));
        if model_dir.exists() {
            let canonical_root = cache_root
                .canonicalize()
                .map_err(|_| AdapterError::RuntimeFailed)?;
            let canonical_dir = model_dir
                .canonicalize()
                .map_err(|_| AdapterError::RuntimeFailed)?;
            if !canonical_dir.starts_with(&canonical_root) {
                return Err(AdapterError::RuntimeFailed);
            }
            std::fs::remove_dir_all(&canonical_dir).map_err(|_| AdapterError::RuntimeFailed)?;
        }
        Ok(())
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
