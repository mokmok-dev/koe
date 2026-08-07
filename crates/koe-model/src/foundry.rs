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

use crate::{
    adapter::{
        AdapterError, AsrSessionSettings, FoundryAdapter, InstalledArtifact, StreamingAsrSession,
    },
    types::{Alias, ModelDescriptor, ModelId, ModelScope, ModelSelector, ModelVersion},
};

/// Maximum directory depth scanned inside the runtime cache.
const MAX_CACHE_DEPTH: usize = 4;
/// Maximum files and bytes admitted to one digest inventory.
const MAX_INVENTORY_FILES: usize = 1_024;
const MAX_INVENTORY_FILE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_INVENTORY_TOTAL_BYTES: u64 = 16 * 1024 * 1024 * 1024;
/// Maximum cache entries examined while locating and inventorying one model.
const MAX_CACHE_ENTRIES: usize = 16_384;

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

    fn discover_cache_artifact(
        model: &ModelDescriptor,
        cache_root: &Path,
    ) -> Result<DiscoveredCacheArtifact, AdapterError> {
        let model_id = sanitize_component(&model.id.0).to_ascii_lowercase();
        let alias = sanitize_component(&model.alias.0).to_ascii_lowercase();
        let models_root = cache_root.join("models");
        let search_root = if models_root.is_dir() {
            models_root
        } else {
            cache_root.to_path_buf()
        };
        let mut candidates = Vec::new();
        let mut entries_examined = 0_usize;
        find_model_dirs(
            &search_root,
            &model_id,
            &alias,
            0,
            &mut candidates,
            &mut entries_examined,
        )?;
        candidates.sort();
        candidates.dedup();
        let [artifact_root] = candidates.as_slice() else {
            return if candidates.is_empty() {
                Err(AdapterError::NotFound)
            } else {
                // Never merge inventories from ID/alias duplicates or
                // side-by-side versions. The old SDK does not expose an
                // authoritative artifact path, so ambiguity is fail-closed.
                Err(AdapterError::RuntimeFailed)
            };
        };
        let mut files = Vec::new();
        let mut total_bytes = 0_u64;
        collect_files(
            cache_root,
            artifact_root,
            0,
            &mut files,
            &mut total_bytes,
            &mut entries_examined,
        )?;
        if files.is_empty() {
            return Err(AdapterError::RuntimeFailed);
        }
        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(DiscoveredCacheArtifact {
            artifact_root: artifact_root.clone(),
            files,
        })
    }
}

struct DiscoveredCacheArtifact {
    artifact_root: PathBuf,
    files: Vec<crate::adapter::InstalledFile>,
}

fn find_model_dirs(
    directory: &Path,
    model_id: &str,
    alias: &str,
    depth: usize,
    candidates: &mut Vec<PathBuf>,
    entries_examined: &mut usize,
) -> Result<(), AdapterError> {
    if depth > MAX_CACHE_DEPTH {
        return Ok(());
    }
    let entries = std::fs::read_dir(directory).map_err(|_| AdapterError::RuntimeFailed)?;
    for entry in entries {
        count_cache_entry(entries_examined)?;
        let entry = entry.map_err(|_| AdapterError::RuntimeFailed)?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|_| AdapterError::RuntimeFailed)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let name_matches = name.eq_ignore_ascii_case(model_id) || name.eq_ignore_ascii_case(alias);
        if file_type.is_symlink() {
            if name_matches {
                return Err(AdapterError::RuntimeFailed);
            }
            continue;
        }
        if file_type.is_dir() {
            if name_matches {
                candidates.push(path);
            } else if depth < MAX_CACHE_DEPTH {
                find_model_dirs(
                    &path,
                    model_id,
                    alias,
                    depth + 1,
                    candidates,
                    entries_examined,
                )?;
            }
        }
    }
    Ok(())
}

fn collect_files(
    cache_root: &Path,
    directory: &Path,
    depth: usize,
    files: &mut Vec<crate::adapter::InstalledFile>,
    total_bytes: &mut u64,
    entries_examined: &mut usize,
) -> Result<(), AdapterError> {
    if depth > MAX_CACHE_DEPTH {
        return Err(AdapterError::RuntimeFailed);
    }
    let entries = std::fs::read_dir(directory).map_err(|_| AdapterError::RuntimeFailed)?;
    for entry in entries {
        count_cache_entry(entries_examined)?;
        let entry = entry.map_err(|_| AdapterError::RuntimeFailed)?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|_| AdapterError::RuntimeFailed)?;
        if file_type.is_symlink() {
            // Runtime loaders may follow links that are invisible to a digest
            // inventory. Reject the complete artifact instead of skipping it.
            return Err(AdapterError::RuntimeFailed);
        }
        if file_type.is_dir() {
            collect_files(
                cache_root,
                &path,
                depth + 1,
                files,
                total_bytes,
                entries_examined,
            )?;
        } else if file_type.is_file() {
            if files.len() >= MAX_INVENTORY_FILES {
                return Err(AdapterError::RuntimeFailed);
            }
            let size = entry
                .metadata()
                .map_err(|_| AdapterError::RuntimeFailed)?
                .len();
            if size > MAX_INVENTORY_FILE_BYTES {
                return Err(AdapterError::RuntimeFailed);
            }
            *total_bytes = total_bytes
                .checked_add(size)
                .filter(|total| *total <= MAX_INVENTORY_TOTAL_BYTES)
                .ok_or(AdapterError::RuntimeFailed)?;
            let relative = path
                .strip_prefix(cache_root)
                .map_err(|_| AdapterError::RuntimeFailed)?
                .to_string_lossy()
                .replace('\\', "/");
            files.push(crate::adapter::InstalledFile {
                absolute_path: path,
                relative_path: relative,
                size,
                // The manager computes the authoritative digest with a
                // bounded streaming reader after validating the cache path.
                sha256: String::new(),
            });
        } else {
            return Err(AdapterError::RuntimeFailed);
        }
    }
    Ok(())
}

fn count_cache_entry(entries_examined: &mut usize) -> Result<(), AdapterError> {
    *entries_examined = entries_examined
        .checked_add(1)
        .filter(|count| *count <= MAX_CACHE_ENTRIES)
        .ok_or(AdapterError::RuntimeFailed)?;
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

fn remove_discovered_cache_artifact(
    model: &ModelDescriptor,
    cache_root: &Path,
) -> Result<(), AdapterError> {
    let artifact = FoundryLocalAdapter::discover_cache_artifact(model, cache_root)?;
    let root_metadata =
        std::fs::symlink_metadata(cache_root).map_err(|_| AdapterError::RuntimeFailed)?;
    let artifact_metadata = std::fs::symlink_metadata(&artifact.artifact_root)
        .map_err(|_| AdapterError::RuntimeFailed)?;
    if root_metadata.file_type().is_symlink()
        || !root_metadata.is_dir()
        || artifact_metadata.file_type().is_symlink()
        || !artifact_metadata.is_dir()
    {
        return Err(AdapterError::RuntimeFailed);
    }
    let canonical_root = cache_root
        .canonicalize()
        .map_err(|_| AdapterError::RuntimeFailed)?;
    let canonical_artifact = artifact
        .artifact_root
        .canonicalize()
        .map_err(|_| AdapterError::RuntimeFailed)?;
    if canonical_artifact == canonical_root || !canonical_artifact.starts_with(&canonical_root) {
        return Err(AdapterError::RuntimeFailed);
    }
    // Delete the validated entry path, never the canonical target. Recheck the
    // entry immediately before deletion so an entry symlink is not followed.
    let final_metadata = std::fs::symlink_metadata(&artifact.artifact_root)
        .map_err(|_| AdapterError::RuntimeFailed)?;
    if final_metadata.file_type().is_symlink() || !final_metadata.is_dir() {
        return Err(AdapterError::RuntimeFailed);
    }
    std::fs::remove_dir_all(&artifact.artifact_root).map_err(|_| AdapterError::RuntimeFailed)
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

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use tempfile::TempDir;

    use super::{
        FoundryLocalAdapter, MAX_INVENTORY_FILES, remove_discovered_cache_artifact,
        sanitize_component,
    };
    use crate::FixtureFoundryAdapter;

    #[test]
    fn nested_foundry_cache_paths_are_relative_to_the_cache_root() {
        let cache = TempDir::new().expect("cache");
        let descriptor = FixtureFoundryAdapter::fixture_descriptor();
        let model_dir = cache
            .path()
            .join("models")
            .join(sanitize_component(&descriptor.id.0));
        std::fs::create_dir_all(&model_dir).expect("model dir");
        std::fs::write(model_dir.join("model.bin"), b"model").expect("model");

        let artifact = FoundryLocalAdapter::discover_cache_artifact(&descriptor, cache.path())
            .expect("discover");
        assert_eq!(artifact.files.len(), 1);
        assert_eq!(
            artifact.files[0].relative_path,
            format!("models/{}/model.bin", sanitize_component(&descriptor.id.0))
        );
    }

    #[test]
    fn global_cache_entry_limit_is_inclusive_and_then_rejected() {
        let mut examined = super::MAX_CACHE_ENTRIES - 1;
        super::count_cache_entry(&mut examined).expect("exact entry limit");
        assert_eq!(examined, super::MAX_CACHE_ENTRIES);
        assert!(super::count_cache_entry(&mut examined).is_err());
    }

    #[test]
    fn inventory_file_limit_is_inclusive_and_then_rejected() {
        let cache = TempDir::new().expect("cache");
        let descriptor = FixtureFoundryAdapter::fixture_descriptor();
        let model_dir = cache
            .path()
            .join("models")
            .join(sanitize_component(&descriptor.id.0));
        std::fs::create_dir_all(&model_dir).expect("model dir");
        for index in 0..MAX_INVENTORY_FILES {
            std::fs::write(model_dir.join(format!("{index}.bin")), []).expect("file");
        }
        assert_eq!(
            FoundryLocalAdapter::discover_cache_artifact(&descriptor, cache.path())
                .expect("exact limit")
                .files
                .len(),
            MAX_INVENTORY_FILES
        );
        std::fs::write(model_dir.join("overflow.bin"), []).expect("overflow");
        assert!(FoundryLocalAdapter::discover_cache_artifact(&descriptor, cache.path()).is_err());
    }

    #[test]
    fn inventory_byte_limits_are_inclusive_and_then_rejected() {
        let descriptor = FixtureFoundryAdapter::fixture_descriptor();

        let cache = TempDir::new().expect("cache");
        let model_dir = cache
            .path()
            .join("models")
            .join(sanitize_component(&descriptor.id.0));
        std::fs::create_dir_all(&model_dir).expect("model dir");
        let one_file = std::fs::File::create(model_dir.join("model.bin")).expect("file");
        one_file
            .set_len(super::MAX_INVENTORY_FILE_BYTES)
            .expect("sparse boundary");
        assert_eq!(
            FoundryLocalAdapter::discover_cache_artifact(&descriptor, cache.path())
                .expect("exact file limit")
                .files
                .len(),
            1
        );
        one_file
            .set_len(super::MAX_INVENTORY_FILE_BYTES + 1)
            .expect("sparse overflow");
        assert!(FoundryLocalAdapter::discover_cache_artifact(&descriptor, cache.path()).is_err());

        let cache = TempDir::new().expect("cache");
        let model_dir = cache
            .path()
            .join("models")
            .join(sanitize_component(&descriptor.id.0));
        std::fs::create_dir_all(&model_dir).expect("model dir");
        for name in ["first.bin", "second.bin"] {
            std::fs::File::create(model_dir.join(name))
                .expect("file")
                .set_len(super::MAX_INVENTORY_TOTAL_BYTES / 2)
                .expect("sparse half");
        }
        assert_eq!(
            FoundryLocalAdapter::discover_cache_artifact(&descriptor, cache.path())
                .expect("exact total limit")
                .files
                .len(),
            2
        );
        std::fs::write(model_dir.join("overflow.bin"), [0]).expect("overflow");
        assert!(FoundryLocalAdapter::discover_cache_artifact(&descriptor, cache.path()).is_err());
    }

    #[test]
    fn model_discovery_honors_the_maximum_depth() {
        let descriptor = FixtureFoundryAdapter::fixture_descriptor();
        for (extra_depth, expected_files) in [(0_usize, 1_usize), (1, 0)] {
            let cache = TempDir::new().expect("cache");
            let mut directory = cache.path().join("models");
            for index in 0..super::MAX_CACHE_DEPTH + extra_depth {
                directory = directory.join(format!("level-{index}"));
            }
            directory = directory.join(sanitize_component(&descriptor.alias.0));
            std::fs::create_dir_all(&directory).expect("model dir");
            std::fs::write(directory.join("model.bin"), b"model").expect("model");
            assert_eq!(
                FoundryLocalAdapter::discover_cache_artifact(&descriptor, cache.path())
                    .map_or(0, |artifact| artifact.files.len()),
                expected_files
            );
        }
    }

    #[test]
    fn nested_artifact_beyond_the_maximum_depth_is_rejected() {
        let cache = TempDir::new().expect("cache");
        let descriptor = FixtureFoundryAdapter::fixture_descriptor();
        let mut directory = cache
            .path()
            .join("models")
            .join(sanitize_component(&descriptor.id.0));
        for index in 0..=super::MAX_CACHE_DEPTH {
            directory = directory.join(format!("artifact-{index}"));
        }
        std::fs::create_dir_all(&directory).expect("artifact dir");
        std::fs::write(directory.join("model.bin"), b"model").expect("model");
        assert!(FoundryLocalAdapter::discover_cache_artifact(&descriptor, cache.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cache_discovery_rejects_symlinks_inside_the_artifact() {
        use std::os::unix::fs::symlink;

        let cache = TempDir::new().expect("cache");
        let descriptor = FixtureFoundryAdapter::fixture_descriptor();
        let model_dir = cache
            .path()
            .join("models")
            .join(sanitize_component(&descriptor.id.0));
        std::fs::create_dir_all(&model_dir).expect("model dir");
        let outside = cache.path().join("outside.bin");
        std::fs::write(&outside, b"outside").expect("outside");
        symlink(&outside, model_dir.join("link.bin")).expect("symlink");
        std::fs::write(model_dir.join("model.bin"), b"model").expect("model");
        assert!(
            FoundryLocalAdapter::discover_cache_artifact(&descriptor, cache.path()).is_err(),
            "a runtime-visible symlink must not be omitted from verification"
        );
    }

    #[test]
    fn cache_discovery_rejects_id_alias_and_side_by_side_ambiguity() {
        let descriptor = FixtureFoundryAdapter::fixture_descriptor();
        for candidate_names in [
            vec![
                sanitize_component(&descriptor.id.0),
                sanitize_component(&descriptor.alias.0),
            ],
            vec![
                format!("v1/{}", sanitize_component(&descriptor.id.0)),
                format!("v2/{}", sanitize_component(&descriptor.id.0)),
            ],
        ] {
            let cache = TempDir::new().expect("cache");
            for name in candidate_names {
                let directory = cache.path().join("models").join(name);
                std::fs::create_dir_all(&directory).expect("model dir");
                std::fs::write(directory.join("model.bin"), b"model").expect("model");
            }
            assert!(
                FoundryLocalAdapter::discover_cache_artifact(&descriptor, cache.path()).is_err()
            );
        }
    }

    #[test]
    fn nested_alias_artifact_is_removed_exactly() {
        let cache = TempDir::new().expect("cache");
        let descriptor = FixtureFoundryAdapter::fixture_descriptor();
        let artifact_root = cache
            .path()
            .join("models/version/variant")
            .join(sanitize_component(&descriptor.alias.0));
        std::fs::create_dir_all(&artifact_root).expect("model dir");
        std::fs::write(artifact_root.join("model.bin"), b"model").expect("model");
        let sibling = cache.path().join("models/keep.bin");
        std::fs::write(&sibling, b"keep").expect("sibling");

        remove_discovered_cache_artifact(&descriptor, cache.path()).expect("remove");
        assert!(!artifact_root.exists());
        assert!(sibling.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cache_entry_symlink_is_never_followed_during_removal() {
        use std::os::unix::fs::symlink;

        let cache = TempDir::new().expect("cache");
        let outside = TempDir::new().expect("outside");
        let descriptor = FixtureFoundryAdapter::fixture_descriptor();
        let models = cache.path().join("models");
        std::fs::create_dir_all(&models).expect("models");
        std::fs::write(outside.path().join("keep.bin"), b"keep").expect("outside file");
        symlink(
            outside.path(),
            models.join(sanitize_component(&descriptor.id.0)),
        )
        .expect("symlink");

        assert!(remove_discovered_cache_artifact(&descriptor, cache.path()).is_err());
        assert!(outside.path().join("keep.bin").exists());
    }
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
        let cache_entry_existed = match Self::discover_cache_artifact(model, &cache_root) {
            Ok(_) => true,
            Err(AdapterError::NotFound) => false,
            Err(error) => return Err(error),
        };
        let downloaded = manager
            .download_model(&model.id.0, None, force)
            .await
            .map_err(|_| AdapterError::DownloadFailed)?;
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
        let artifact = Self::discover_cache_artifact(model, &cache_root)?;
        Ok(InstalledArtifact {
            cache_root,
            artifact_root: artifact.artifact_root,
            model_id: model.id.clone(),
            files: artifact.files,
            created_by_install: !cache_entry_existed,
        })
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
        let cache_location = manager
            .get_cache_location()
            .await
            .map_err(|_| AdapterError::RuntimeFailed)?;
        let cache_root = PathBuf::from(cache_location);
        let artifact = Self::discover_cache_artifact(model, &cache_root)?;
        Ok(InstalledArtifact {
            cache_root,
            artifact_root: artifact.artifact_root,
            model_id: model.id.clone(),
            files: artifact.files,
            created_by_install: false,
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
        let cache_root = PathBuf::from(&cache_location);
        remove_discovered_cache_artifact(model, &cache_root)
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
