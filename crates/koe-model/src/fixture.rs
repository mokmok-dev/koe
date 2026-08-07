//! Deterministic fixture adapter for component tests and offline E2E.
//!
//! The fixture never touches the network. `install` materializes a real
//! artifact tree so the digest inventory path is exercised, and
//! [`fixture_transcribe`] maps PCM to a stable word sequence so latency and
//! WER baselines are reproducible.

use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::{
    adapter::{
        AdapterError, AsrError, AsrEvent, AsrSessionSettings, FinalTranscript, FoundryAdapter,
        InstalledArtifact, InstalledFile, Pcm16Mono16k, StreamingAsrSession,
    },
    types::{Alias, ModelDescriptor, ModelId, ModelScope, ModelSelector, ModelVersion},
};

/// Alias served by the fixture catalog.
pub const FIXTURE_ALIAS: &str = "fixture-nemotron-asr-0.6b";
/// Stable catalog id for the fixture model.
pub const FIXTURE_MODEL_ID: &str = "FixtureLocal/NemotronASRStreaming0.6B";
/// Fixture model version.
pub const FIXTURE_VERSION: &str = "1.0.0-fixture";
/// Canonical ASR sample rate.
pub const FIXTURE_SAMPLE_RATE: u32 = 16_000;

/// Deterministic word table indexed by per-block features.
const WORD_TABLE: [&str; 16] = [
    "aha", "amma", "ane", "asa", "awa", "baba", "bee", "dada", "e", "ene", "fufu", "koko", "mama",
    "nana", "oh", "yaya",
];

const BLOCK_SAMPLES: usize = 160; // 10 ms at 16 kHz.

/// Maps 16 kHz mono PCM to a stable word sequence.
///
/// Each 10 ms block contributes one word selected by zero-crossing rate and
/// mean absolute energy, producing a reproducible pseudo-transcript.
#[must_use]
pub fn fixture_transcribe(samples: &[i16]) -> String {
    let mut words = Vec::with_capacity(samples.len().div_ceil(BLOCK_SAMPLES));
    for block in samples.chunks(BLOCK_SAMPLES) {
        let crossings = zero_crossings(block);
        let energy = mean_abs(block);
        let index = crossings.wrapping_add(energy.rotate_left(3) as usize) & 0xF;
        // Deterministic style pressure: every block contributes a word.
        words.push(WORD_TABLE[index]);
    }
    words.join(" ")
}

fn zero_crossings(samples: &[i16]) -> usize {
    let mut count = 0;
    let mut previous = 0_i16;
    for sample in samples {
        if (previous < 0 && *sample > 0) || (previous > 0 && *sample < 0) {
            count += 1;
        }
        previous = *sample;
    }
    count
}

fn mean_abs(samples: &[i16]) -> u32 {
    if samples.is_empty() {
        return 0;
    }
    let total: u64 = samples
        .iter()
        .map(|sample| u64::from(sample.unsigned_abs()))
        .sum();
    u32::try_from(total / samples.len() as u64).unwrap_or(u32::MAX)
}

/// Streaming session that produces fixture events from input PCM.
pub struct FixtureAsrSession {
    _settings: AsrSessionSettings,
    cursor_us: u64,
    events: Vec<AsrEvent>,
    delivered: usize,
    started: bool,
}

impl FixtureAsrSession {
    /// Creates a session for one model.
    #[must_use]
    pub fn new(settings: &AsrSessionSettings) -> Self {
        Self {
            _settings: settings.clone(),
            cursor_us: 0,
            events: Vec::new(),
            delivered: 0,
            started: true,
        }
    }
}

impl FixtureAsrSession {
    fn next_event(&mut self) -> Option<AsrEvent> {
        let event = self.events.get(self.delivered).cloned();
        if event.is_some() {
            self.delivered += 1;
        }
        event
    }
}

#[async_trait]
impl StreamingAsrSession for FixtureAsrSession {
    async fn append(
        &mut self,
        chunk: Pcm16Mono16k,
    ) -> Result<(), AsrError> {
        if !self.started {
            return Err(AsrError::SessionNotActive);
        }
        if chunk.samples.is_empty() {
            return Ok(());
        }
        #[allow(clippy::cast_possible_truncation)]
        let duration_us =
            chunk.samples.len().saturating_mul(1_000_000) / FIXTURE_SAMPLE_RATE as usize;
        if self.events.is_empty() {
            self.cursor_us = chunk.session_start_us;
        }
        let start_us = self.cursor_us;
        let end_us = u64::try_from(duration_us)
            .ok()
            .map_or(self.cursor_us, |duration| {
                self.cursor_us
                    .checked_add(duration)
                    .unwrap_or(self.cursor_us)
            });
        let event = AsrEvent {
            segment_id: uuid::Uuid::new_v4(),
            text: fixture_transcribe(&chunk.samples),
            start_us,
            end_us,
            is_final: true,
        };
        self.cursor_us = end_us;
        self.events.push(event);
        Ok(())
    }

    async fn poll_results(&mut self) -> Result<Option<AsrEvent>, AsrError> {
        Ok(self.next_event())
    }

    async fn finish(self: Box<Self>) -> Result<FinalTranscript, AsrError> {
        Ok(FinalTranscript {
            events: self.events,
        })
    }
}

/// Adapter that materializes fixture artifacts and fakes runtime state.
pub struct FixtureFoundryAdapter {
    cache_root: PathBuf,
    installed: BTreeSet<ModelId>,
    loaded: BTreeSet<ModelId>,
    install_calls: Arc<Mutex<usize>>,
    install_delay: Duration,
    ignore_install_cancellation: bool,
}

impl FixtureFoundryAdapter {
    /// Creates a fixture adapter with a private cache root.
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        let cache_root = cache_root.into();
        let _ignored = fs::create_dir_all(&cache_root);
        Self {
            cache_root,
            installed: BTreeSet::new(),
            loaded: BTreeSet::new(),
            install_calls: Arc::new(Mutex::new(0)),
            install_delay: Duration::ZERO,
            ignore_install_cancellation: false,
        }
    }

    /// Delays fixture installation so cancellation races can be tested.
    #[must_use]
    pub const fn with_install_delay(
        mut self,
        delay: Duration,
    ) -> Self {
        self.install_delay = delay;
        self
    }

    /// Simulates an SDK download that cannot observe cancellation until its
    /// blocking operation completes.
    #[must_use]
    pub const fn ignoring_install_cancellation(mut self) -> Self {
        self.ignore_install_cancellation = true;
        self
    }

    /// Number of install (download) calls performed.
    ///
    /// # Errors
    ///
    /// Only fails if the mutex is poisoned after a panic.
    pub async fn install_call_count(&self) -> usize {
        *self.install_calls.lock().await
    }

    /// The deterministic catalog descriptor for the fixture model.
    #[must_use]
    pub fn fixture_descriptor() -> ModelDescriptor {
        ModelDescriptor {
            id: ModelId::new(FIXTURE_MODEL_ID.to_owned()),
            alias: Alias(FIXTURE_ALIAS.to_owned()),
            version: ModelVersion::new(FIXTURE_VERSION.to_owned()),
            variant: "cpu".to_owned(),
            provider: "FixtureLocal".to_owned(),
            license_id: "fixture-license-apache-2.0".to_owned(),
            license_description: "Fixture license for offline tests".to_owned(),
            source: "fixture://catalog".to_owned(),
            size_mb: 1,
            task: "automatic-speech-recognition".to_owned(),
        }
    }

    /// Writes model files under the adapter-owned cache root. Existing files
    /// are preserved so a cached install can be verified again.
    fn materialize_artifact(
        &self,
        model: &ModelDescriptor,
    ) -> Result<InstalledArtifact, AdapterError> {
        let model_dir = self
            .cache_root
            .join("models")
            .join(sanitize_component(&model.id.0));
        fs::create_dir_all(&model_dir).map_err(|_| AdapterError::DownloadFailed)?;
        let model_path = model_dir.join("model.bin");
        if !model_path.exists() {
            // Deterministic content derived from the version string.
            let version_bytes = model.version.0.as_bytes();
            let mut content = Vec::with_capacity(4096);
            for i in 0_u64..4096 {
                let index = usize::try_from(i % 16).unwrap_or(0);
                let seed = version_bytes
                    .get(index % version_bytes.len().max(1))
                    .copied()
                    .unwrap_or(0);
                content.push((i.wrapping_mul(31).wrapping_add(u64::from(seed)) & 0xFF) as u8);
            }
            fs::write(&model_path, content).map_err(|_| AdapterError::DownloadFailed)?;
            let metadata = serde_json::json!({
                "model_id": model.id.0,
                "alias": model.alias.0,
                "version": model.version.0,
            });
            let metadata_path = model_dir.join("metadata.json");
            let file =
                fs::File::create(&metadata_path).map_err(|_| AdapterError::DownloadFailed)?;
            serde_json::to_writer_pretty(file, &metadata)
                .map_err(|_| AdapterError::DownloadFailed)?;
        }
        self.artifact_from_cache(model)
    }

    fn artifact_from_cache(
        &self,
        model: &ModelDescriptor,
    ) -> Result<InstalledArtifact, AdapterError> {
        let model_dir = self
            .cache_root
            .join("models")
            .join(sanitize_component(&model.id.0));
        let mut files = Vec::new();
        for entry in fs::read_dir(&model_dir).map_err(|_| AdapterError::RuntimeFailed)? {
            let entry = entry.map_err(|_| AdapterError::RuntimeFailed)?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|_| AdapterError::RuntimeFailed)?;
            if file_type.is_symlink() || !file_type.is_file() {
                return Err(AdapterError::RuntimeFailed);
            }
            let bytes = fs::read(&path).map_err(|_| AdapterError::RuntimeFailed)?;
            let sha256 = hex_encode(&Sha256::digest(&bytes));
            let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            let relative = path
                .strip_prefix(&self.cache_root)
                .map_err(|_| AdapterError::RuntimeFailed)?
                .to_string_lossy()
                .replace('\\', "/");
            files.push(InstalledFile {
                absolute_path: path,
                relative_path: relative,
                size,
                sha256,
            });
        }
        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(InstalledArtifact {
            cache_root: self.cache_root.clone(),
            artifact_root: model_dir,
            model_id: model.id.clone(),
            files,
            created_by_install: false,
        })
    }
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

/// SHA-256 hex encoding without extra dependencies.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0F)]));
    }
    output
}

#[async_trait]
impl FoundryAdapter for FixtureFoundryAdapter {
    fn backend_name(&self) -> &'static str {
        "fixture"
    }

    async fn list_catalog(&mut self) -> Result<Vec<ModelDescriptor>, AdapterError> {
        Ok(vec![Self::fixture_descriptor()])
    }

    async fn resolve(
        &mut self,
        selector: &ModelSelector,
    ) -> Result<ModelDescriptor, AdapterError> {
        let descriptor = Self::fixture_descriptor();
        let key = selector.key();
        if key == descriptor.alias.0.to_ascii_lowercase()
            || key == descriptor.id.0.to_ascii_lowercase()
        {
            Ok(descriptor)
        } else {
            Err(AdapterError::NotFound)
        }
    }

    async fn latest_version(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<ModelVersion, AdapterError> {
        Ok(model.version.clone())
    }

    async fn list_installed(&mut self) -> Result<Vec<ModelDescriptor>, AdapterError> {
        let mut models = self
            .installed
            .iter()
            .map(|_| Self::fixture_descriptor())
            .collect::<Vec<_>>();
        models.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(models)
    }

    async fn list_loaded(&mut self) -> Result<Vec<ModelId>, AdapterError> {
        Ok(self.loaded.iter().cloned().collect())
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
        {
            let mut counter = self.install_calls.lock().await;
            *counter = counter.saturating_add(1);
        }
        if !self.install_delay.is_zero() {
            if self.ignore_install_cancellation {
                tokio::time::sleep(self.install_delay).await;
            } else {
                tokio::select! {
                    () = tokio::time::sleep(self.install_delay) => {},
                    () = cancel.cancelled() => return Err(AdapterError::DownloadFailed),
                }
            }
        }
        let model_dir = self
            .cache_root
            .join("models")
            .join(sanitize_component(&model.id.0));
        let cache_existed = model_dir.is_dir();
        if cache_existed && !force {
            self.installed.insert(model.id.clone());
            return self.artifact_from_cache(model);
        }
        self.installed.insert(model.id.clone());
        let mut artifact = self.materialize_artifact(model)?;
        artifact.created_by_install = !cache_existed;
        Ok(artifact)
    }

    async fn inspect_local_artifact(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<InstalledArtifact, AdapterError> {
        let artifact = self.artifact_from_cache(model)?;
        self.installed.insert(model.id.clone());
        Ok(artifact)
    }

    async fn load(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<(), AdapterError> {
        self.artifact_from_cache(model)?;
        self.installed.insert(model.id.clone());
        self.loaded.insert(model.id.clone());
        Ok(())
    }

    async fn unload(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<(), AdapterError> {
        self.loaded.remove(&model.id);
        Ok(())
    }

    async fn remove_from_cache(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<(), AdapterError> {
        self.loaded.remove(&model.id);
        self.installed.remove(&model.id);
        let model_dir = self
            .cache_root
            .join("models")
            .join(sanitize_component(&model.id.0));
        match fs::symlink_metadata(&model_dir) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                Err(AdapterError::RuntimeFailed)
            },
            Ok(_) => fs::remove_dir_all(&model_dir).map_err(|_| AdapterError::RuntimeFailed),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(AdapterError::NotFound)
            },
            Err(_) => Err(AdapterError::RuntimeFailed),
        }
    }

    async fn create_asr_session(
        &mut self,
        _model: &ModelDescriptor,
        settings: &AsrSessionSettings,
    ) -> Result<Box<dyn StreamingAsrSession>, AdapterError> {
        Ok(Box::new(FixtureAsrSession::new(settings)))
    }

    fn offline_scopes(&self) -> Vec<ModelScope> {
        vec![ModelScope::Installed, ModelScope::Loaded]
    }
}

/// Wraps an adapter and counts outbound calls for offline enforcement tests.
#[cfg(test)]
pub struct CountingAdapter<T> {
    inner: T,
    calls: Arc<Mutex<usize>>,
}

#[cfg(test)]
impl<T> CountingAdapter<T> {
    /// Wraps an adapter.
    #[must_use]
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            calls: Arc::new(Mutex::new(0)),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl<T> FoundryAdapter for CountingAdapter<T>
where
    T: FoundryAdapter + Send,
{
    fn backend_name(&self) -> &'static str {
        self.inner.backend_name()
    }

    async fn list_catalog(&mut self) -> Result<Vec<ModelDescriptor>, AdapterError> {
        {
            let mut counter = self.calls.lock().await;
            *counter = counter.saturating_add(1);
        }
        self.inner.list_catalog().await
    }

    async fn resolve(
        &mut self,
        selector: &ModelSelector,
    ) -> Result<ModelDescriptor, AdapterError> {
        {
            let mut counter = self.calls.lock().await;
            *counter = counter.saturating_add(1);
        }
        self.inner.resolve(selector).await
    }

    async fn latest_version(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<ModelVersion, AdapterError> {
        {
            let mut counter = self.calls.lock().await;
            *counter = counter.saturating_add(1);
        }
        self.inner.latest_version(model).await
    }

    async fn list_installed(&mut self) -> Result<Vec<ModelDescriptor>, AdapterError> {
        {
            let mut counter = self.calls.lock().await;
            *counter = counter.saturating_add(1);
        }
        self.inner.list_installed().await
    }

    async fn list_loaded(&mut self) -> Result<Vec<ModelId>, AdapterError> {
        {
            let mut counter = self.calls.lock().await;
            *counter = counter.saturating_add(1);
        }
        self.inner.list_loaded().await
    }

    async fn install(
        &mut self,
        model: &ModelDescriptor,
        cancel: &tokio_util::sync::CancellationToken,
        force: bool,
    ) -> Result<InstalledArtifact, AdapterError> {
        {
            let mut counter = self.calls.lock().await;
            *counter = counter.saturating_add(1);
        }
        self.inner.install(model, cancel, force).await
    }

    async fn inspect_local_artifact(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<InstalledArtifact, AdapterError> {
        {
            let mut counter = self.calls.lock().await;
            *counter = counter.saturating_add(1);
        }
        self.inner.inspect_local_artifact(model).await
    }

    async fn load(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<(), AdapterError> {
        {
            let mut counter = self.calls.lock().await;
            *counter = counter.saturating_add(1);
        }
        self.inner.load(model).await
    }

    async fn unload(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<(), AdapterError> {
        {
            let mut counter = self.calls.lock().await;
            *counter = counter.saturating_add(1);
        }
        self.inner.unload(model).await
    }

    async fn remove_from_cache(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<(), AdapterError> {
        {
            let mut counter = self.calls.lock().await;
            *counter = counter.saturating_add(1);
        }
        self.inner.remove_from_cache(model).await
    }

    async fn create_asr_session(
        &mut self,
        model: &ModelDescriptor,
        settings: &AsrSessionSettings,
    ) -> Result<Box<dyn StreamingAsrSession>, AdapterError> {
        {
            let mut counter = self.calls.lock().await;
            *counter = counter.saturating_add(1);
        }
        self.inner.create_asr_session(model, settings).await
    }

    fn offline_scopes(&self) -> Vec<ModelScope> {
        self.inner.offline_scopes()
    }

    fn outbound_attempts(&self) -> usize {
        self.calls.try_lock().map_or(0, |calls| *calls)
    }
}

#[cfg(test)]
mod tests {
    use super::{FIXTURE_ALIAS, fixture_transcribe, hex_encode};
    use crate::{AsrSessionSettings, FixtureAsrSession, Pcm16Mono16k, StreamingAsrSession};

    #[tokio::test]
    #[allow(clippy::cast_lossless, clippy::cast_possible_truncation)]
    async fn fixture_transcription_is_deterministic_and_uses_anchor() {
        let samples: Vec<i16> = (0..16_000)
            .map(|i| {
                let phase = (i as f64 * 2.0 * std::f64::consts::PI * 440.0 / 16_000.0).sin();
                (phase * 12_000.0) as i16
            })
            .collect();
        let first = fixture_transcribe(&samples);
        let second = fixture_transcribe(&samples);
        assert_eq!(first, second);
        assert!(!first.is_empty());

        let mut session = FixtureAsrSession::new(&AsrSessionSettings::default());
        session
            .append(Pcm16Mono16k {
                samples: samples.clone(),
                session_start_us: 500_000,
            })
            .await
            .expect("append");
        let transcript = Box::new(session).finish().await.expect("finish");
        assert_eq!(transcript.events.len(), 1);
        assert_eq!(transcript.events[0].text, first);
        assert!(transcript.events[0].is_final);
        assert_eq!(transcript.events[0].start_us, 500_000);
        assert_eq!(transcript.events[0].end_us, 1_500_000);
    }

    #[tokio::test]
    async fn fixture_session_concatenates_chunk_timelines() {
        let mut session = FixtureAsrSession::new(&AsrSessionSettings::default());
        session
            .append(Pcm16Mono16k {
                samples: vec![0; 1600],
                session_start_us: 2_000_000,
            })
            .await
            .expect("first append");
        session
            .append(Pcm16Mono16k {
                samples: vec![0; 800],
                session_start_us: 2_100_000,
            })
            .await
            .expect("second append");
        let transcript = Box::new(session).finish().await.expect("finish");
        assert_eq!(transcript.events.len(), 2);
        assert_eq!(transcript.events[0].end_us, 2_100_000);
        assert_eq!(transcript.events[1].start_us, 2_100_000);
        assert_eq!(transcript.events[1].end_us, 2_150_000);
    }

    #[test]
    fn hex_encoding_round_trips() {
        assert_eq!(hex_encode(&[0xDE, 0xAD]), "dead");
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(FIXTURE_ALIAS, "fixture-nemotron-asr-0.6b");
    }
}
