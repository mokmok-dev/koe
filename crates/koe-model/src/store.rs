//! Durable model store: immutable manifests, digest allowlist, quarantine.
//!
//! Layout under the app-owned data root:
//!
//! ```text
//! models/
//!   <uuid>/manifest.json
//!   <uuid>/benchmarks.json
//!   quarantine/<uuid>-note.json
//! ```
//!
//! Directory names are koe-generated UUIDs only. The actual model artifacts
//! live in the adapter-owned runtime cache; the store records their digest
//! inventory for verification and license display.

use std::{
    collections::BTreeMap,
    fs,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    FOUNDRY_SDK_VERSION,
    types::{
        InstalledModelId, ModelDescriptor, ModelError, ModelFile, ModelId, ModelManifest,
        ModelSelector, ModelVersion, Verification,
    },
};

const MANIFEST_SCHEMA: u32 = 1;
/// Absolute path of one expected digest for an allowlist.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileDigest {
    pub sha256: String,
    pub size: u64,
}

/// Publisher-managed expected digests keyed by `model_id@version`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DigestAllowlist {
    pub entries: BTreeMap<String, AllowlistEntry>,
}

/// One allowlist entry: every file that must be present with exact digest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AllowlistEntry {
    pub files: BTreeMap<String, FileDigest>,
}

impl DigestAllowlist {
    /// Empty allowlist; all artifacts are verified as `runtime-only`.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Looks up an entry by model id and version.
    #[must_use]
    pub fn entry(
        &self,
        model_id: &ModelId,
        version: &ModelVersion,
    ) -> Option<&AllowlistEntry> {
        self.entries.get(&allowlist_key(model_id, version))
    }

    /// Inserts or replaces an entry for round-trip testing.
    pub fn insert(
        &mut self,
        model_id: &ModelId,
        version: &ModelVersion,
        files: BTreeMap<String, FileDigest>,
    ) {
        self.entries
            .insert(allowlist_key(model_id, version), AllowlistEntry { files });
    }
}

fn allowlist_key(
    model_id: &ModelId,
    version: &ModelVersion,
) -> String {
    format!("{}@{version}", model_id.0.to_ascii_lowercase())
}

/// Persisted verification failure note.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QuarantineNote {
    pub schema_version: u32,
    pub model_id: String,
    pub version: String,
    pub reason: String,
    pub quarantined_at_unix_ms: u128,
}

/// Filesystem-backed model manifest registry.
#[derive(Clone, Debug)]
pub struct ModelStore {
    data_root: PathBuf,
    models_dir: PathBuf,
    quarantine_dir: PathBuf,
    allowlist: DigestAllowlist,
}

impl ModelStore {
    /// Opens or creates the model store under `data_root`.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::PathRejected`] for a symlinked root and
    /// [`ModelError::StoreFailed`] for filesystem failures.
    pub fn open(
        data_root: impl Into<PathBuf>,
        allowlist: DigestAllowlist,
    ) -> Result<Self, ModelError> {
        let data_root = data_root.into();
        let metadata = fs::symlink_metadata(&data_root).map_err(map_store_error)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ModelError::PathRejected);
        }
        let models_dir = data_root.join("models");
        fs::create_dir_all(&models_dir).map_err(map_store_error)?;
        let quarantine_dir = models_dir.join("quarantine");
        fs::create_dir_all(&quarantine_dir).map_err(map_store_error)?;
        Ok(Self {
            data_root,
            models_dir,
            quarantine_dir,
            allowlist,
        })
    }

    /// Publishes an immutable manifest under `id` and returns it.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::StoreFailed`] for filesystem failures.
    pub fn publish_manifest(
        &self,
        id: InstalledModelId,
        descriptor: &ModelDescriptor,
        files: Vec<ModelFile>,
        verification: Verification,
    ) -> Result<InstalledModelId, ModelError> {
        let directory = self.models_dir.join(id.to_string());
        fs::create_dir(&directory).map_err(map_store_error)?;
        let manifest = ModelManifest {
            schema_version: MANIFEST_SCHEMA,
            model_id: descriptor.id.clone(),
            alias: descriptor.alias.clone(),
            version: descriptor.version.clone(),
            variant: descriptor.variant.clone(),
            provider: descriptor.provider.clone(),
            license_id: descriptor.license_id.clone(),
            license_description: descriptor.license_description.clone(),
            source: descriptor.source.clone(),
            files,
            installed_at_unix_ms: unix_millis(),
            foundry_version: FOUNDRY_SDK_VERSION.to_owned(),
            verification,
        };
        let mut serialized = Vec::new();
        serde_json::to_writer_pretty(&mut serialized, &manifest)
            .map_err(|_| ModelError::StoreFailed)?;
        serialized.push(b'\n');
        atomic_write(&directory.join("manifest.json"), &serialized)?;
        Ok(id)
    }

    /// Loads one manifest.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::NotFound`] for unknown ids and
    /// [`ModelError::PathRejected`] for unsafe store entries.
    pub fn load_manifest(
        &self,
        id: &InstalledModelId,
    ) -> Result<ModelManifest, ModelError> {
        let directory = self.models_dir.join(id.to_string());
        let manifest_path = directory.join("manifest.json");
        let metadata = fs::symlink_metadata(&manifest_path).map_err(map_manifest_missing)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ModelError::PathRejected);
        }
        let bytes = fs::read(&manifest_path).map_err(map_store_error)?;
        serde_json::from_slice(&bytes).map_err(|_| ModelError::StoreFailed)
    }

    /// Lists every installed (id, manifest) pair.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::StoreFailed`] for filesystem failures.
    pub fn installed_manifests(
        &self
    ) -> Result<Vec<(InstalledModelId, ModelManifest)>, ModelError> {
        let mut installed = Vec::new();
        for entry in fs::read_dir(&self.models_dir).map_err(map_store_error)? {
            let entry = entry.map_err(map_store_error)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Ok(id) = InstalledModelId::parse(name) else {
                continue;
            };
            let metadata = fs::symlink_metadata(entry.path()).map_err(map_store_error)?;
            if metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && let Ok(manifest) = self.load_manifest(&id)
            {
                installed.push((id, manifest));
            }
        }
        installed.sort_by(|left, right| {
            left.1
                .installed_at_unix_ms
                .cmp(&right.1.installed_at_unix_ms)
                .then_with(|| left.1.model_id.cmp(&right.1.model_id))
        });
        Ok(installed)
    }

    /// Lists every installed manifest.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::StoreFailed`] for filesystem failures.
    pub fn list_manifests(&self) -> Result<Vec<ModelManifest>, ModelError> {
        Ok(self
            .installed_manifests()?
            .into_iter()
            .map(|(_id, manifest)| manifest)
            .collect())
    }

    /// Removes one installed manifest directory.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::NotFound`] for unknown ids and
    /// [`ModelError::PathRejected`] for unsafe entries.
    pub fn remove_manifest(
        &self,
        id: &InstalledModelId,
    ) -> Result<(), ModelError> {
        let directory = self.models_dir.join(id.to_string());
        let metadata = fs::symlink_metadata(&directory).map_err(map_manifest_missing)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ModelError::PathRejected);
        }
        fs::remove_dir_all(&directory).map_err(map_store_error)
    }

    /// Finds the installed id matching a selector key, if any.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::StoreFailed`] for filesystem failures.
    pub fn find_installed(
        &self,
        selector: &ModelSelector,
    ) -> Result<Option<InstalledModelId>, ModelError> {
        let key = selector.key();
        for (id, manifest) in self.installed_manifests()? {
            if manifest.alias.0.to_ascii_lowercase() == key
                || manifest.model_id.0.to_ascii_lowercase() == key
            {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    /// Verifies an artifact inventory against the allowlist.
    ///
    /// A missing allowlist entry yields `RuntimeOnly`. Any digest mismatch or
    /// unknown file quarantines the artifact and returns
    /// [`ModelError::VerifyFailed`].
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::VerifyFailed`] on mismatch and
    /// [`ModelError::StoreFailed`] when the quarantine note cannot be written.
    pub fn verify_inventory(
        &self,
        descriptor: &ModelDescriptor,
        files: &[ModelFile],
    ) -> Result<Verification, ModelError> {
        let Some(expected) = self.allowlist.entry(&descriptor.id, &descriptor.version) else {
            return Ok(Verification::RuntimeOnly);
        };
        let actual_paths = files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let expected_paths = expected
            .files
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        if files.len() != actual_paths.len() || actual_paths != expected_paths {
            self.quarantine(
                descriptor,
                format!(
                    "file set mismatch ({} vs {})",
                    actual_paths.len(),
                    expected_paths.len()
                ),
            )?;
            return Err(ModelError::VerifyFailed);
        }
        for file in files {
            let Some(expected_digest) = expected.files.get(&file.path) else {
                self.quarantine(
                    descriptor,
                    format!("unknown file `{}`", redact_file(&file.path)),
                )?;
                return Err(ModelError::VerifyFailed);
            };
            if file.sha256 != expected_digest.sha256 || file.size != expected_digest.size {
                self.quarantine(
                    descriptor,
                    format!("digest mismatch for `{}`", redact_file(&file.path)),
                )?;
                return Err(ModelError::VerifyFailed);
            }
        }
        Ok(Verification::Verified)
    }

    /// Writes a persisted quarantine note for diagnosability.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::StoreFailed`] when the note cannot be written.
    fn quarantine(
        &self,
        descriptor: &ModelDescriptor,
        reason: String,
    ) -> Result<(), ModelError> {
        let note = QuarantineNote {
            schema_version: 1,
            model_id: descriptor.id.0.clone(),
            version: descriptor.version.0.clone(),
            reason,
            quarantined_at_unix_ms: unix_millis(),
        };
        let mut serialized = Vec::new();
        serde_json::to_writer_pretty(&mut serialized, &note)
            .map_err(|_| ModelError::StoreFailed)?;
        serialized.push(b'\n');
        atomic_write(
            &self.quarantine_dir.join(format!("{}.json", Uuid::new_v4())),
            &serialized,
        )
    }

    /// Saves a benchmark report for one installed model.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::NotFound`] for unknown ids and
    /// [`ModelError::StoreFailed`] for filesystem failures.
    pub fn save_benchmarks(
        &self,
        id: &InstalledModelId,
        report: &crate::benchmark::BenchmarkReport,
    ) -> Result<(), ModelError> {
        let directory = self.models_dir.join(id.to_string());
        if !Self::dir_is_owned_manifest(&directory) {
            return Err(ModelError::NotFound);
        }
        let mut serialized = Vec::new();
        serde_json::to_writer_pretty(&mut serialized, report)
            .map_err(|_| ModelError::StoreFailed)?;
        serialized.push(b'\n');
        atomic_write(&directory.join("benchmarks.json"), &serialized)
    }

    /// Loads the benchmark report for one installed model.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::NotFound`] for unknown ids.
    pub fn load_benchmarks(
        &self,
        id: &InstalledModelId,
    ) -> Result<crate::benchmark::BenchmarkReport, ModelError> {
        let path = self.models_dir.join(id.to_string()).join("benchmarks.json");
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| ModelError::StoreFailed),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(crate::benchmark::BenchmarkReport::default())
            },
            Err(_) => Err(ModelError::StoreFailed),
        }
    }

    /// Lists store manifests as installation-scope descriptors.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::StoreFailed`] for filesystem failures.
    pub fn list_descriptors(&self) -> Result<Vec<ModelDescriptor>, ModelError> {
        Ok(self
            .list_manifests()?
            .into_iter()
            .map(|manifest| descriptor_from_manifest(&manifest))
            .collect())
    }

    fn dir_is_owned_manifest(directory: &Path) -> bool {
        let Ok(metadata) = fs::symlink_metadata(directory) else {
            return false;
        };
        metadata.is_dir() && !metadata.file_type().is_symlink()
    }

    /// Raw data root for tests.
    #[must_use]
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }
}

fn descriptor_from_manifest(manifest: &ModelManifest) -> ModelDescriptor {
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

fn atomic_write(
    path: &Path,
    bytes: &[u8],
) -> Result<(), ModelError> {
    let parent = path.parent().ok_or(ModelError::PathRejected)?;
    let file_name = path.file_name().ok_or(ModelError::PathRejected)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        Uuid::new_v4()
    ));
    let result = (|| -> Result<(), ModelError> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        let file = options.open(&temporary).map_err(map_store_error)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(bytes).map_err(map_store_error)?;
        writer.flush().map_err(map_store_error)?;
        drop(writer);
        Ok(())
    })();
    if let Err(error) = result {
        let _ignored = fs::remove_file(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, path).map_err(map_store_error)
}

#[allow(clippy::needless_pass_by_value)]
fn map_store_error(error: std::io::Error) -> ModelError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ModelError::NotFound,
        _ => ModelError::StoreFailed,
    }
}

#[allow(clippy::needless_pass_by_value)]
fn map_manifest_missing(error: std::io::Error) -> ModelError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ModelError::NotFound
    } else {
        ModelError::StoreFailed
    }
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

const fn redact_file(path: &str) -> &str {
    // File names are model metadata, not user content, but keep display
    // minimal to avoid surprising path leaks.
    path
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::TempDir;

    use super::{DigestAllowlist, FileDigest, ModelStore};
    use crate::{Alias, ModelDescriptor, ModelId, ModelSelector, ModelVersion, Verification};

    fn descriptor(version: &str) -> ModelDescriptor {
        ModelDescriptor {
            id: ModelId::new("FixtureLocal/NemotronASRStreaming0.6B".to_owned()),
            alias: Alias("fixture-nemotron-asr-0.6b".to_owned()),
            version: ModelVersion::new(version.to_owned()),
            variant: "cpu".to_owned(),
            provider: "FixtureLocal".to_owned(),
            license_id: "fixture-license-apache-2.0".to_owned(),
            license_description: "Fixture license".to_owned(),
            source: "fixture://catalog".to_owned(),
            size_mb: 1,
            task: "automatic-speech-recognition".to_owned(),
        }
    }

    #[test]
    fn publishes_and_lists_manifest_round_trip() {
        let root = TempDir::new().expect("temp");
        let store = ModelStore::open(root.path(), DigestAllowlist::empty()).expect("store");
        let id = store
            .publish_manifest(
                crate::InstalledModelId::new(),
                &descriptor("1.0"),
                Vec::new(),
                Verification::RuntimeOnly,
            )
            .expect("publish");
        let manifests = store.list_manifests().expect("list");
        assert_eq!(manifests.len(), 1);
        assert_eq!(store.load_manifest(&id).expect("load"), manifests[0]);
    }

    #[test]
    fn allowlist_mismatch_quarantines_and_fails() {
        let root = TempDir::new().expect("temp");
        let mut allowlist = DigestAllowlist::empty();
        let model = descriptor("1.0");
        let mut files = BTreeMap::new();
        files.insert(
            "models/x/model.bin".to_owned(),
            FileDigest {
                sha256: "deadbeef".to_owned(),
                size: 1,
            },
        );
        allowlist.insert(&model.id, &model.version, files);
        let store = ModelStore::open(root.path(), allowlist).expect("store");
        let verification = store
            .verify_inventory(
                &model,
                &[crate::ModelFile {
                    path: "models/x/model.bin".to_owned(),
                    sha256: "deadbeef".to_owned(),
                    size: 2,
                }],
            )
            .expect_err("mismatch must fail");
        assert_eq!(verification, crate::ModelError::VerifyFailed);
    }

    #[test]
    fn allowlist_match_is_verified() {
        let root = TempDir::new().expect("temp");
        let mut allowlist = DigestAllowlist::empty();
        let model = descriptor("1.0");
        let mut files = BTreeMap::new();
        files.insert(
            "model.bin".to_owned(),
            FileDigest {
                sha256: "abc123".to_owned(),
                size: 2,
            },
        );
        allowlist.insert(&model.id, &model.version, files);
        let store = ModelStore::open(root.path(), allowlist).expect("store");
        let verification = store
            .verify_inventory(
                &model,
                &[crate::ModelFile {
                    path: "model.bin".to_owned(),
                    sha256: "abc123".to_owned(),
                    size: 2,
                }],
            )
            .expect("verified");
        assert_eq!(verification, Verification::Verified);
    }

    #[test]
    fn unknown_files_fail_verification() {
        let root = TempDir::new().expect("temp");
        let model = descriptor("1.0");
        let mut allowlist = DigestAllowlist::empty();
        let mut files = BTreeMap::new();
        files.insert(
            "model.bin".to_owned(),
            FileDigest {
                sha256: "expected".to_owned(),
                size: 1,
            },
        );
        allowlist.insert(&model.id, &model.version, files);
        let store = ModelStore::open(root.path(), allowlist).expect("store");
        let verification = store
            .verify_inventory(
                &model,
                &[crate::ModelFile {
                    path: "other.bin".to_owned(),
                    sha256: "x".to_owned(),
                    size: 1,
                }],
            )
            .expect_err("unknown file must fail");
        assert_eq!(verification, crate::ModelError::VerifyFailed);
    }

    #[test]
    fn remove_manifest_clears_the_directory() {
        let root = TempDir::new().expect("temp");
        let store = ModelStore::open(root.path(), DigestAllowlist::empty()).expect("store");
        let id = store
            .publish_manifest(
                crate::InstalledModelId::new(),
                &descriptor("1.0"),
                Vec::new(),
                Verification::RuntimeOnly,
            )
            .expect("publish");
        store.remove_manifest(&id).expect("remove");
        assert_eq!(
            store.load_manifest(&id).expect_err("must be gone"),
            crate::ModelError::NotFound
        );
    }

    #[test]
    fn installed_selector_resolves_from_manifests() {
        let root = TempDir::new().expect("temp");
        let store = ModelStore::open(root.path(), DigestAllowlist::empty()).expect("store");
        store
            .publish_manifest(
                crate::InstalledModelId::new(),
                &descriptor("1.0"),
                Vec::new(),
                Verification::RuntimeOnly,
            )
            .expect("publish");
        let id = store
            .find_installed(&ModelSelector::Alias(
                "fixture-nemotron-asr-0.6b".to_owned(),
            ))
            .expect("find")
            .expect("found");
        let manifest = store.load_manifest(&id).expect("load");
        assert_eq!(manifest.alias.0, "fixture-nemotron-asr-0.6b");
    }

    #[test]
    fn descriptors_derive_from_manifests() {
        let root = TempDir::new().expect("temp");
        let store = ModelStore::open(root.path(), DigestAllowlist::empty()).expect("store");
        store
            .publish_manifest(
                crate::InstalledModelId::new(),
                &descriptor("1.0"),
                Vec::new(),
                Verification::RuntimeOnly,
            )
            .expect("publish");
        let descriptors = store.list_descriptors().expect("list");
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].version.0, "1.0");
    }
}
