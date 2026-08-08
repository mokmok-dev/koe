//! Real adapter over the supported `foundry-local-sdk` crate.
//!
//! SDK catalog, model, and live-audio handles remain private to this module.
//! The adapter maps them to koe descriptors, verified artifact inventories,
//! and the model-neutral [`StreamingAsrSession`] port.

use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use foundry_local_sdk::{
    FoundryLocalConfig, FoundryLocalError, FoundryLocalManager, LiveAudioTranscriptionOptions,
    LiveAudioTranscriptionResponse, LiveAudioTranscriptionSession, LiveAudioTranscriptionStream,
    Model, ModelInfo,
};
use futures_util::{FutureExt as _, StreamExt as _};

use crate::{
    adapter::{
        AdapterError, ArtifactValidationError, AsrError, AsrEvent, AsrSessionSettings,
        FinalTranscript, FoundryAdapter, InstalledArtifact, InstalledFile, Pcm16Mono16k,
        StreamingAsrSession,
    },
    types::{Alias, ModelDescriptor, ModelId, ModelScope, ModelSelector, ModelVersion},
};

/// Maximum directory depth inventoried inside one runtime-owned model path.
const MAX_CACHE_DEPTH: usize = 4;
/// Maximum files admitted to one digest inventory.
const MAX_INVENTORY_FILES: usize = 1_024;
/// Maximum entries examined while inventorying one exact SDK model path.
const MAX_CACHE_ENTRIES: usize = 16_384;
/// Wraps the process-wide SDK manager without exposing it through koe APIs.
pub struct FoundryLocalAdapter {
    inner: Option<&'static FoundryLocalManager>,
}

impl FoundryLocalAdapter {
    /// Creates an adapter; native runtime initialization remains lazy.
    #[must_use]
    pub const fn new() -> Self {
        Self { inner: None }
    }

    fn manager(&mut self) -> Result<&'static FoundryLocalManager, AdapterError> {
        if let Some(manager) = self.inner {
            return Ok(manager);
        }
        let manager = FoundryLocalManager::create(FoundryLocalConfig::new("koe_foundry_local"))
            .map_err(map_initialization_error)?;
        self.inner = Some(manager);
        Ok(manager)
    }

    fn descriptor_from_model(model: &Model) -> ModelDescriptor {
        descriptor_from_info(model.info())
    }

    async fn exact_model(
        &mut self,
        descriptor: &ModelDescriptor,
    ) -> Result<Arc<Model>, AdapterError> {
        let manager = self.manager()?;
        let model = manager
            .catalog()
            .get_model_variant(&descriptor.id.0)
            .await
            .map_err(map_runtime_error)?;
        if !descriptor_matches_model(descriptor, &model) {
            return Err(AdapterError::RuntimeFailed);
        }
        Ok(model)
    }

    /// Ensure execution providers are registered with the native core.
    ///
    /// The native core bundles onnxruntime libraries but requires explicit
    /// registration via `download_and_register_eps`. Without this step
    /// inference silently produces no output (empty responses).
    ///
    /// Registration is idempotent — already-registered EPs are
    /// skipped without error.
    async fn ensure_eps_registered(&mut self) -> Result<(), AdapterError> {
        let manager = self.manager()?;
        // Register all available execution providers. On Apple Silicon
        // this will be WebGpuExecutionProvider; on Intel it will be
        // CPUExecutionProvider. Passing `None` lets the native core
        // discover and register whichever EPs are available.
        manager.download_and_register_eps(None).await.map_err(|e| {
            eprintln!("[koe] EP registration failed: {e:?}");
            AdapterError::RuntimeFailed
        })?;
        Ok(())
    }

    fn artifact_from_path(
        descriptor: &ModelDescriptor,
        artifact_root: &Path,
        created_by_operation: bool,
    ) -> Result<InstalledArtifact, AdapterError> {
        let artifact_metadata =
            std::fs::symlink_metadata(artifact_root).map_err(|_| AdapterError::RuntimeFailed)?;
        if artifact_metadata.file_type().is_symlink() || !artifact_metadata.is_dir() {
            return Err(AdapterError::RuntimeFailed);
        }
        let cache_root = artifact_root
            .parent()
            .filter(|parent| *parent != artifact_root)
            .ok_or(AdapterError::RuntimeFailed)?
            .to_path_buf();
        let cache_metadata =
            std::fs::symlink_metadata(&cache_root).map_err(|_| AdapterError::RuntimeFailed)?;
        if cache_metadata.file_type().is_symlink() || !cache_metadata.is_dir() {
            return Err(AdapterError::RuntimeFailed);
        }

        let mut files = Vec::new();
        let mut total_bytes = 0_u64;
        let mut entries_examined = 0_usize;
        collect_files(
            &cache_root,
            artifact_root,
            0,
            &mut files,
            &mut total_bytes,
            &mut entries_examined,
        )?;
        if files.is_empty() {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::EmptyInventory,
            ));
        }
        files.sort_by(|left, right| left.relative_path().cmp(right.relative_path()));
        Ok(InstalledArtifact {
            cache_root,
            model_id: descriptor.id.clone(),
            files,
            created_by_operation,
            operation_lease: None,
        })
    }
}

impl Default for FoundryLocalAdapter {
    fn default() -> Self {
        Self::new()
    }
}

fn descriptor_from_info(info: &ModelInfo) -> ModelDescriptor {
    let variant = info
        .runtime
        .as_ref()
        .map(|runtime| runtime.execution_provider.clone())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| info.model_type.clone());
    let provider = info
        .publisher
        .clone()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| info.provider_type.clone());
    let license_id = info
        .license
        .clone()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    let license_description = info
        .license_description
        .clone()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| license_id.clone());
    ModelDescriptor {
        id: ModelId::new(info.id.clone()),
        alias: Alias(info.alias.clone()),
        version: ModelVersion::new(info.version.to_string()),
        variant,
        provider,
        license_id,
        license_description,
        source: info.uri.clone(),
        size_mb: info.file_size_mb.unwrap_or(0),
        task: info.task.clone().unwrap_or_default(),
    }
}

fn descriptor_matches_model(
    descriptor: &ModelDescriptor,
    model: &Model,
) -> bool {
    descriptor_matches_persisted(
        descriptor,
        &FoundryLocalAdapter::descriptor_from_model(model),
    )
}

fn descriptor_matches_persisted(
    descriptor: &ModelDescriptor,
    current: &ModelDescriptor,
) -> bool {
    descriptor.id == current.id
        && descriptor.alias == current.alias
        && descriptor.version == current.version
        && descriptor.variant == current.variant
        && descriptor.provider == current.provider
        && descriptor.license_id == current.license_id
        && descriptor.license_description == current.license_description
        && descriptor.source == current.source
}

fn collect_files(
    root: &Path,
    directory: &Path,
    depth: usize,
    files: &mut Vec<InstalledFile>,
    total_size: &mut u64,
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
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::Symlink,
            ));
        }
        if file_type.is_dir() {
            collect_files(root, &path, depth + 1, files, total_size, entries_examined)?;
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
            files.push(InstalledFile::try_from_cache_path_blocking(root, relative)?);
        } else {
            return Err(AdapterError::InvalidArtifact(
                ArtifactValidationError::InvalidPath,
            ));
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

fn map_initialization_error(error: FoundryLocalError) -> AdapterError {
    let mapped = match &error {
        FoundryLocalError::LibraryLoad { .. } => AdapterError::Unavailable,
        _ => AdapterError::RuntimeFailed,
    };
    drop(error);
    mapped
}

fn map_catalog_error(error: FoundryLocalError) -> AdapterError {
    let mapped = match &error {
        FoundryLocalError::LibraryLoad { .. } => AdapterError::Unavailable,
        FoundryLocalError::ModelOperation { .. } | FoundryLocalError::Validation { .. } => {
            AdapterError::NotFound
        },
        _ => AdapterError::CatalogFailed,
    };
    drop(error);
    mapped
}

fn map_download_error(error: FoundryLocalError) -> AdapterError {
    let mapped = match &error {
        FoundryLocalError::LibraryLoad { .. } => AdapterError::Unavailable,
        _ => AdapterError::DownloadFailed,
    };
    drop(error);
    mapped
}

fn map_runtime_error(error: FoundryLocalError) -> AdapterError {
    let mapped = match &error {
        FoundryLocalError::LibraryLoad { .. } => AdapterError::Unavailable,
        _ => AdapterError::RuntimeFailed,
    };
    drop(error);
    mapped
}

fn map_asr_error(error: FoundryLocalError) -> AsrError {
    let mapped = match &error {
        FoundryLocalError::LibraryLoad { .. } => AsrError::Unavailable,
        FoundryLocalError::Validation { .. } => AsrError::SessionNotActive,
        _ => AsrError::RuntimeFailed,
    };
    drop(error);
    mapped
}

const fn sdk_download_required(
    cache_existed: bool,
    force: bool,
) -> Result<bool, AdapterError> {
    // SDK 1.2.3 has cancellation but no atomic force/replace option. Refuse a
    // forced refresh of a usable cache entry rather than deleting it before a
    // fallible download. A future SDK force primitive can replace this policy.
    if cache_existed && force {
        return Err(AdapterError::ForceRedownloadUnsupported);
    }
    Ok(!cache_existed)
}

fn sdk_session_settings(
    settings: &AsrSessionSettings
) -> Result<LiveAudioTranscriptionOptions, AdapterError> {
    settings
        .validate()
        .map_err(|_| AdapterError::InvalidSettings)?;
    Ok(LiveAudioTranscriptionOptions {
        sample_rate: 16_000,
        channels: 1,
        bits_per_sample: 16,
        language: settings.language.clone(),
        push_queue_capacity: settings.push_queue_capacity,
    })
}

fn pcm_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len().saturating_mul(2));
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

#[derive(Clone, Copy)]
struct SegmentMapping {
    id: uuid::Uuid,
    start_us: u64,
    end_us: u64,
}

struct ResponseMapper {
    anchor_us: Option<u64>,
    fed_end_us: u64,
    last_final_end_us: Option<u64>,
    segments: HashMap<String, SegmentMapping>,
    fallback_segment: Option<SegmentMapping>,
}

impl ResponseMapper {
    fn new() -> Self {
        Self {
            anchor_us: None,
            fed_end_us: 0,
            last_final_end_us: None,
            segments: HashMap::new(),
            fallback_segment: None,
        }
    }

    fn record_chunk(
        &mut self,
        chunk: &Pcm16Mono16k,
    ) {
        self.anchor_us.get_or_insert(chunk.session_start_us);
        let duration_us = u64::try_from(chunk.samples.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(1_000_000)
            / 16_000;
        self.fed_end_us = self
            .fed_end_us
            .max(chunk.session_start_us.saturating_add(duration_us));
    }

    fn map(
        &mut self,
        response: LiveAudioTranscriptionResponse,
    ) -> Option<AsrEvent> {
        let text = response.content.first()?.text.clone();
        if text.is_empty() {
            return None;
        }
        let anchor_us = self.anchor_us.unwrap_or(0);
        let previous = match response.id.as_ref() {
            Some(id) => self.segments.get(id).copied(),
            None => self.fallback_segment,
        };
        let start_us = response
            .start_time
            .and_then(seconds_to_micros)
            .map(|offset| anchor_us.saturating_add(offset))
            .or_else(|| previous.map(|segment| segment.start_us))
            .unwrap_or_else(|| self.last_final_end_us.unwrap_or(anchor_us));
        let end_us = response
            .end_time
            .and_then(seconds_to_micros)
            .map(|offset| anchor_us.saturating_add(offset))
            .or_else(|| previous.map(|segment| segment.end_us))
            .unwrap_or_else(|| self.fed_end_us.max(start_us))
            .max(start_us);
        let mapping = SegmentMapping {
            id: previous.map_or_else(uuid::Uuid::new_v4, |segment| segment.id),
            start_us,
            end_us,
        };
        if let Some(id) = response.id {
            self.segments.insert(id, mapping);
        } else if response.is_final {
            self.fallback_segment = None;
        } else {
            self.fallback_segment = Some(mapping);
        }
        if response.is_final {
            self.last_final_end_us = Some(end_us);
        }
        Some(AsrEvent {
            segment_id: mapping.id,
            text,
            start_us,
            end_us,
            is_final: response.is_final,
        })
    }
}

fn seconds_to_micros(seconds: f64) -> Option<u64> {
    let duration = Duration::try_from_secs_f64(seconds).ok()?;
    u64::try_from(duration.as_micros()).ok()
}

struct FoundryStreamingAsrSession {
    session: LiveAudioTranscriptionSession,
    stream: LiveAudioTranscriptionStream,
    mapper: ResponseMapper,
    events: Vec<AsrEvent>,
    max_chunk_samples: usize,
    active: bool,
}

impl FoundryStreamingAsrSession {
    fn accept_response(
        &mut self,
        response: LiveAudioTranscriptionResponse,
    ) -> Option<AsrEvent> {
        let event = self.mapper.map(response)?;
        self.events.push(event.clone());
        Some(event)
    }
}

#[async_trait]
impl StreamingAsrSession for FoundryStreamingAsrSession {
    async fn append(
        &mut self,
        chunk: Pcm16Mono16k,
    ) -> Result<(), AsrError> {
        if !self.active {
            return Err(AsrError::SessionNotActive);
        }
        if chunk.samples.len() > self.max_chunk_samples {
            return Err(AsrError::InvalidInput);
        }
        if chunk.samples.is_empty() {
            return Ok(());
        }
        let bytes = pcm_le_bytes(&chunk.samples);
        self.session
            .append(&bytes, None)
            .await
            .map_err(map_asr_error)?;
        self.mapper.record_chunk(&chunk);
        Ok(())
    }

    async fn poll_results(&mut self) -> Result<Option<AsrEvent>, AsrError> {
        loop {
            let pending = self.stream.next().now_or_never();
            match pending {
                None | Some(None) => return Ok(None),
                Some(Some(Err(error))) => return Err(map_asr_error(error)),
                Some(Some(Ok(response))) => {
                    if let Some(event) = self.accept_response(response) {
                        return Ok(Some(event));
                    }
                },
            }
        }
    }

    async fn finish(mut self: Box<Self>) -> Result<FinalTranscript, AsrError> {
        if !self.active {
            return Err(AsrError::SessionNotActive);
        }
        let stop_result = self.session.stop(None).await.map_err(map_asr_error);
        self.active = false;
        let mut stream_result = Ok(());
        while let Some(result) = self.stream.next().await {
            match result {
                Ok(response) => {
                    let _ignored = self.accept_response(response);
                },
                Err(error) => {
                    stream_result = Err(map_asr_error(error));
                    break;
                },
            }
        }
        stop_result?;
        stream_result?;
        Ok(FinalTranscript {
            events: std::mem::take(&mut self.events),
        })
    }
}

#[async_trait]
impl FoundryAdapter for FoundryLocalAdapter {
    fn backend_name(&self) -> &'static str {
        "foundry-local"
    }

    async fn list_catalog(&mut self) -> Result<Vec<ModelDescriptor>, AdapterError> {
        let manager = self.manager()?;
        let models = manager
            .catalog()
            .get_models()
            .await
            .map_err(map_catalog_error)?;
        let mut descriptors = models
            .iter()
            .flat_map(|model| model.variants())
            .map(|variant| Self::descriptor_from_model(&variant))
            .collect::<Vec<_>>();
        descriptors.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(descriptors)
    }

    async fn resolve(
        &mut self,
        selector: &ModelSelector,
    ) -> Result<ModelDescriptor, AdapterError> {
        let manager = self.manager()?;
        let model = match selector {
            ModelSelector::Alias(alias) => manager.catalog().get_model(alias).await,
            ModelSelector::Id(id) => manager.catalog().get_model_variant(&id.0).await,
        }
        .map_err(map_catalog_error)?;
        Ok(Self::descriptor_from_model(&model))
    }

    async fn latest_version(
        &mut self,
        model: &ModelDescriptor,
    ) -> Result<ModelVersion, AdapterError> {
        let manager = self.manager()?;
        let current = manager
            .catalog()
            .get_model_variant(&model.id.0)
            .await
            .map_err(map_catalog_error)?;
        let latest = manager
            .catalog()
            .get_latest_version(&current)
            .await
            .map_err(map_catalog_error)?;
        Ok(ModelVersion::new(latest.info().version.to_string()))
    }

    async fn list_installed(&mut self) -> Result<Vec<ModelDescriptor>, AdapterError> {
        let manager = self.manager()?;
        let mut descriptors = manager
            .catalog()
            .get_cached_models()
            .await
            .map_err(map_runtime_error)?
            .iter()
            .map(|model| Self::descriptor_from_model(model))
            .collect::<Vec<_>>();
        descriptors.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(descriptors)
    }

    async fn list_loaded(&mut self) -> Result<Vec<ModelId>, AdapterError> {
        let manager = self.manager()?;
        let mut ids = manager
            .catalog()
            .get_loaded_models()
            .await
            .map_err(map_runtime_error)?
            .iter()
            .map(|model| ModelId::new(model.id().to_owned()))
            .collect::<Vec<_>>();
        ids.sort();
        Ok(ids)
    }

    async fn install(
        &mut self,
        descriptor: &ModelDescriptor,
        cancel: &tokio_util::sync::CancellationToken,
        force: bool,
    ) -> Result<InstalledArtifact, AdapterError> {
        if cancel.is_cancelled() {
            return Err(AdapterError::DownloadFailed);
        }
        let model = self.exact_model(descriptor).await?;
        // Register execution providers before downloading the model.
        // The native core requires EPs to be registered for inference
        // to work, even though the onnxruntime libraries are bundled.
        self.ensure_eps_registered().await?;
        let cache_existed = model.is_cached().await.map_err(map_runtime_error)?;
        if sdk_download_required(cache_existed, force)? {
            let cancel_flag = Arc::new(AtomicBool::new(false));
            let watcher_flag = Arc::clone(&cancel_flag);
            let watcher_token = cancel.clone();
            let watcher = tokio::spawn(async move {
                watcher_token.cancelled().await;
                watcher_flag.store(true, Ordering::Release);
            });
            let download_result = model
                .download_builder()
                .cancel(cancel_flag)
                .run()
                .await
                .map_err(map_download_error);
            watcher.abort();
            download_result?;
        }
        if !model.is_cached().await.map_err(map_runtime_error)? {
            return Err(AdapterError::DownloadFailed);
        }
        let path = model.path().await.map_err(map_runtime_error)?;
        let descriptor = descriptor.clone();
        tokio::task::spawn_blocking(move || {
            Self::artifact_from_path(&descriptor, &path, !cache_existed)
        })
        .await
        .map_err(|_| AdapterError::RuntimeFailed)?
    }

    async fn inspect_local_artifact(
        &mut self,
        descriptor: &ModelDescriptor,
    ) -> Result<InstalledArtifact, AdapterError> {
        let model = self.exact_model(descriptor).await?;
        if !model.is_cached().await.map_err(map_runtime_error)? {
            return Err(AdapterError::NotFound);
        }
        let path = model.path().await.map_err(map_runtime_error)?;
        let descriptor = descriptor.clone();
        tokio::task::spawn_blocking(move || Self::artifact_from_path(&descriptor, &path, false))
            .await
            .map_err(|_| AdapterError::RuntimeFailed)?
    }

    async fn load(
        &mut self,
        descriptor: &ModelDescriptor,
    ) -> Result<(), AdapterError> {
        let model = self.exact_model(descriptor).await?;
        if !model.is_cached().await.map_err(map_runtime_error)? {
            return Err(AdapterError::NotFound);
        }
        // Ensure execution providers are registered before loading.
        // The native core requires EPs for inference.
        self.ensure_eps_registered().await?;
        model.load().await.map_err(map_runtime_error)
    }

    async fn unload(
        &mut self,
        descriptor: &ModelDescriptor,
    ) -> Result<(), AdapterError> {
        self.exact_model(descriptor)
            .await?
            .unload()
            .await
            .map(|_| ())
            .map_err(map_runtime_error)
    }

    async fn remove_from_cache(
        &mut self,
        descriptor: &ModelDescriptor,
    ) -> Result<(), AdapterError> {
        let model = self.exact_model(descriptor).await?;
        if !model.is_cached().await.map_err(map_runtime_error)? {
            return Err(AdapterError::NotFound);
        }
        model
            .remove_from_cache()
            .await
            .map(|_| ())
            .map_err(map_runtime_error)
    }

    async fn create_asr_session(
        &mut self,
        descriptor: &ModelDescriptor,
        settings: &AsrSessionSettings,
    ) -> Result<Box<dyn StreamingAsrSession>, AdapterError> {
        let model = self.exact_model(descriptor).await?;
        if !model.is_loaded().await.map_err(map_runtime_error)? {
            return Err(AdapterError::RuntimeFailed);
        }
        let max_chunk_samples = usize::try_from(settings.chunk_ms.saturating_mul(16))
            .map_err(|_| AdapterError::InvalidSettings)?;
        let mut session = model
            .create_audio_client()
            .create_live_transcription_session();
        session.settings = sdk_session_settings(settings)?;
        session.start(None).await.map_err(map_runtime_error)?;
        let stream = match session.get_stream().await {
            Ok(stream) => stream,
            Err(error) => {
                let _ignored = session.stop(None).await;
                return Err(map_runtime_error(error));
            },
        };
        Ok(Box::new(FoundryStreamingAsrSession {
            session,
            stream,
            mapper: ResponseMapper::new(),
            events: Vec::new(),
            max_chunk_samples,
            active: true,
        }))
    }

    fn offline_scopes(&self) -> Vec<ModelScope> {
        vec![ModelScope::Installed, ModelScope::Loaded]
    }
}

#[cfg(test)]
mod tests {
    use foundry_local_sdk::{DeviceType, ModelInfo, Runtime};
    use tempfile::TempDir;

    use super::{
        AdapterError, FoundryLocalAdapter, ResponseMapper, count_cache_entry, descriptor_from_info,
        descriptor_matches_persisted, pcm_le_bytes, sdk_download_required, sdk_session_settings,
    };
    use crate::{AsrSessionSettings, FoundryAdapter, ModelSelector, Pcm16Mono16k};

    fn model_info() -> ModelInfo {
        ModelInfo {
            id: "Nvidia/Nemotron/cuda".to_owned(),
            name: "nemotron".to_owned(),
            version: 7,
            alias: "nemotron-speech-streaming-en-0.6b".to_owned(),
            display_name: Some("Nemotron".to_owned()),
            provider_type: "FoundryLocal".to_owned(),
            uri: "https://example.invalid/model".to_owned(),
            model_type: "ONNX".to_owned(),
            prompt_template: None,
            publisher: Some("NVIDIA".to_owned()),
            model_settings: None,
            license: Some("nvidia-open-model-license".to_owned()),
            license_description: Some("NVIDIA Open Model License".to_owned()),
            cached: false,
            task: Some("automatic-speech-recognition".to_owned()),
            runtime: Some(Runtime {
                device_type: DeviceType::GPU,
                execution_provider: "CUDAExecutionProvider".to_owned(),
            }),
            file_size_mb: Some(1_234),
            supports_tool_calling: None,
            max_output_tokens: None,
            min_fl_version: Some("1.2.3".to_owned()),
            created_at_unix: 0,
            context_length: None,
            input_modalities: Some("audio".to_owned()),
            output_modalities: Some("text".to_owned()),
            capabilities: None,
        }
    }

    #[test]
    fn sdk_metadata_maps_to_the_koe_descriptor() {
        let descriptor = descriptor_from_info(&model_info());
        assert_eq!(descriptor.id.0, "Nvidia/Nemotron/cuda");
        assert_eq!(descriptor.alias.0, "nemotron-speech-streaming-en-0.6b");
        assert_eq!(descriptor.version.0, "7");
        assert_eq!(descriptor.variant, "CUDAExecutionProvider");
        assert_eq!(descriptor.provider, "NVIDIA");
        assert_eq!(descriptor.license_id, "nvidia-open-model-license");
        assert_eq!(descriptor.license_description, "NVIDIA Open Model License");
        assert_eq!(descriptor.size_mb, 1_234);
        assert_eq!(descriptor.task, "automatic-speech-recognition");
    }

    #[test]
    fn runtime_matching_uses_only_manifest_persisted_metadata() {
        let current = descriptor_from_info(&model_info());
        let mut persisted = current.clone();
        persisted.size_mb = 0;
        persisted.task = "automatic-speech-recognition".to_owned();
        assert!(descriptor_matches_persisted(&persisted, &current));

        persisted.version.0 = "8".to_owned();
        assert!(!descriptor_matches_persisted(&persisted, &current));
    }

    #[test]
    fn session_settings_are_canonical_and_bounded() {
        let settings = AsrSessionSettings {
            language: Some("en".to_owned()),
            ..AsrSessionSettings::default()
        };
        let mapped = sdk_session_settings(&settings).expect("settings");
        assert_eq!(mapped.sample_rate, 16_000);
        assert_eq!(mapped.channels, 1);
        assert_eq!(mapped.bits_per_sample, 16);
        assert_eq!(mapped.language.as_deref(), Some("en"));
        assert_eq!(mapped.push_queue_capacity, 100);

        for capacity in [0, crate::MAX_ASR_PUSH_QUEUE_CAPACITY + 1] {
            let invalid = AsrSessionSettings {
                push_queue_capacity: capacity,
                ..AsrSessionSettings::default()
            };
            assert_eq!(
                sdk_session_settings(&invalid).expect_err("invalid settings"),
                AdapterError::InvalidSettings
            );
        }
    }

    #[test]
    fn forced_sdk_refresh_never_removes_a_usable_cache_entry() {
        assert_eq!(sdk_download_required(false, false), Ok(true));
        assert_eq!(sdk_download_required(false, true), Ok(true));
        assert_eq!(sdk_download_required(true, false), Ok(false));
        assert_eq!(
            sdk_download_required(true, true),
            Err(AdapterError::ForceRedownloadUnsupported)
        );
    }

    #[test]
    fn canonical_samples_map_to_little_endian_bytes() {
        assert_eq!(
            pcm_le_bytes(&[i16::MIN, -1, 0, 1, i16::MAX]),
            vec![0, 128, 255, 255, 0, 0, 1, 0, 255, 127]
        );
    }

    #[test]
    fn live_results_map_to_the_session_timeline_and_reuse_ids() {
        let mut mapper = ResponseMapper::new();
        mapper.record_chunk(&Pcm16Mono16k {
            samples: vec![0; 16_000],
            session_start_us: 5_000_000,
        });
        let interim = foundry_local_sdk::LiveAudioTranscriptionResponse::from_json(
            r#"{"is_final":false,"text":"hello","start_time":0.25,"end_time":0.75,"id":"segment-1"}"#,
        )
        .expect("response");
        let final_response = foundry_local_sdk::LiveAudioTranscriptionResponse::from_json(
            r#"{"is_final":true,"text":"hello world","start_time":0.25,"end_time":1.0,"id":"segment-1"}"#,
        )
        .expect("response");
        let interim = mapper.map(interim).expect("mapped interim");
        let final_event = mapper.map(final_response).expect("mapped final");
        assert_eq!(interim.segment_id, final_event.segment_id);
        assert_eq!(interim.start_us, 5_250_000);
        assert_eq!(interim.end_us, 5_750_000);
        assert_eq!(final_event.end_us, 6_000_000);
        assert!(final_event.is_final);
    }

    #[test]
    fn idless_timestampless_revisions_keep_identity_and_bounds_until_final() {
        let mut mapper = ResponseMapper::new();
        mapper.record_chunk(&Pcm16Mono16k {
            samples: vec![0; 8_000],
            session_start_us: 2_000_000,
        });
        let response = |is_final: bool, text: &str| {
            foundry_local_sdk::LiveAudioTranscriptionResponse::from_json(&format!(
                r#"{{"is_final":{is_final},"text":"{text}"}}"#
            ))
            .expect("response")
        };

        let first = mapper.map(response(false, "hel")).expect("first interim");
        mapper.record_chunk(&Pcm16Mono16k {
            samples: vec![0; 8_000],
            session_start_us: 2_500_000,
        });
        let revised = mapper
            .map(response(false, "hello"))
            .expect("revised interim");
        let final_event = mapper.map(response(true, "hello world")).expect("final");

        assert_eq!(first.segment_id, revised.segment_id);
        assert_eq!(first.segment_id, final_event.segment_id);
        assert_eq!((first.start_us, first.end_us), (2_000_000, 2_500_000));
        assert_eq!(
            (revised.start_us, revised.end_us),
            (first.start_us, first.end_us)
        );
        assert_eq!(
            (final_event.start_us, final_event.end_us),
            (first.start_us, first.end_us)
        );

        let next = mapper.map(response(false, "next")).expect("next segment");
        assert_ne!(next.segment_id, first.segment_id);
        assert_eq!((next.start_us, next.end_us), (2_500_000, 3_000_000));
    }

    #[test]
    fn timestamp_less_sdk_id_revisions_reuse_their_original_bounds() {
        let mut mapper = ResponseMapper::new();
        mapper.record_chunk(&Pcm16Mono16k {
            samples: vec![0; 16_000],
            session_start_us: 7_000_000,
        });
        let first = foundry_local_sdk::LiveAudioTranscriptionResponse::from_json(
            r#"{"is_final":false,"text":"one","id":"stable"}"#,
        )
        .expect("response");
        let final_response = foundry_local_sdk::LiveAudioTranscriptionResponse::from_json(
            r#"{"is_final":true,"text":"one two","id":"stable"}"#,
        )
        .expect("response");
        let first = mapper.map(first).expect("first");
        let final_event = mapper.map(final_response).expect("final");
        assert_eq!(first.segment_id, final_event.segment_id);
        assert_eq!((first.start_us, first.end_us), (7_000_000, 8_000_000));
        assert_eq!(
            (final_event.start_us, final_event.end_us),
            (first.start_us, first.end_us)
        );
    }

    #[test]
    fn sdk_model_path_inventory_is_exact_and_relative() {
        let cache = TempDir::new().expect("cache");
        let artifact_root = cache.path().join("exact-model-id");
        std::fs::create_dir(&artifact_root).expect("artifact");
        std::fs::write(artifact_root.join("model.bin"), b"model").expect("model");
        let descriptor = crate::FixtureFoundryAdapter::fixture_descriptor();
        let artifact = FoundryLocalAdapter::artifact_from_path(&descriptor, &artifact_root, true)
            .expect("inventory");
        assert_eq!(artifact.cache_root(), cache.path());
        assert_eq!(artifact.files().len(), 1);
        assert_eq!(
            artifact.files()[0].relative_path(),
            "exact-model-id/model.bin"
        );
        assert!(artifact.was_created_by_install());
    }

    #[test]
    fn global_cache_entry_limit_is_inclusive_and_then_rejected() {
        let mut examined = super::MAX_CACHE_ENTRIES - 1;
        count_cache_entry(&mut examined).expect("exact entry limit");
        assert_eq!(examined, super::MAX_CACHE_ENTRIES);
        assert!(count_cache_entry(&mut examined).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn exact_model_inventory_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let cache = TempDir::new().expect("cache");
        let artifact_root = cache.path().join("exact-model-id");
        std::fs::create_dir(&artifact_root).expect("artifact");
        let outside = cache.path().join("outside.bin");
        std::fs::write(&outside, b"outside").expect("outside");
        symlink(&outside, artifact_root.join("model.bin")).expect("symlink");
        let descriptor = crate::FixtureFoundryAdapter::fixture_descriptor();
        assert!(
            FoundryLocalAdapter::artifact_from_path(&descriptor, &artifact_root, false).is_err()
        );
    }

    /// Requires the native runtime and a pre-cached model selected with
    /// `KOE_FOUNDRY_LIVE_TEST_MODEL`; CI/unit runs never download a model.
    #[tokio::test]
    #[ignore = "requires Foundry Local native runtime and a pre-cached live ASR model"]
    async fn live_runtime_creates_starts_stops_and_unloads_a_session() {
        let Ok(alias) = std::env::var("KOE_FOUNDRY_LIVE_TEST_MODEL") else {
            return;
        };
        let mut adapter = FoundryLocalAdapter::new();
        let descriptor = adapter
            .resolve(&alias.parse::<ModelSelector>().expect("selector"))
            .await
            .expect("resolve");
        assert!(
            adapter
                .list_installed()
                .await
                .expect("cached models")
                .iter()
                .any(|candidate| candidate == &descriptor),
            "live test model must already be cached"
        );
        adapter.load(&descriptor).await.expect("load");
        let mut session = adapter
            .create_asr_session(&descriptor, &AsrSessionSettings::default())
            .await
            .expect("session");
        session
            .append(Pcm16Mono16k {
                samples: vec![0; 1_600],
                session_start_us: 0,
            })
            .await
            .expect("append");
        let _pending_event = session.poll_results().await.expect("poll live results");
        let transcript = session.finish().await.expect("stop");
        for event in transcript.events {
            assert!(event.start_us <= event.end_us);
        }
        adapter.unload(&descriptor).await.expect("unload");
    }
}
