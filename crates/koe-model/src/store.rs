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
    fs::{self, File},
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    FOUNDRY_SDK_VERSION,
    types::{
        InstalledModelId, ManifestValidationError, ModelDescriptor, ModelError, ModelFile, ModelId,
        ModelManifest, ModelSelector, ModelVersion, Verification,
    },
};

const MANIFEST_SCHEMA: u32 = 1;
/// Maximum serialized manifest size (4 MiB).
pub const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_BENCHMARK_BYTES: u64 = 4 * 1024 * 1024;
/// Maximum bytes in required manifest identity text fields (16 KiB).
pub const MAX_MANIFEST_TEXT_BYTES: usize = 16 * 1024;
/// Maximum bytes in one manifest inventory path (16 KiB).
pub const MAX_MANIFEST_PATH_BYTES: usize = 16 * 1024;
/// Maximum files recorded by one manifest.
pub const MAX_MANIFEST_FILES: usize = 4_096;
/// Expected digest for one cache-root-relative artifact path.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "FileDigestWire")]
pub struct FileDigest {
    /// Exactly 64 lowercase hexadecimal SHA-256 characters.
    sha256: String,
    size: u64,
}

#[derive(Deserialize)]
struct FileDigestWire {
    sha256: String,
    size: u64,
}

impl TryFrom<FileDigestWire> for FileDigest {
    type Error = String;

    fn try_from(value: FileDigestWire) -> Result<Self, Self::Error> {
        Self::try_new(value.sha256, value.size)
            .map_err(|_| "SHA-256 must be 64 lowercase hexadecimal characters".to_owned())
    }
}

impl FileDigest {
    /// Creates a validated expected digest.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::InvalidDigest`] unless `sha256` is exactly 64
    /// lowercase hexadecimal characters.
    pub fn try_new(
        sha256: impl Into<String>,
        size: u64,
    ) -> Result<Self, ModelError> {
        let sha256 = sha256.into();
        if sha256.len() != 64
            || !sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(ModelError::InvalidDigest);
        }
        Ok(Self { sha256, size })
    }

    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }
}

/// Publisher-managed expected digests keyed by `model_id@version`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DigestAllowlist {
    pub entries: BTreeMap<String, AllowlistEntry>,
}

/// One allowlist entry: every file that must be present with exact digest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AllowlistEntry {
    /// Digests keyed by the matching [`crate::InstalledFile::relative_path`].
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

    /// Inserts or replaces an entry keyed by cache-root-relative artifact paths.
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

struct LimitedManifestWriter {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl Write for LimitedManifestWriter {
    fn write(
        &mut self,
        buffer: &[u8],
    ) -> std::io::Result<usize> {
        let limit = usize::try_from(MAX_MANIFEST_BYTES)
            .unwrap_or(usize::MAX)
            .saturating_sub(1);
        if buffer.len() > limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "manifest too large",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialize_manifest(manifest: &ModelManifest) -> Result<Vec<u8>, ModelError> {
    let mut writer = LimitedManifestWriter {
        bytes: Vec::with_capacity(8 * 1024),
        exceeded: false,
    };
    if serde_json::to_writer_pretty(&mut writer, manifest).is_err() {
        return Err(if writer.exceeded {
            ModelError::InvalidManifest(ManifestValidationError::SerializedSizeLimit)
        } else {
            ModelError::StoreFailed
        });
    }
    writer.bytes.push(b'\n');
    Ok(writer.bytes)
}

fn validate_manifest(manifest: &ModelManifest) -> Result<(), ManifestValidationError> {
    fn valid_text(value: &str) -> bool {
        !value.is_empty()
            && value.len() <= MAX_MANIFEST_TEXT_BYTES
            && !value.chars().any(char::is_control)
    }

    if manifest.schema_version != MANIFEST_SCHEMA
        || !valid_text(&manifest.model_id.0)
        || !valid_text(&manifest.alias.0)
        || !valid_text(&manifest.version.0)
        || !valid_text(&manifest.variant)
        || !valid_text(&manifest.provider)
        || !valid_text(&manifest.license_id)
        || !valid_text(&manifest.license_description)
        || !valid_text(&manifest.source)
        || !valid_text(&manifest.foundry_version)
        || manifest.cache_directory.as_deref().is_some_and(|path| {
            path.is_empty()
                || path.len() > MAX_MANIFEST_PATH_BYTES
                || path.chars().any(char::is_control)
                || cfg!(windows) && path.contains(':')
                || path
                    .split(['/', '\\'])
                    .any(|component| matches!(component, "" | "." | ".."))
        })
    {
        return Err(ManifestValidationError::InvalidIdentity);
    }
    if manifest.files.is_empty() {
        return Err(ManifestValidationError::EmptyInventory);
    }
    if manifest.files.len() > MAX_MANIFEST_FILES {
        return Err(ManifestValidationError::TooManyFiles);
    }
    let mut total_size = 0_u64;
    if manifest.cache_directory.is_some()
        && manifest.cache_directory
            != common_inventory_directory(&manifest.files, &manifest.model_id.0, &manifest.alias.0)
    {
        return Err(ManifestValidationError::InvalidIdentity);
    }
    let mut paths = std::collections::BTreeSet::new();
    for file in &manifest.files {
        let valid_path = !file.path.is_empty()
            && file.path.len() <= MAX_MANIFEST_PATH_BYTES
            && !file.path.chars().any(char::is_control)
            && !file.path.starts_with('/')
            && !file.path.starts_with('\\')
            && file.path.as_bytes().get(1) != Some(&b':')
            && !(cfg!(windows) && file.path.contains(':'))
            && file
                .path
                .split(['/', '\\'])
                .all(|component| !matches!(component, "" | "." | ".."))
            && paths.insert(file.path.as_str());
        if !valid_path {
            return Err(ManifestValidationError::InvalidPath);
        }
        let valid_digest = file.sha256.len() == 64
            && file
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
        if !valid_digest {
            return Err(ManifestValidationError::InvalidDigest);
        }
        total_size = total_size
            .checked_add(file.size)
            .ok_or(ManifestValidationError::ArtifactSizeLimit)?;
        if total_size > crate::MAX_ARTIFACT_INVENTORY_BYTES {
            return Err(ManifestValidationError::ArtifactSizeLimit);
        }
    }
    Ok(())
}

fn common_inventory_directory(
    files: &[ModelFile],
    model_id: &str,
    alias: &str,
) -> Option<String> {
    let mut common = files.first()?.path.split('/').collect::<Vec<_>>();
    common.pop();
    for file in &files[1..] {
        let mut directory = file.path.split('/').collect::<Vec<_>>();
        directory.pop();
        let shared = common
            .iter()
            .zip(directory.iter())
            .take_while(|(left, right)| left == right)
            .count();
        common.truncate(shared);
    }
    let model_id = sanitize_cache_component(model_id);
    let alias = sanitize_cache_component(alias);
    let index = common.iter().rposition(|component| {
        component.eq_ignore_ascii_case(&model_id) || component.eq_ignore_ascii_case(&alias)
    })?;
    common.truncate(index + 1);
    Some(common.join("/"))
}

fn sanitize_cache_component(value: &str) -> String {
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

/// One per-entry result from diagnostic installed-model enumeration.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum InstalledManifestEntry {
    Valid {
        id: InstalledModelId,
        manifest: Box<ModelManifest>,
    },
    Corrupt {
        id: InstalledModelId,
    },
}

/// Filesystem-backed model manifest registry.
#[derive(Clone, Debug)]
pub struct ModelStore {
    _lock_file: Arc<File>,
    models_handle: Arc<cap_std::fs::Dir>,
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
    /// The returned store and all of its clones hold an exclusive
    /// interprocess lock for `data_root`; independent opens fail with
    /// [`ModelError::StoreLocked`]. Returns [`ModelError::PathRejected`] for a
    /// symlinked root and [`ModelError::StoreFailed`] for filesystem failures.
    pub fn open(
        data_root: impl Into<PathBuf>,
        allowlist: DigestAllowlist,
    ) -> Result<Self, ModelError> {
        let data_root = data_root.into();
        let metadata = fs::symlink_metadata(&data_root).map_err(map_store_error)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ModelError::PathRejected);
        }
        let lock_path = data_root.join(".koe-model.lock");
        if matches!(fs::symlink_metadata(&lock_path), Ok(metadata) if metadata.file_type().is_symlink())
        {
            return Err(ModelError::PathRejected);
        }
        let mut lock_options = fs::OpenOptions::new();
        lock_options.read(true).write(true).create(true);
        set_no_follow(&mut lock_options);
        let lock_file = lock_options.open(&lock_path).map_err(map_store_error)?;
        fs2::FileExt::try_lock_exclusive(&lock_file).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                ModelError::StoreLocked
            } else {
                ModelError::StoreFailed
            }
        })?;
        let data_handle =
            cap_std::fs::Dir::open_ambient_dir(&data_root, cap_std::ambient_authority())
                .map_err(map_store_error)?;
        match data_handle.create_dir("models") {
            Ok(()) => {},
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {},
            Err(error) => return Err(map_store_error(error)),
        }
        let models_metadata = data_handle
            .symlink_metadata("models")
            .map_err(map_store_error)?;
        if models_metadata.file_type().is_symlink() || !models_metadata.is_dir() {
            return Err(ModelError::PathRejected);
        }
        let models_handle = {
            use cap_fs_ext::DirExt;
            data_handle
                .open_dir_nofollow("models")
                .map_err(map_store_error)?
        };
        match models_handle.create_dir("quarantine") {
            Ok(()) => {},
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {},
            Err(error) => return Err(map_store_error(error)),
        }
        let quarantine_metadata = models_handle
            .symlink_metadata("quarantine")
            .map_err(map_store_error)?;
        if quarantine_metadata.file_type().is_symlink() || !quarantine_metadata.is_dir() {
            return Err(ModelError::PathRejected);
        }
        let models_dir = data_root.join("models");
        let quarantine_dir = models_dir.join("quarantine");
        recover_staged_operations(&models_handle)?;
        Ok(Self {
            _lock_file: Arc::new(lock_file),
            models_handle: Arc::new(models_handle),
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
    /// Returns [`ModelError::InvalidManifest`] with a reason unless descriptor text is nonempty,
    /// bounded, and control-free and the inventory is nonempty, within public
    /// byte/manifest limits, uses unique traversal-free bounded paths, and has
    /// lowercase 64-character SHA-256 digests. Returns
    /// [`ModelError::StoreFailed`] for filesystem failures.
    pub fn publish_manifest(
        &self,
        id: InstalledModelId,
        descriptor: &ModelDescriptor,
        files: Vec<ModelFile>,
        verification: Verification,
    ) -> Result<InstalledModelId, ModelError> {
        let directory_name = id.to_string();
        let staging_name = format!(".install-{id}");
        self.models_handle
            .create_dir(&staging_name)
            .map_err(map_store_error)?;
        let staging_handle = self
            .models_handle
            .open_dir(&staging_name)
            .map_err(map_store_error)?;
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
            cache_directory: common_inventory_directory(
                &files,
                &descriptor.id.0,
                &descriptor.alias.0,
            ),
            files,
            installed_at_unix_ms: unix_millis(),
            foundry_version: FOUNDRY_SDK_VERSION.to_owned(),
            verification,
        };
        if let Err(reason) = validate_manifest(&manifest) {
            let _ignored = self.models_handle.remove_dir_all(&staging_name);
            return Err(ModelError::InvalidManifest(reason));
        }
        let serialized = match serialize_manifest(&manifest) {
            Ok(serialized) => serialized,
            Err(error) => {
                let _ignored = self.models_handle.remove_dir_all(&staging_name);
                return Err(error);
            },
        };
        if let Err(error) = atomic_write_cap(&staging_handle, "manifest.json", &serialized) {
            let _ignored = self.models_handle.remove_dir_all(&staging_name);
            return Err(error);
        }
        if let Err(error) = self
            .models_handle
            .rename(&staging_name, &self.models_handle, &directory_name)
            .map_err(map_store_error)
        {
            let _ignored = self.models_handle.remove_dir_all(&staging_name);
            return Err(error);
        }
        if let Err(error) = sync_directory(&self.models_dir) {
            let _ignored = self.models_handle.remove_dir_all(&directory_name);
            let _ignored = sync_directory(&self.models_dir);
            return Err(error);
        }
        Ok(id)
    }

    pub(crate) fn update_manifest_inventory(
        &self,
        id: &InstalledModelId,
        files: Vec<ModelFile>,
        verification: Verification,
    ) -> Result<ModelManifest, ModelError> {
        let mut manifest = self.load_manifest(id)?;
        manifest.cache_directory =
            common_inventory_directory(&files, &manifest.model_id.0, &manifest.alias.0);
        manifest.files = files;
        manifest.verification = verification;
        validate_manifest(&manifest).map_err(ModelError::InvalidManifest)?;
        let serialized = serialize_manifest(&manifest)?;
        let directory = {
            use cap_fs_ext::DirExt;
            self.models_handle
                .open_dir_nofollow(id.to_string())
                .map_err(map_store_error)?
        };
        atomic_write_store_file(
            &directory,
            &self.models_dir.join(id.to_string()).join("manifest.json"),
            "manifest.json",
            &serialized,
        )?;
        Ok(manifest)
    }

    /// Loads one manifest.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::NotFound`] for unknown ids,
    /// [`ModelError::PathRejected`] for unsafe store entries, and
    /// [`ModelError::CorruptManifest`] with the offending installation id when
    /// JSON validation fails.
    pub fn load_manifest(
        &self,
        id: &InstalledModelId,
    ) -> Result<ModelManifest, ModelError> {
        let directory_name = id.to_string();
        let directory_metadata = self
            .models_handle
            .symlink_metadata(&directory_name)
            .map_err(map_manifest_missing)?;
        if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
            return Err(ModelError::PathRejected);
        }
        let directory = {
            use cap_fs_ext::DirExt;
            self.models_handle
                .open_dir_nofollow(&directory_name)
                .map_err(map_manifest_missing)?
        };
        let metadata = match directory.symlink_metadata("manifest.json") {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ModelError::CorruptManifest(*id));
            },
            Err(error) => return Err(map_manifest_missing(error)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ModelError::PathRejected);
        }
        if metadata.len() > MAX_MANIFEST_BYTES {
            return Err(ModelError::CorruptManifest(*id));
        }
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        {
            use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
            options.follow(FollowSymlinks::No);
        }
        let file = directory
            .open_with("manifest.json", &options)
            .map_err(map_store_error)?;
        if !file.metadata().map_err(map_store_error)?.is_file() {
            return Err(ModelError::PathRejected);
        }
        let mut bytes = Vec::new();
        file.take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(map_store_error)?;
        if u64::try_from(bytes.len()).map_err(|_| ModelError::CorruptManifest(*id))?
            > MAX_MANIFEST_BYTES
        {
            return Err(ModelError::CorruptManifest(*id));
        }
        let manifest = serde_json::from_slice::<ModelManifest>(&bytes)
            .map_err(|_| ModelError::CorruptManifest(*id))?;
        if validate_manifest(&manifest).is_err() {
            return Err(ModelError::CorruptManifest(*id));
        }
        Ok(manifest)
    }

    /// Removes an installation only when its manifest is corrupt.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::NotCorrupt`] when the manifest is valid,
    /// [`ModelError::NotFound`] for an unknown id, and store/path errors when
    /// the entry cannot be validated or removed safely.
    pub fn remove_corrupt_manifest(
        &self,
        id: &InstalledModelId,
    ) -> Result<(), ModelError> {
        match self.load_manifest(id) {
            Err(ModelError::CorruptManifest(corrupt)) if corrupt == *id => self.remove_manifest(id),
            Ok(_) => Err(ModelError::NotCorrupt),
            Err(error) => Err(error),
        }
    }

    /// Diagnoses every installed entry without hiding healthy manifests when
    /// another entry is corrupt.
    ///
    /// # Errors
    ///
    /// Returns path/store errors that cannot be attributed to one malformed
    /// manifest.
    pub fn inspect_installed_manifests(&self) -> Result<Vec<InstalledManifestEntry>, ModelError> {
        let mut entries = Vec::new();
        for entry in self.models_handle.entries().map_err(map_store_error)? {
            let entry = entry.map_err(map_store_error)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Ok(id) = InstalledModelId::parse(name) else {
                continue;
            };
            let file_type = entry.file_type().map_err(map_store_error)?;
            if file_type.is_symlink() || !file_type.is_dir() {
                return Err(ModelError::PathRejected);
            }
            match self.load_manifest(&id) {
                Ok(manifest) => entries.push(InstalledManifestEntry::Valid {
                    id,
                    manifest: Box::new(manifest),
                }),
                Err(ModelError::CorruptManifest(_)) => {
                    entries.push(InstalledManifestEntry::Corrupt { id });
                },
                Err(error) => return Err(error),
            }
        }
        entries.sort_by_key(|entry| match entry {
            InstalledManifestEntry::Valid { id, .. } | InstalledManifestEntry::Corrupt { id } => {
                *id
            },
        });
        Ok(entries)
    }

    /// Lists every installed (id, manifest) pair.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::PathRejected`] for unsafe UUID-named entries,
    /// [`ModelError::CorruptManifest`] with the offending id for malformed
    /// manifests, and [`ModelError::StoreFailed`] for filesystem failures.
    pub fn installed_manifests(
        &self
    ) -> Result<Vec<(InstalledModelId, ModelManifest)>, ModelError> {
        let mut installed = Vec::new();
        for entry in self.models_handle.entries().map_err(map_store_error)? {
            let entry = entry.map_err(map_store_error)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Ok(id) = InstalledModelId::parse(name) else {
                continue;
            };
            let file_type = entry.file_type().map_err(map_store_error)?;
            if file_type.is_symlink() || !file_type.is_dir() {
                return Err(ModelError::PathRejected);
            }
            installed.push((id, self.load_manifest(&id)?));
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
    /// Returns [`ModelError::PathRejected`] for unsafe UUID-named entries,
    /// [`ModelError::CorruptManifest`] with the offending id for malformed
    /// manifests, and [`ModelError::StoreFailed`] for filesystem failures.
    pub fn list_manifests(&self) -> Result<Vec<ModelManifest>, ModelError> {
        Ok(self
            .installed_manifests()?
            .into_iter()
            .map(|(_id, manifest)| manifest)
            .collect())
    }

    pub(crate) fn stage_removal(
        &self,
        id: &InstalledModelId,
    ) -> Result<PathBuf, ModelError> {
        let directory_name = id.to_string();
        let metadata = self
            .models_handle
            .symlink_metadata(&directory_name)
            .map_err(map_manifest_missing)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ModelError::PathRejected);
        }
        let staged_name = format!(".remove-{id}");
        self.models_handle
            .rename(&directory_name, &self.models_handle, &staged_name)
            .map_err(map_store_error)?;
        if let Err(error) = sync_directory(&self.models_dir) {
            let _ignored =
                self.models_handle
                    .rename(&staged_name, &self.models_handle, &directory_name);
            let _ignored = sync_directory(&self.models_dir);
            return Err(error);
        }
        Ok(self.models_dir.join(staged_name))
    }

    pub(crate) fn commit_removal(
        &self,
        staged: &Path,
    ) -> Result<(), ModelError> {
        let name = staged.file_name().ok_or(ModelError::PathRejected)?;
        self.models_handle
            .remove_dir_all(name)
            .map_err(map_store_error)?;
        sync_directory(&self.models_dir)
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
        let directory_name = id.to_string();
        let metadata = self
            .models_handle
            .symlink_metadata(&directory_name)
            .map_err(map_manifest_missing)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ModelError::PathRejected);
        }
        self.models_handle
            .remove_dir_all(&directory_name)
            .map_err(map_store_error)?;
        sync_directory(&self.models_dir)
    }

    /// Finds the installed id matching a selector key, if any.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::CorruptManifest`] for a repairable entry,
    /// [`ModelError::PathRejected`] for unsafe entries, or
    /// [`ModelError::StoreFailed`] for filesystem failures.
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
        let mut actual_paths = std::collections::BTreeSet::new();
        for file in files {
            if !actual_paths.insert(file.path.as_str()) {
                self.quarantine(
                    descriptor,
                    format!("duplicate file `{}`", redact_file(&file.path)),
                )?;
                return Err(ModelError::VerifyFailed);
            }
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
        let directory = {
            use cap_fs_ext::DirExt;
            self.models_handle
                .open_dir_nofollow(id.to_string())
                .map_err(map_manifest_missing)?
        };
        let mut serialized = Vec::new();
        serde_json::to_writer_pretty(&mut serialized, report)
            .map_err(|_| ModelError::StoreFailed)?;
        serialized.push(b'\n');
        if u64::try_from(serialized.len()).map_err(|_| ModelError::StoreFailed)?
            > MAX_BENCHMARK_BYTES
        {
            return Err(ModelError::StoreFailed);
        }
        atomic_write_store_file(
            &directory,
            &self.models_dir.join(id.to_string()).join("benchmarks.json"),
            "benchmarks.json",
            &serialized,
        )
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
        let directory = {
            use cap_fs_ext::DirExt;
            self.models_handle
                .open_dir_nofollow(id.to_string())
                .map_err(map_manifest_missing)?
        };
        let metadata = match directory.symlink_metadata("benchmarks.json") {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(crate::benchmark::BenchmarkReport::default());
            },
            Err(_) => return Err(ModelError::StoreFailed),
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_BENCHMARK_BYTES
        {
            return Err(ModelError::PathRejected);
        }
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        {
            use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
            options.follow(FollowSymlinks::No);
        }
        let file = directory
            .open_with("benchmarks.json", &options)
            .map_err(map_store_error)?;
        let mut bytes = Vec::new();
        file.take(MAX_BENCHMARK_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(map_store_error)?;
        if u64::try_from(bytes.len()).map_err(|_| ModelError::StoreFailed)? > MAX_BENCHMARK_BYTES {
            return Err(ModelError::StoreFailed);
        }
        serde_json::from_slice(&bytes).map_err(|_| ModelError::StoreFailed)
    }

    /// Lists store manifests as installation-scope descriptors.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::CorruptManifest`] for a repairable entry,
    /// [`ModelError::PathRejected`] for unsafe paths, or
    /// [`ModelError::StoreFailed`] for filesystem failures. Use
    /// [`Self::inspect_installed_manifests`] to retain healthy entries and
    /// [`Self::remove_corrupt_manifest`] to repair by installation id.
    pub fn list_descriptors(&self) -> Result<Vec<ModelDescriptor>, ModelError> {
        Ok(self
            .list_manifests()?
            .into_iter()
            .map(|manifest| descriptor_from_manifest(&manifest))
            .collect())
    }

    /// Raw data root for tests.
    #[must_use]
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }
}

fn recover_staged_operations(models_dir: &cap_std::fs::Dir) -> Result<(), ModelError> {
    let mut changed = false;
    for entry in models_dir.entries().map_err(map_store_error)? {
        let entry = entry.map_err(map_store_error)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let id = if let Some(id) = name.strip_prefix(".remove-") {
            id
        } else if let Some(id) = name.strip_prefix(".install-") {
            id
        } else {
            continue;
        };
        if InstalledModelId::parse(id).is_err() {
            continue;
        }
        let file_type = entry.file_type().map_err(map_store_error)?;
        if file_type.is_symlink() || !file_type.is_dir() {
            return Err(ModelError::PathRejected);
        }
        models_dir.remove_dir_all(name).map_err(map_store_error)?;
        changed = true;
    }
    if changed {
        models_dir
            .open(".")
            .and_then(|file| file.sync_all())
            .map_err(map_store_error)?;
    }
    Ok(())
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

fn atomic_write_store_file(
    directory: &cap_std::fs::Dir,
    ambient_path: &Path,
    file_name: &str,
    bytes: &[u8],
) -> Result<(), ModelError> {
    #[cfg(windows)]
    {
        let _ = (directory, file_name);
        atomic_write(ambient_path, bytes)
    }
    #[cfg(not(windows))]
    {
        let _ = ambient_path;
        atomic_write_cap(directory, file_name, bytes)
    }
}

fn atomic_write_cap(
    directory: &cap_std::fs::Dir,
    file_name: &str,
    bytes: &[u8],
) -> Result<(), ModelError> {
    let temporary = format!(".{file_name}.{}.tmp", Uuid::new_v4());
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let file = directory
        .open_with(&temporary, &options)
        .map_err(map_store_error)?;
    let mut writer = std::io::BufWriter::new(file);
    let result = (|| {
        writer.write_all(bytes).map_err(map_store_error)?;
        writer.flush().map_err(map_store_error)?;
        writer.get_ref().sync_all().map_err(map_store_error)
    })();
    drop(writer);
    if let Err(error) = result {
        let _ignored = directory.remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = directory
        .rename(&temporary, directory, file_name)
        .map_err(map_store_error)
    {
        let _ignored = directory.remove_file(&temporary);
        return Err(error);
    }
    #[cfg(unix)]
    {
        directory
            .open(".")
            .and_then(|file| file.sync_all())
            .map_err(map_store_error)
    }
    #[cfg(not(unix))]
    {
        Ok(())
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
        writer.get_ref().sync_all().map_err(map_store_error)?;
        drop(writer);
        Ok(())
    })();
    if let Err(error) = result {
        let _ignored = fs::remove_file(&temporary);
        return Err(error);
    }
    replace_file(&temporary, path)?;
    sync_directory(parent)
}

#[cfg(not(windows))]
fn replace_file(
    source: &Path,
    destination: &Path,
) -> Result<(), ModelError> {
    fs::rename(source, destination).map_err(map_store_error)
}

#[cfg(windows)]
fn replace_file(
    source: &Path,
    destination: &Path,
) -> Result<(), ModelError> {
    use atomicwrites::{AllowOverwrite, AtomicFile};
    let atomic = AtomicFile::new(destination, AllowOverwrite);
    atomic
        .write(|output| {
            let mut input = File::open(source)?;
            std::io::copy(&mut input, output)?;
            Ok::<(), std::io::Error>(())
        })
        .map_err(|_| ModelError::StoreFailed)?;
    fs::remove_file(source).map_err(map_store_error)
}

fn set_no_follow(options: &mut fs::OpenOptions) {
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    #[cfg(windows)]
    options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
}

fn sync_directory(path: &Path) -> Result<(), ModelError> {
    #[cfg(not(unix))]
    let _ = path;
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(map_store_error)?;
    Ok(())
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

    fn files() -> Vec<crate::ModelFile> {
        vec![crate::ModelFile {
            path: "models/model.bin".to_owned(),
            sha256: "0".repeat(64),
            size: 1,
        }]
    }

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
                files(),
                Verification::RuntimeOnly,
            )
            .expect("publish");
        let manifests = store.list_manifests().expect("list");
        assert_eq!(manifests.len(), 1);
        assert_eq!(store.load_manifest(&id).expect("load"), manifests[0]);
    }

    #[test]
    fn corrupt_manifest_is_reported_during_enumeration() {
        let root = TempDir::new().expect("temp");
        let store = ModelStore::open(root.path(), DigestAllowlist::empty()).expect("store");
        let id = store
            .publish_manifest(
                crate::InstalledModelId::new(),
                &descriptor("1.0"),
                files(),
                Verification::RuntimeOnly,
            )
            .expect("publish");
        std::fs::write(
            root.path()
                .join("models")
                .join(id.to_string())
                .join("manifest.json"),
            b"not json\n",
        )
        .expect("corrupt manifest");
        assert_eq!(
            store.installed_manifests(),
            Err(crate::ModelError::CorruptManifest(id))
        );
        store
            .remove_corrupt_manifest(&id)
            .expect("explicit corruption repair");
        assert!(
            store
                .installed_manifests()
                .expect("list repaired")
                .is_empty()
        );
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
                files(),
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
                files(),
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
                files(),
                Verification::RuntimeOnly,
            )
            .expect("publish");
        let descriptors = store.list_descriptors().expect("list");
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].version.0, "1.0");
    }
}
