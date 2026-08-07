//! Durable, serialized side-by-side executable updates and rollback.
//!
//! `state.json` is the only authoritative commit record. A version directory
//! is published first and may be safely reused by a retry; activation and the
//! replay watermark are then committed together by one atomic replacement.

use std::{
    collections::BTreeSet,
    fs,
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::Builder;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt as WindowsMetadataExt, OpenOptionsExt as _};

use crate::{
    signing::{hex_encode, verify_signature},
    types::{
        MAX_METADATA_BYTES, MAX_TARGET_SIZE, MAX_TARGETS, SignedUpdate, TARGETS_ROLE, UpdateError,
        UpdateMetadata, UpdateState, UpdateStatus, UpdateTarget,
    },
};

const STATE_SCHEMA: u32 = 2;
const INSTALLED_SCHEMA: u32 = 1;
const ARTIFACT_FILE: &str = "artifact";
const METADATA_FILE: &str = "metadata.json";
const STATE_FILE: &str = "state.json";
const LOCK_FILE: &str = ".lock";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct InstalledVersion {
    schema_version: u32,
    /// Publisher metadata for downloaded updates. `None` is reserved for the
    /// executable that was already running when the store was initialized.
    signed: Option<SignedUpdate>,
    app_version: String,
    target: UpdateTarget,
}

/// Persisted rejection note for update diagnostics. It contains no paths,
/// keys, metadata, or artifact content.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantineNote {
    pub schema_version: u32,
    pub reason: String,
    pub quarantined_at_unix_ms: u128,
}

/// Filesystem-backed update store.
#[derive(Clone, Debug)]
pub struct UpdateStore {
    versions_dir: PathBuf,
    quarantine_dir: PathBuf,
    state_path: PathBuf,
    lock_path: PathBuf,
}

impl UpdateStore {
    /// Opens or creates a store under a real, app-owned data root.
    ///
    /// Incomplete random staging directories are removed on open. Published
    /// version directories are retained because an interrupted install can
    /// validate and activate them on retry.
    ///
    /// # Errors
    ///
    /// Returns a stable path/store error for unsafe or inaccessible storage.
    pub fn open(data_root: impl Into<PathBuf>) -> Result<Self, UpdateError> {
        let data_root = data_root.into();
        require_real_directory(&data_root)?;
        let updates_dir = data_root.join("updates");
        ensure_real_directory(&updates_dir)?;
        let versions_dir = updates_dir.join("versions");
        ensure_real_directory(&versions_dir)?;
        let quarantine_dir = updates_dir.join("quarantine");
        ensure_real_directory(&quarantine_dir)?;
        let store = Self {
            state_path: updates_dir.join(STATE_FILE),
            lock_path: updates_dir.join(LOCK_FILE),
            versions_dir,
            quarantine_dir,
        };
        let _lock = store.exclusive_lock()?;
        store.recover_staging()?;
        Ok(store)
    }

    /// Returns durable status without network access.
    ///
    /// # Errors
    ///
    /// Returns an error if state or installed-version inventory is invalid.
    pub fn status(&self) -> Result<UpdateStatus, UpdateError> {
        let state = self.load_state()?;
        self.status_from_state(state)
    }

    /// Verifies metadata against a trusted key, the current time, platform,
    /// install-target convention, and durable replay watermark.
    ///
    /// # Errors
    ///
    /// Returns a stable metadata/signature/platform/replay error.
    pub fn verify_metadata(
        &self,
        signed: &SignedUpdate,
        trusted_public_key_hex: &str,
    ) -> Result<(), UpdateError> {
        self.verify_metadata_at(signed, trusted_public_key_hex, now_unix_s()?)
    }

    /// Deterministic form of [`Self::verify_metadata`] for expiry-boundary
    /// tests and release tooling.
    ///
    /// # Errors
    ///
    /// Returns a stable metadata/signature/platform/replay error.
    pub fn verify_metadata_at(
        &self,
        signed: &SignedUpdate,
        trusted_public_key_hex: &str,
        now_unix_s: u64,
    ) -> Result<(), UpdateError> {
        verify_signature(signed, trusted_public_key_hex)?;
        validate_payload(&signed.payload, &built_in_target_triple(), Some(now_unix_s))?;
        let state = self.load_state()?;
        if signed.payload.version <= state.last_verified_version {
            return Err(UpdateError::Replay);
        }
        Ok(())
    }

    /// Verifies any explicitly named inventory artifact without installing it.
    /// This is used for package signatures (for example an `AppImage`) while
    /// only `install_target` remains launchable.
    ///
    /// # Errors
    ///
    /// Returns metadata, target-name, path, size, or digest errors.
    pub fn verify_release_artifact(
        &self,
        signed: &SignedUpdate,
        target_name: &str,
        artifact_path: &Path,
        trusted_public_key_hex: &str,
    ) -> Result<(), UpdateError> {
        let _lock = self.exclusive_lock()?;
        verify_signature(signed, trusted_public_key_hex)?;
        validate_payload(
            &signed.payload,
            &built_in_target_triple(),
            Some(now_unix_s()?),
        )?;
        let target = signed
            .payload
            .targets
            .iter()
            .find(|target| target.path == target_name)
            .ok_or(UpdateError::TargetNotFound)?;
        let (digest, size) = file_digest(artifact_path)?;
        if size != target.size {
            return Err(UpdateError::TargetSizeMismatch);
        }
        if digest != target.sha256 {
            return Err(UpdateError::TargetDigestMismatch);
        }
        Ok(())
    }

    /// Retains the currently running, already-trusted executable as the first
    /// rollback target. This is intentionally explicit: callers invoke it only
    /// after a candidate update's metadata and artifact have both verified.
    /// Existing update state is never changed.
    ///
    /// # Errors
    ///
    /// Returns a path, conflict, or store error.
    pub fn bootstrap_fallback(
        &self,
        app_version: &str,
        executable: &Path,
    ) -> Result<UpdateStatus, UpdateError> {
        let _lock = self.exclusive_lock()?;
        let state = self.load_state()?;
        if state.current.is_some() {
            return self.status_from_state(state);
        }
        let version = sanitize_version(app_version)?;
        let version_dir = self.versions_dir.join(&version);
        if version_dir.exists() {
            return Err(UpdateError::Conflict);
        }
        self.publish_bootstrap(&version_dir, &version, executable)?;
        let new_state = UpdateState {
            schema_version: STATE_SCHEMA,
            current: Some(version),
            ..state
        };
        self.write_state(&new_state)?;
        self.status_from_state(new_state)
    }

    /// Installs and activates the one explicitly designated executable target.
    ///
    /// The entire verify/publish/state transaction is protected by an
    /// inter-process lock. The source is opened once before hashing/copying,
    /// the staged copy is rehashed, and activation plus replay state is one
    /// atomic state replacement.
    ///
    /// # Errors
    ///
    /// Returns a stable update error. Quarantine recording is best effort and
    /// never hides the primary rejection reason.
    pub fn install(
        &self,
        signed: &SignedUpdate,
        artifact_path: &Path,
        trusted_public_key_hex: &str,
    ) -> Result<UpdateStatus, UpdateError> {
        let _lock = self.exclusive_lock()?;
        let result = self.install_locked(signed, artifact_path, trusted_public_key_hex);
        if let Err(error) = result
            && should_quarantine(error)
        {
            self.quarantine_best_effort(error.code());
        }
        result
    }

    /// Activates the previous executable after re-verifying its publisher
    /// signature, schema, role, platform, app-version and selected-target
    /// binding, then hashing the stored bytes.
    ///
    /// Metadata expiry is intentionally an acceptance deadline, not a point at
    /// which a previously installed recovery binary loses authenticity.
    ///
    /// # Errors
    ///
    /// Returns `NoPrevious`, a signature/binding error, or an artifact error.
    pub fn rollback(
        &self,
        trusted_public_key_hex: &str,
    ) -> Result<UpdateStatus, UpdateError> {
        let _lock = self.exclusive_lock()?;
        let state = self.load_state()?;
        let previous = state.previous.clone().ok_or(UpdateError::NoPrevious)?;
        self.verify_installed_locked(&previous, trusted_public_key_hex)?;
        let new_state = UpdateState {
            current: Some(previous),
            previous: state.current,
            ..state
        };
        self.write_state(&new_state)?;
        self.status_from_state(new_state)
    }

    /// Verifies an installed version at rest against the trusted publisher key.
    ///
    /// # Errors
    ///
    /// Returns a stable signature, binding, path, not-found, or digest error.
    pub fn verify_installed(
        &self,
        version: &str,
        trusted_public_key_hex: &str,
    ) -> Result<(), UpdateError> {
        let _lock = self.exclusive_lock()?;
        self.verify_installed_locked(version, trusted_public_key_hex)
    }

    #[cfg(test)]
    fn active_executable(
        &self,
        trusted_public_key_hex: &str,
    ) -> Result<PathBuf, UpdateError> {
        let _lock = self.exclusive_lock()?;
        let state = self.load_state()?;
        let current = state.current.ok_or(UpdateError::Missing)?;
        self.verify_installed_locked(&current, trusted_public_key_hex)?;
        Ok(self.versions_dir.join(current).join(ARTIFACT_FILE))
    }

    /// Verifies and starts the active executable while keeping both the store
    /// lock and verified file handle alive through process creation. Unix
    /// launches through the open descriptor; Windows opens without delete or
    /// write sharing, preventing pathname replacement during `CreateProcess`.
    ///
    /// # Errors
    ///
    /// Returns a verification or process-start error.
    pub fn run_active(
        &self,
        trusted_public_key_hex: &str,
        args: &[String],
    ) -> Result<ExitStatus, UpdateError> {
        let lock = self.exclusive_lock()?;
        let state = self.load_state()?;
        let current = state.current.ok_or(UpdateError::Missing)?;
        self.verify_installed_locked(&current, trusted_public_key_hex)?;
        let path = self.versions_dir.join(current).join(ARTIFACT_FILE);
        let file = open_real_file(&path)?;
        #[cfg(unix)]
        let executable = {
            use std::os::fd::AsRawFd;
            if cfg!(target_os = "linux") {
                PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
            } else {
                PathBuf::from(format!("/dev/fd/{}", file.as_raw_fd()))
            }
        };
        #[cfg(not(unix))]
        let executable = path;
        let mut child = Command::new(executable)
            .args(args)
            .spawn()
            .map_err(map_store_error)?;
        drop(file);
        drop(lock);
        child.wait().map_err(map_store_error)
    }

    /// Determines current and previous versions for launch/status adapters.
    ///
    /// # Errors
    ///
    /// Returns an error when state is malformed.
    pub fn active_versions(&self) -> Result<(Option<String>, Option<String>), UpdateError> {
        let state = self.load_state()?;
        Ok((state.current, state.previous))
    }

    /// Lists safely named, completely published version directories.
    ///
    /// # Errors
    ///
    /// Returns an error when the inventory cannot be read.
    pub fn installed_versions(&self) -> Result<Vec<String>, UpdateError> {
        let mut versions = Vec::new();
        for entry in fs::read_dir(&self.versions_dir).map_err(map_store_error)? {
            let entry = entry.map_err(map_store_error)?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if sanitize_version(&name).is_ok()
                && real_file(&entry.path().join(ARTIFACT_FILE)).is_ok()
                && real_file(&entry.path().join(METADATA_FILE)).is_ok()
            {
                versions.push(name);
            }
        }
        versions.sort();
        Ok(versions)
    }

    fn install_locked(
        &self,
        signed: &SignedUpdate,
        artifact_path: &Path,
        trusted_public_key_hex: &str,
    ) -> Result<UpdateStatus, UpdateError> {
        verify_signature(signed, trusted_public_key_hex)?;
        validate_payload(
            &signed.payload,
            &built_in_target_triple(),
            Some(now_unix_s()?),
        )?;
        let state = self.load_state()?;
        if signed.payload.version <= state.last_verified_version {
            return Err(UpdateError::Replay);
        }
        let version = sanitize_version(&signed.payload.app_version)?;
        if state.current.as_deref() == Some(version.as_str()) {
            return Err(UpdateError::Conflict);
        }
        let target = selected_target(&signed.payload)?.clone();
        let version_dir = self.versions_dir.join(&version);
        if version_dir.exists() {
            self.verify_published_retry(&version, signed, &target, trusted_public_key_hex)?;
        } else {
            self.publish_version(&version_dir, signed, &target, artifact_path)?;
        }
        let new_state = UpdateState {
            schema_version: STATE_SCHEMA,
            current: Some(version),
            previous: state.current,
            last_verified_version: signed.payload.version,
            last_verified_at_unix_ms: unix_millis(),
            last_verified_metadata: Some(signed.clone()),
        };
        self.write_state(&new_state)?;
        self.status_from_state(new_state)
    }

    fn verify_published_retry(
        &self,
        version: &str,
        signed: &SignedUpdate,
        target: &UpdateTarget,
        trusted_public_key_hex: &str,
    ) -> Result<(), UpdateError> {
        let stored = self.load_version_metadata(version)?;
        if stored.signed.as_ref() != Some(signed) || stored.target != *target {
            return Err(UpdateError::Conflict);
        }
        self.verify_installed_locked(version, trusted_public_key_hex)
    }

    fn publish_version(
        &self,
        version_dir: &Path,
        signed: &SignedUpdate,
        target: &UpdateTarget,
        artifact_path: &Path,
    ) -> Result<(), UpdateError> {
        let mut source = open_real_file(artifact_path)?;
        let (source_digest, source_size) = digest_reader(&mut source)?;
        if source_size != target.size {
            return Err(UpdateError::TargetSizeMismatch);
        }
        if source_digest != target.sha256 {
            return Err(UpdateError::TargetDigestMismatch);
        }
        source.seek(SeekFrom::Start(0)).map_err(map_store_error)?;

        let stage_dir = self
            .versions_dir
            .join(format!(".staging-{}", Uuid::new_v4()));
        ensure_real_directory(&stage_dir)?;
        let result = (|| -> Result<(), UpdateError> {
            let artifact = stage_dir.join(ARTIFACT_FILE);
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o500);
            let mut staged = options.open(&artifact).map_err(map_store_error)?;
            std::io::copy(&mut source, &mut staged).map_err(map_store_error)?;
            staged.sync_all().map_err(map_store_error)?;
            drop(staged);
            #[cfg(unix)]
            fs::set_permissions(&artifact, fs::Permissions::from_mode(0o500))
                .map_err(map_store_error)?;
            let (staged_digest, staged_size) = file_digest(&artifact)?;
            if staged_size != target.size || staged_digest != target.sha256 {
                return Err(UpdateError::TargetDigestMismatch);
            }
            let installed = InstalledVersion {
                schema_version: INSTALLED_SCHEMA,
                signed: Some(signed.clone()),
                app_version: signed.payload.app_version.clone(),
                target: target.clone(),
            };
            atomic_write_new(&stage_dir.join(METADATA_FILE), &serialize(&installed)?)?;
            sync_directory(&stage_dir)?;
            fs::rename(&stage_dir, version_dir).map_err(map_store_error)?;
            sync_directory(&self.versions_dir)
        })();
        if result.is_err() {
            let _ignored = fs::remove_dir_all(&stage_dir);
        }
        result
    }

    fn publish_bootstrap(
        &self,
        version_dir: &Path,
        version: &str,
        executable: &Path,
    ) -> Result<(), UpdateError> {
        let mut source = open_real_file(executable)?;
        let (sha256, size) = digest_reader(&mut source)?;
        if size > MAX_TARGET_SIZE {
            return Err(UpdateError::TargetSizeMismatch);
        }
        source.seek(SeekFrom::Start(0)).map_err(map_store_error)?;
        let stage_dir = self
            .versions_dir
            .join(format!(".staging-{}", Uuid::new_v4()));
        ensure_real_directory(&stage_dir)?;
        let result = (|| -> Result<(), UpdateError> {
            let artifact = stage_dir.join(ARTIFACT_FILE);
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o500);
            let mut staged = options.open(&artifact).map_err(map_store_error)?;
            std::io::copy(&mut source, &mut staged).map_err(map_store_error)?;
            staged.sync_all().map_err(map_store_error)?;
            drop(staged);
            #[cfg(unix)]
            fs::set_permissions(&artifact, fs::Permissions::from_mode(0o500))
                .map_err(map_store_error)?;
            let installed = InstalledVersion {
                schema_version: INSTALLED_SCHEMA,
                signed: None,
                app_version: version.to_owned(),
                target: UpdateTarget {
                    path: ARTIFACT_FILE.to_owned(),
                    sha256,
                    size,
                },
            };
            atomic_write_new(&stage_dir.join(METADATA_FILE), &serialize(&installed)?)?;
            sync_directory(&stage_dir)?;
            fs::rename(&stage_dir, version_dir).map_err(map_store_error)?;
            sync_directory(&self.versions_dir)
        })();
        if result.is_err() {
            let _ignored = fs::remove_dir_all(&stage_dir);
        }
        result
    }

    fn verify_installed_locked(
        &self,
        version: &str,
        trusted_public_key_hex: &str,
    ) -> Result<(), UpdateError> {
        let version = sanitize_version(version)?;
        let stored = self.load_version_metadata(&version)?;
        if stored.schema_version != INSTALLED_SCHEMA {
            return Err(UpdateError::UnsupportedSchema);
        }
        if stored.app_version != version {
            return Err(UpdateError::ArtifactCorrupt);
        }
        if let Some(signed) = &stored.signed {
            verify_signature(signed, trusted_public_key_hex)?;
            validate_payload(&signed.payload, &built_in_target_triple(), None)?;
            if signed.payload.app_version != version
                || selected_target(&signed.payload)? != &stored.target
            {
                return Err(UpdateError::ArtifactCorrupt);
            }
        } else if stored.target.path != ARTIFACT_FILE {
            return Err(UpdateError::ArtifactCorrupt);
        }
        let artifact = self.versions_dir.join(&version).join(ARTIFACT_FILE);
        let (digest, size) = file_digest(&artifact)?;
        if digest == stored.target.sha256 && size == stored.target.size {
            Ok(())
        } else {
            Err(UpdateError::ArtifactCorrupt)
        }
    }

    fn load_version_metadata(
        &self,
        version: &str,
    ) -> Result<InstalledVersion, UpdateError> {
        let version_dir = self.versions_dir.join(version);
        require_real_directory(&version_dir).map_err(|error| {
            if version_dir.exists() {
                error
            } else {
                UpdateError::NotFound
            }
        })?;
        let path = version_dir.join(METADATA_FILE);
        let mut file = open_real_file(&path).map_err(|error| {
            if path.exists() {
                error
            } else {
                UpdateError::NotFound
            }
        })?;
        let bytes = read_bounded(&mut file, MAX_METADATA_BYTES)?;
        serde_json::from_slice(&bytes).map_err(|_| UpdateError::ArtifactCorrupt)
    }

    fn load_state(&self) -> Result<UpdateState, UpdateError> {
        match read_real_file(&self.state_path) {
            Ok(bytes) => {
                let state: UpdateState =
                    serde_json::from_slice(&bytes).map_err(|_| UpdateError::StoreFailed)?;
                if state.schema_version == STATE_SCHEMA {
                    Ok(state)
                } else {
                    Err(UpdateError::UnsupportedSchema)
                }
            },
            Err(UpdateError::NotFound) => Ok(UpdateState {
                schema_version: STATE_SCHEMA,
                ..UpdateState::default()
            }),
            Err(error) => Err(error),
        }
    }

    fn write_state(
        &self,
        state: &UpdateState,
    ) -> Result<(), UpdateError> {
        atomic_replace(&self.state_path, &serialize(state)?)
    }

    fn status_from_state(
        &self,
        state: UpdateState,
    ) -> Result<UpdateStatus, UpdateError> {
        let metadata = state
            .last_verified_metadata
            .as_ref()
            .map(|signed| &signed.payload);
        Ok(UpdateStatus {
            current_version: state.current,
            previous_version: state.previous,
            installed_versions: self.installed_versions()?,
            last_verified_update_version: state.last_verified_version,
            last_verified_app_version: metadata.map(|payload| payload.app_version.clone()),
            last_verified_expires_at_unix_s: metadata.map(|payload| payload.expires_at_unix_s),
            last_verified_platform: metadata.map(|payload| payload.platform.clone()),
            last_verified_at_unix_ms: (state.last_verified_at_unix_ms != 0)
                .then_some(state.last_verified_at_unix_ms),
        })
    }

    fn exclusive_lock(&self) -> Result<fs::File, UpdateError> {
        let file = open_lock_file(&self.lock_path)?;
        FileExt::lock_exclusive(&file).map_err(map_store_error)?;
        let path_metadata = real_file(&self.lock_path)?;
        let opened_metadata = file.metadata().map_err(map_store_error)?;
        if !same_file_identity(&path_metadata, &opened_metadata) || hard_linked(&opened_metadata) {
            return Err(UpdateError::PathRejected);
        }
        Ok(file)
    }

    fn recover_staging(&self) -> Result<(), UpdateError> {
        for entry in fs::read_dir(&self.versions_dir).map_err(map_store_error)? {
            let entry = entry.map_err(map_store_error)?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".staging-"))
            {
                let metadata = fs::symlink_metadata(entry.path()).map_err(map_store_error)?;
                if metadata.file_type().is_symlink() {
                    fs::remove_file(entry.path()).map_err(map_store_error)?;
                } else if metadata.is_dir() {
                    fs::remove_dir_all(entry.path()).map_err(map_store_error)?;
                } else {
                    fs::remove_file(entry.path()).map_err(map_store_error)?;
                }
            }
        }
        Ok(())
    }

    fn quarantine_best_effort(
        &self,
        reason: &str,
    ) {
        let note = QuarantineNote {
            schema_version: 1,
            reason: reason.to_owned(),
            quarantined_at_unix_ms: unix_millis(),
        };
        if let Ok(bytes) = serialize(&note) {
            let path = self.quarantine_dir.join(format!("{}.json", Uuid::new_v4()));
            let _ignored = atomic_write_new(&path, &bytes);
        }
    }
}

/// Performs complete context-free metadata validation for release tooling.
/// Replay checks remain store-specific.
///
/// # Errors
///
/// Returns schema, metadata, expiry, or platform errors.
pub fn validate_metadata(
    payload: &UpdateMetadata,
    expected_platform: &str,
    now_unix_s: u64,
) -> Result<(), UpdateError> {
    validate_payload(payload, expected_platform, Some(now_unix_s))
}

fn validate_payload(
    payload: &UpdateMetadata,
    expected_platform: &str,
    now_unix_s: Option<u64>,
) -> Result<(), UpdateError> {
    if payload.schema_version != crate::types::METADATA_SCHEMA {
        return Err(UpdateError::UnsupportedSchema);
    }
    if payload.role != TARGETS_ROLE
        || payload.version == 0
        || payload.role.len() > 32
        || payload.app_version.len() > 128
        || payload.platform.is_empty()
        || payload.platform.len() > 128
        || payload.install_target.len() > 1024
        || payload.targets.is_empty()
        || sanitize_version(&payload.app_version).is_err()
        || payload.platform != expected_platform
        || !valid_targets(&payload.targets)
        || payload.install_target != expected_install_target(expected_platform)
        || selected_target(payload).is_err()
    {
        if payload.platform != expected_platform {
            return Err(UpdateError::PlatformMismatch);
        }
        return Err(UpdateError::InvalidMetadata);
    }
    if now_unix_s.is_some_and(|now| now >= payload.expires_at_unix_s) {
        return Err(UpdateError::MetadataExpired);
    }
    Ok(())
}

fn selected_target(payload: &UpdateMetadata) -> Result<&UpdateTarget, UpdateError> {
    let mut matches = payload
        .targets
        .iter()
        .filter(|target| target.path == payload.install_target);
    let selected = matches.next().ok_or(UpdateError::TargetNotFound)?;
    if matches.next().is_some() {
        Err(UpdateError::InvalidMetadata)
    } else {
        Ok(selected)
    }
}

fn expected_install_target(platform: &str) -> String {
    if platform.contains("-windows-") {
        format!("koe-cli-{platform}.exe")
    } else {
        format!("koe-cli-{platform}")
    }
}

fn valid_targets(targets: &[UpdateTarget]) -> bool {
    if targets.len() > MAX_TARGETS {
        return false;
    }
    let mut paths = BTreeSet::new();
    targets.iter().all(|target| {
        let path_valid = !target.path.is_empty()
            && target.path.len() <= 1024
            && !target.path.starts_with('/')
            && !target.path.contains('\\')
            && target
                .path
                .split('/')
                .all(|component| !component.is_empty() && component != "." && component != "..");
        let digest_valid = target.size > 0
            && target.size <= MAX_TARGET_SIZE
            && target.sha256.len() == 64
            && target
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
        path_valid && digest_valid && paths.insert(target.path.as_str())
    })
}

const fn should_quarantine(error: UpdateError) -> bool {
    matches!(
        error,
        UpdateError::UnsupportedSchema
            | UpdateError::SignatureInvalid
            | UpdateError::InvalidMetadata
            | UpdateError::MetadataExpired
            | UpdateError::Replay
            | UpdateError::PlatformMismatch
            | UpdateError::TargetSizeMismatch
            | UpdateError::TargetDigestMismatch
            | UpdateError::TargetNotFound
            | UpdateError::ArtifactCorrupt
    )
}

fn require_real_directory(path: &Path) -> Result<(), UpdateError> {
    let metadata = fs::symlink_metadata(path).map_err(map_store_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        Err(UpdateError::PathRejected)
    } else {
        Ok(())
    }
}

fn ensure_real_directory(path: &Path) -> Result<(), UpdateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {},
        Ok(_) => return Err(UpdateError::PathRejected),
        Err(error) if error.kind() == ErrorKind::NotFound => match fs::create_dir(path) {
            Ok(()) => {},
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                require_real_directory(path)?;
            },
            Err(error) => return Err(map_store_error(error)),
        },
        Err(_) => return Err(UpdateError::StoreFailed),
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(map_store_error)?;
    Ok(())
}

fn real_file(path: &Path) -> Result<fs::Metadata, UpdateError> {
    let metadata = fs::symlink_metadata(path).map_err(map_missing)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(UpdateError::PathRejected);
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return Err(UpdateError::PathRejected);
    }
    Ok(metadata)
}

fn open_lock_file(path: &Path) -> Result<fs::File, UpdateError> {
    let mut create = fs::OpenOptions::new();
    create.read(true).write(true).create_new(true);
    #[cfg(unix)]
    create.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    #[cfg(windows)]
    create.custom_flags(0x0020_0000).share_mode(0x0000_0003); // share reads/writes, deny replacement
    match create.open(path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let before = real_file(path)?;
            let mut open = fs::OpenOptions::new();
            open.read(true).write(true);
            #[cfg(unix)]
            open.custom_flags(libc::O_NOFOLLOW);
            #[cfg(windows)]
            open.custom_flags(0x0020_0000).share_mode(0x0000_0003);
            let file = open.open(path).map_err(map_store_error)?;
            let after = file.metadata().map_err(map_store_error)?;
            if same_file_identity(&before, &after) && !hard_linked(&after) {
                Ok(file)
            } else {
                Err(UpdateError::PathRejected)
            }
        },
        Err(error) => Err(map_store_error(error)),
    }
}

fn open_real_file(path: &Path) -> Result<fs::File, UpdateError> {
    let before = real_file(path)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    #[cfg(windows)]
    options
        .custom_flags(0x0020_0000) // FILE_FLAG_OPEN_REPARSE_POINT
        .share_mode(0x0000_0001); // FILE_SHARE_READ; deny writes/deletes
    let file = options.open(path).map_err(map_missing)?;
    let metadata = file.metadata().map_err(map_store_error)?;
    if !metadata.is_file() || !same_file_identity(&before, &metadata) || hard_linked(&metadata) {
        return Err(UpdateError::PathRejected);
    }
    Ok(file)
}

#[cfg(unix)]
fn same_file_identity(
    before: &fs::Metadata,
    after: &fs::Metadata,
) -> bool {
    before.dev() == after.dev() && before.ino() == after.ino()
}

#[cfg(windows)]
fn same_file_identity(
    before: &fs::Metadata,
    after: &fs::Metadata,
) -> bool {
    before.volume_serial_number() == after.volume_serial_number()
        && before.file_index() == after.file_index()
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(
    before: &fs::Metadata,
    after: &fs::Metadata,
) -> bool {
    before.is_file() && after.is_file() && before.len() == after.len()
}

#[cfg(unix)]
fn hard_linked(metadata: &fs::Metadata) -> bool {
    metadata.nlink() != 1
}

#[cfg(windows)]
fn hard_linked(metadata: &fs::Metadata) -> bool {
    metadata.number_of_links() != Some(1)
}

#[cfg(not(any(unix, windows)))]
fn hard_linked(_metadata: &fs::Metadata) -> bool {
    false
}

fn read_bounded(
    file: &mut fs::File,
    maximum: u64,
) -> Result<Vec<u8>, UpdateError> {
    if file.metadata().map_err(map_store_error)?.len() > maximum {
        return Err(UpdateError::InvalidMetadata);
    }
    let capacity = usize::try_from(maximum.min(64 * 1024)).map_err(|_| UpdateError::StoreFailed)?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(map_store_error)?;
    if u64::try_from(bytes.len()).map_err(|_| UpdateError::InvalidMetadata)? > maximum {
        Err(UpdateError::InvalidMetadata)
    } else {
        Ok(bytes)
    }
}

fn read_real_file(path: &Path) -> Result<Vec<u8>, UpdateError> {
    let mut file = open_real_file(path)?;
    read_bounded(&mut file, MAX_METADATA_BYTES)
}

fn serialize<T: Serialize>(value: &T) -> Result<Vec<u8>, UpdateError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|_| UpdateError::StoreFailed)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn atomic_write_new(
    path: &Path,
    bytes: &[u8],
) -> Result<(), UpdateError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).map_err(map_store_error)?;
    file.write_all(bytes).map_err(map_store_error)?;
    file.sync_all().map_err(map_store_error)?;
    sync_directory(path.parent().ok_or(UpdateError::PathRejected)?)
}

fn atomic_replace(
    path: &Path,
    bytes: &[u8],
) -> Result<(), UpdateError> {
    let parent = path.parent().ok_or(UpdateError::PathRejected)?;
    let mut temporary = Builder::new()
        .prefix(".state-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(map_store_error)?;
    temporary.write_all(bytes).map_err(map_store_error)?;
    temporary.as_file().sync_all().map_err(map_store_error)?;
    temporary
        .persist(path)
        .map_err(|_| UpdateError::StoreFailed)?;
    sync_directory(parent)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), UpdateError> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(map_store_error)
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), UpdateError> {
    Ok(())
}

/// Computes lowercase SHA-256 and byte size after rejecting symlinks and,
/// where available, hard links.
///
/// # Errors
///
/// Returns a path/store error for unsafe or unreadable files.
pub fn file_digest(path: &Path) -> Result<(String, u64), UpdateError> {
    let mut file = open_real_file(path)?;
    digest_reader(&mut file)
}

fn digest_reader(file: &mut fs::File) -> Result<(String, u64), UpdateError> {
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut size = 0_u64;
    loop {
        let count = file.read(&mut buffer).map_err(map_store_error)?;
        if count == 0 {
            break;
        }
        size = size
            .checked_add(u64::try_from(count).map_err(|_| UpdateError::StoreFailed)?)
            .ok_or(UpdateError::StoreFailed)?;
        digest.update(&buffer[..count]);
    }
    Ok((hex_encode(&digest.finalize()), size))
}

fn sanitize_version(version: &str) -> Result<String, UpdateError> {
    let safe = version
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_'));
    if safe
        && !version.is_empty()
        && version.len() <= 128
        && !version.starts_with('.')
        && !version.ends_with('.')
        && !version.contains("..")
    {
        Ok(version.to_owned())
    } else {
        Err(UpdateError::InvalidVersion)
    }
}

/// Canonical rustc target triple of the running binary.
#[must_use]
pub fn built_in_target_triple() -> String {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    match os {
        "macos" => format!("{arch}-apple-darwin"),
        "windows" => format!(
            "{arch}-pc-windows-{}",
            if cfg!(target_env = "gnu") {
                "gnu"
            } else {
                "msvc"
            }
        ),
        "linux" => format!(
            "{arch}-unknown-linux-{}",
            if cfg!(target_env = "musl") {
                "musl"
            } else {
                "gnu"
            }
        ),
        _ => format!("{arch}-unknown-{os}"),
    }
}

fn now_unix_s() -> Result<u64, UpdateError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| UpdateError::StoreFailed)
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

#[allow(clippy::needless_pass_by_value)]
fn map_store_error(_error: std::io::Error) -> UpdateError {
    UpdateError::StoreFailed
}

#[allow(clippy::needless_pass_by_value)]
fn map_missing(error: std::io::Error) -> UpdateError {
    if error.kind() == ErrorKind::NotFound {
        UpdateError::NotFound
    } else {
        UpdateError::StoreFailed
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, Barrier},
        thread,
    };

    use tempfile::TempDir;

    use super::{UpdateStore, built_in_target_triple, expected_install_target, file_digest};
    use crate::{
        public_key_hex, sign_update,
        types::{SignedUpdate, UpdateError, UpdateMetadata, UpdateTarget},
    };

    const SEED: [u8; 32] = [5_u8; 32];
    const WRONG_SEED: [u8; 32] = [6_u8; 32];

    fn artifact(
        root: &TempDir,
        name: &str,
        bytes: &[u8],
    ) -> PathBuf {
        let path = root.path().join(name);
        fs::write(&path, bytes).expect("fixture artifact");
        path
    }

    use std::path::PathBuf;

    fn signed_for(
        root: &TempDir,
        bytes: &[u8],
        metadata_version: u64,
        app_version: &str,
    ) -> (SignedUpdate, PathBuf) {
        let path = artifact(root, &format!("source-{metadata_version}.bin"), bytes);
        let (sha256, size) = file_digest(&path).expect("digest");
        let install_target = expected_install_target(&built_in_target_triple());
        let payload = UpdateMetadata {
            schema_version: 1,
            role: "targets".to_owned(),
            version: metadata_version,
            expires_at_unix_s: u64::MAX,
            app_version: app_version.to_owned(),
            platform: built_in_target_triple(),
            install_target: install_target.clone(),
            targets: vec![UpdateTarget {
                path: install_target,
                sha256,
                size,
            }],
        };
        (sign_update(&payload, &SEED).expect("sign"), path)
    }

    fn store(root: &TempDir) -> UpdateStore {
        UpdateStore::open(root.path()).expect("store")
    }

    fn quarantine_count(root: &TempDir) -> usize {
        fs::read_dir(root.path().join("updates/quarantine"))
            .expect("quarantine")
            .count()
    }

    #[test]
    fn install_repeated_install_and_rollback_are_atomic_replacements() {
        let root = TempDir::new().expect("temp");
        let (first, first_path) = signed_for(&root, b"one", 1, "0.1.0");
        let (second, second_path) = signed_for(&root, b"two", 2, "0.2.0");
        let store = store(&root);
        store
            .install(&first, &first_path, &public_key_hex(&SEED))
            .expect("first");
        let status = store
            .install(&second, &second_path, &public_key_hex(&SEED))
            .expect("second");
        assert_eq!(status.current_version.as_deref(), Some("0.2.0"));
        let status = store.rollback(&public_key_hex(&SEED)).expect("rollback");
        assert_eq!(status.current_version.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn only_designated_executable_can_be_installed() {
        let root = TempDir::new().expect("temp");
        let (mut signed, executable) = signed_for(&root, b"binary", 1, "0.1.0");
        let sums = artifact(&root, "SHA256SUMS", b"binary  file");
        let (sha256, size) = file_digest(&sums).expect("digest");
        signed.payload.targets.push(UpdateTarget {
            path: "SHA256SUMS".to_owned(),
            sha256,
            size,
        });
        signed = sign_update(&signed.payload, &SEED).expect("resign");
        assert_eq!(
            store(&root).install(&signed, &sums, &public_key_hex(&SEED)),
            Err(UpdateError::TargetSizeMismatch)
        );
        store(&root)
            .install(&signed, &executable, &public_key_hex(&SEED))
            .expect("executable");
    }

    #[cfg(unix)]
    #[test]
    fn launcher_observes_apply_and_rollback_executable_versions() {
        use std::process::Command;

        let root = TempDir::new().expect("temp");
        let (first, first_path) = signed_for(&root, b"#!/bin/sh\necho v1\n", 1, "0.1.0");
        let (second, second_path) = signed_for(&root, b"#!/bin/sh\necho v2\n", 2, "0.2.0");
        let store = store(&root);
        store
            .install(&first, &first_path, &public_key_hex(&SEED))
            .expect("first");
        let run = |store: &UpdateStore| {
            let executable = store
                .active_executable(&public_key_hex(&SEED))
                .expect("active executable");
            String::from_utf8(Command::new(executable).output().expect("launch").stdout)
                .expect("utf8")
        };
        assert_eq!(run(&store), "v1\n");
        store
            .install(&second, &second_path, &public_key_hex(&SEED))
            .expect("second");
        assert_eq!(run(&store), "v2\n");
        store.rollback(&public_key_hex(&SEED)).expect("rollback");
        assert_eq!(run(&store), "v1\n");
    }

    #[test]
    fn active_executable_and_rollback_reverify_signature_and_binding() {
        let root = TempDir::new().expect("temp");
        let (first, first_path) = signed_for(&root, b"one", 1, "0.1.0");
        let (second, second_path) = signed_for(&root, b"two", 2, "0.2.0");
        let store = store(&root);
        store
            .install(&first, &first_path, &public_key_hex(&SEED))
            .expect("first");
        store
            .install(&second, &second_path, &public_key_hex(&SEED))
            .expect("second");
        let metadata_path = root.path().join("updates/versions/0.1.0/metadata.json");
        let mut installed: serde_json::Value =
            serde_json::from_slice(&fs::read(&metadata_path).expect("read")).expect("json");
        installed["signed"]["payload"]["app_version"] = serde_json::json!("9.9.9");
        fs::write(
            &metadata_path,
            serde_json::to_vec(&installed).expect("json"),
        )
        .expect("write");
        assert_eq!(
            store.rollback(&public_key_hex(&SEED)),
            Err(UpdateError::SignatureInvalid)
        );
        assert!(store.active_executable(&public_key_hex(&SEED)).is_ok());
    }

    #[test]
    fn retry_activates_an_orphaned_published_version() {
        let root = TempDir::new().expect("temp");
        let (signed, path) = signed_for(&root, b"one", 1, "0.1.0");
        let store = store(&root);
        let target = super::selected_target(&signed.payload)
            .expect("target")
            .clone();
        store
            .publish_version(
                &root.path().join("updates/versions/0.1.0"),
                &signed,
                &target,
                &path,
            )
            .expect("publish");
        let status = store
            .install(&signed, &path, &public_key_hex(&SEED))
            .expect("recover");
        assert_eq!(status.current_version.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn concurrent_installs_cannot_regress_replay_state() {
        let root = Arc::new(TempDir::new().expect("temp"));
        let (one, one_path) = signed_for(&root, b"one", 1, "0.1.0");
        let (two, two_path) = signed_for(&root, b"two", 2, "0.2.0");
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for (signed, path) in [(one, one_path), (two, two_path)] {
            let root = Arc::clone(&root);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                let store = UpdateStore::open(root.path()).expect("store");
                barrier.wait();
                store.install(&signed, &path, &public_key_hex(&SEED))
            }));
        }
        barrier.wait();
        for handle in handles {
            let _result = handle.join().expect("join");
        }
        let status = store(&root).status().expect("status");
        assert_eq!(status.last_verified_update_version, 2);
        assert_eq!(status.current_version.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn invalid_signatures_are_quarantined_once_without_publication() {
        let cases = ["tampered", "wrong-key", "empty", "malformed", "role"];
        for case in cases {
            let root = TempDir::new().expect("temp");
            let (mut signed, path) = signed_for(&root, b"one", 1, "0.1.0");
            let key = if case == "wrong-key" {
                public_key_hex(&WRONG_SEED)
            } else {
                public_key_hex(&SEED)
            };
            match case {
                "tampered" => signed.payload.app_version = "0.1.1".to_owned(),
                "empty" => signed.signatures.clear(),
                "malformed" => signed.signatures[0].signature = "zz".to_owned(),
                "role" => signed.signatures[0].role = "root".to_owned(),
                _ => {},
            }
            let store = store(&root);
            assert!(store.install(&signed, &path, &key).is_err());
            assert_eq!(quarantine_count(&root), 1, "{case}");
            assert!(store.installed_versions().expect("versions").is_empty());
            assert_eq!(store.status().expect("status").current_version, None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn quarantine_write_failure_preserves_primary_security_error() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new().expect("temp");
        let (mut signed, path) = signed_for(&root, b"one", 1, "0.1.0");
        signed.payload.app_version = "tampered".to_owned();
        let store = store(&root);
        let quarantine = root.path().join("updates/quarantine");
        fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o000))
            .expect("deny quarantine write");
        let error = store
            .install(&signed, &path, &public_key_hex(&SEED))
            .expect_err("reject");
        fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o700))
            .expect("restore permissions");
        assert_eq!(error, UpdateError::SignatureInvalid);
    }

    #[test]
    fn expiry_boundary_is_deterministic() {
        let root = TempDir::new().expect("temp");
        let (mut signed, _path) = signed_for(&root, b"one", 1, "0.1.0");
        signed.payload.expires_at_unix_s = 100;
        signed = sign_update(&signed.payload, &SEED).expect("sign");
        let store = store(&root);
        assert!(
            store
                .verify_metadata_at(&signed, &public_key_hex(&SEED), 99)
                .is_ok()
        );
        assert_eq!(
            store.verify_metadata_at(&signed, &public_key_hex(&SEED), 100),
            Err(UpdateError::MetadataExpired)
        );
        assert_eq!(
            store.verify_metadata_at(&signed, &public_key_hex(&SEED), 101),
            Err(UpdateError::MetadataExpired)
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_hard_links_in_protected_components_are_rejected() {
        use std::os::unix::fs::symlink;
        let root = TempDir::new().expect("temp");
        let outside = TempDir::new().expect("outside");
        symlink(outside.path(), root.path().join("updates")).expect("symlink");
        assert!(matches!(
            UpdateStore::open(root.path()),
            Err(UpdateError::PathRejected)
        ));

        let root = TempDir::new().expect("temp");
        let (signed, path) = signed_for(&root, b"one", 1, "0.1.0");
        let store = store(&root);
        store
            .install(&signed, &path, &public_key_hex(&SEED))
            .expect("install");
        let metadata = root.path().join("updates/versions/0.1.0/metadata.json");
        fs::hard_link(&metadata, root.path().join("metadata-hardlink")).expect("hard link");
        assert_eq!(
            store.verify_installed("0.1.0", &public_key_hex(&SEED)),
            Err(UpdateError::PathRejected)
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_hard_links_in_protected_components_are_rejected() {
        let root = TempDir::new().expect("temp");
        let (signed, path) = signed_for(&root, b"one", 1, "0.1.0");
        let store = store(&root);
        store
            .install(&signed, &path, &public_key_hex(&SEED))
            .expect("install");
        let metadata = root.path().join("updates/versions/0.1.0/metadata.json");
        fs::hard_link(&metadata, root.path().join("metadata-hardlink")).expect("hard link");
        assert_eq!(
            store.verify_installed("0.1.0", &public_key_hex(&SEED)),
            Err(UpdateError::PathRejected)
        );
    }

    #[test]
    fn metadata_inventory_count_and_target_size_are_bounded() {
        let platform = built_in_target_triple();
        let install_target = expected_install_target(&platform);
        let target = UpdateTarget {
            path: install_target.clone(),
            sha256: "0".repeat(64),
            size: 1,
        };
        let mut payload = UpdateMetadata {
            schema_version: 1,
            role: "targets".to_owned(),
            version: 1,
            expires_at_unix_s: u64::MAX,
            app_version: "1.0.0".to_owned(),
            platform: platform.clone(),
            install_target,
            targets: vec![target.clone()],
        };
        for index in 1..=crate::MAX_TARGETS {
            payload.targets.push(UpdateTarget {
                path: format!("inventory-{index}"),
                ..target.clone()
            });
        }
        assert_eq!(
            super::validate_metadata(&payload, &platform, 1),
            Err(UpdateError::InvalidMetadata)
        );
        payload.targets.truncate(1);
        payload.targets[0].size = crate::MAX_TARGET_SIZE + 1;
        assert_eq!(
            super::validate_metadata(&payload, &platform, 1),
            Err(UpdateError::InvalidMetadata)
        );
    }

    #[test]
    fn unknown_version_is_not_found() {
        let root = TempDir::new().expect("temp");
        assert_eq!(
            store(&root).verify_installed("9.9.9", &public_key_hex(&SEED)),
            Err(UpdateError::NotFound)
        );
    }

    #[test]
    fn malformed_signature_entry_type_is_rejected_by_deserialization() {
        let root = TempDir::new().expect("temp");
        let (signed, _path) = signed_for(&root, b"one", 1, "0.1.0");
        let mut value = serde_json::to_value(signed).expect("value");
        value["signatures"][0]["signature"] = serde_json::json!(42);
        assert!(serde_json::from_value::<SignedUpdate>(value).is_err());
    }
}
