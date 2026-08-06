//! Platform-neutral audio boundary and allocation-free callback handoff.

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};
use std::time::Instant;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use koe_core::{Availability, CapabilityState, PermissionState, ProbeEffect, SourceKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thingbuf::{
    mpsc::blocking::{Receiver, Sender, with_recycle},
    recycling::Recycle,
};
use thiserror::Error;

/// Opaque device information suitable for persistence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AudioDevice {
    pub id: String,
    pub display_name: String,
    pub backend: String,
    pub kind: SourceKind,
    /// Whether `id` is safe to reuse after process restart.
    #[serde(default)]
    pub persistent: bool,
}

/// Runtime capability, including permission and unsupported states.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AudioCapability {
    pub source: SourceKind,
    /// Deprecated compatibility summary. New callers should inspect the
    /// orthogonal availability and permission fields.
    pub state: CapabilityState,
    #[serde(default)]
    pub availability: Availability,
    #[serde(default)]
    pub permission: PermissionState,
    #[serde(default)]
    pub probe_effect: ProbeEffect,
    pub backend: String,
}

/// Device and native format selected by explicit user action.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OpenSource {
    pub device_id: String,
    pub kind: SourceKind,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Packaging-time trust policy for host-wide Linux audio access.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackagingPolicy {
    /// Native packages may use explicitly selected host audio backends.
    #[default]
    DirectAllowed,
    /// Sandboxed packages must use a separately implemented portal adapter.
    PortalRequired,
}

/// Marker attached without formatting or logging in the callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameMetadata {
    pub sequence: u64,
    pub source_id: [u8; 16],
    pub source_kind: SourceKind,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: NativeSampleFormat,
    /// Format of `FrameSink`'s payload after adapter conversion.
    pub payload_sample_format: NativeSampleFormat,
    pub capture_timestamp_ns: u64,
    /// Process-monotonic time at callback arrival. This is shared by all
    /// streams and is used to anchor native capture clocks to one timeline.
    pub callback_arrival_timestamp_ns: u64,
    pub discontinuity: bool,
    pub overflow: bool,
    pub device_lost: bool,
    pub runtime_failure: Option<RuntimeFailure>,
    pub dropped_frames: u64,
    pub sample_count: u32,
}

impl Default for FrameMetadata {
    fn default() -> Self {
        Self {
            sequence: 0,
            source_id: [0; 16],
            source_kind: SourceKind::Microphone,
            sample_rate: 0,
            channels: 0,
            sample_format: NativeSampleFormat::I16,
            payload_sample_format: NativeSampleFormat::I16,
            capture_timestamp_ns: 0,
            callback_arrival_timestamp_ns: 0,
            discontinuity: false,
            overflow: false,
            device_lost: false,
            runtime_failure: None,
            dropped_frames: 0,
            sample_count: 0,
        }
    }
}

/// Typed asynchronous stream failure delivered outside the realtime callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeFailure {
    PermissionDenied,
    DeviceLost,
    BufferOverflow,
    StreamRuntimeFailed,
}

impl RuntimeFailure {
    #[must_use]
    pub const fn audio_error(self) -> AudioError {
        match self {
            Self::PermissionDenied => AudioError::PermissionDenied,
            Self::DeviceLost => AudioError::DeviceLost,
            Self::BufferOverflow => AudioError::BufferOverflow,
            Self::StreamRuntimeFailed => AudioError::StreamRuntimeFailed,
        }
    }
}

/// Native sample representation captured by the adapter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NativeSampleFormat {
    #[default]
    I16,
    U16,
    F32,
}

impl NativeSampleFormat {
    /// Stable manifest label for the native device payload.
    #[must_use]
    pub const fn manifest_label(self) -> &'static str {
        match self {
            Self::I16 => "signed-16-bit-pcm",
            Self::U16 => "unsigned-16-bit-pcm",
            Self::F32 => "float-32-pcm",
        }
    }
}

/// Object-safe callback boundary. Implementations must not block or allocate.
pub trait FrameSink: Send + Sync {
    /// Copies one complete callback frame into a preallocated bounded slot.
    ///
    /// # Errors
    ///
    /// Returns overflow or format errors without waiting.
    fn try_send(
        &self,
        metadata: FrameMetadata,
        samples: &[i16],
    ) -> Result<(), AudioError>;
}

/// OS adapter boundary. Foundational implementation does not leak CPAL types.
pub trait AudioBackend {
    type Stream: AudioStream;

    /// Queries runtime support rather than inferring it from the target OS alone.
    ///
    /// # Errors
    ///
    /// Returns an adapter error if capability probing fails.
    fn capabilities(&self) -> Result<Vec<AudioCapability>, AudioError>;
    /// Probes permission state separately from hardware/backend capability.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the platform permission probe cannot run.
    fn permissions(&self) -> Result<Vec<AudioCapability>, AudioError>;
    /// Lists opaque IDs for one source category, explicitly marking whether
    /// each backend supports persistence.
    ///
    /// # Errors
    ///
    /// Returns an adapter error if device enumeration fails.
    fn enumerate(
        &self,
        kind: SourceKind,
    ) -> Result<Vec<AudioDevice>, AudioError>;
    /// Opens exactly the requested device; non-persistent IDs must be selected
    /// again from the current process enumeration.
    ///
    /// # Errors
    ///
    /// Returns an adapter error if the exact device or format cannot be opened.
    fn open(
        &self,
        request: &OpenSource,
    ) -> Result<Self::Stream, AudioError>;
}

/// Stream lifecycle separated from capture callback plumbing.
pub trait AudioStream {
    /// Native device payload format selected when the stream was opened.
    fn native_sample_format(&self) -> NativeSampleFormat;
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;

    /// Starts callback delivery.
    ///
    /// # Errors
    ///
    /// Returns an adapter error if the stream cannot start.
    fn start(
        &mut self,
        sink: Box<dyn FrameSink>,
    ) -> Result<(), AudioError>;
    /// Stops callback delivery.
    ///
    /// # Errors
    ///
    /// Returns an adapter error if the stream cannot stop cleanly.
    fn stop(&mut self) -> Result<(), AudioError>;
}

/// Explicit adapter used until an OS capture implementation is linked.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnsupportedBackend;

/// Empty stream type for [`UnsupportedBackend`].
#[derive(Clone, Copy, Debug)]
pub struct UnsupportedStream;

impl AudioBackend for UnsupportedBackend {
    type Stream = UnsupportedStream;

    fn capabilities(&self) -> Result<Vec<AudioCapability>, AudioError> {
        Ok([SourceKind::Microphone, SourceKind::System]
            .into_iter()
            .map(|source| AudioCapability {
                source,
                state: CapabilityState::Unsupported,
                availability: Availability::Unsupported,
                permission: PermissionState::NotApplicable,
                probe_effect: ProbeEffect::None,
                backend: "none".to_owned(),
            })
            .collect())
    }

    fn permissions(&self) -> Result<Vec<AudioCapability>, AudioError> {
        self.capabilities()
    }

    fn enumerate(
        &self,
        _kind: SourceKind,
    ) -> Result<Vec<AudioDevice>, AudioError> {
        Ok(Vec::new())
    }

    fn open(
        &self,
        _request: &OpenSource,
    ) -> Result<Self::Stream, AudioError> {
        Err(AudioError::Unsupported)
    }
}

impl AudioStream for UnsupportedStream {
    fn native_sample_format(&self) -> NativeSampleFormat {
        NativeSampleFormat::I16
    }

    fn sample_rate(&self) -> u32 {
        0
    }

    fn channels(&self) -> u16 {
        0
    }

    fn start(
        &mut self,
        _sink: Box<dyn FrameSink>,
    ) -> Result<(), AudioError> {
        Err(AudioError::Unsupported)
    }

    fn stop(&mut self) -> Result<(), AudioError> {
        Ok(())
    }
}

/// CPAL-backed microphone adapter used by the Milestone 1 CLI.
pub struct CpalBackend {
    host: cpal::Host,
    packaging_policy: PackagingPolicy,
}

impl Default for CpalBackend {
    fn default() -> Self {
        Self {
            host: platform_default_host(),
            packaging_policy: PackagingPolicy::DirectAllowed,
        }
    }
}

/// Open CPAL microphone stream. The native stream is created on `start`.
pub struct CpalStream {
    device: cpal::Device,
    config: cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    source_id: [u8; 16],
    source_kind: SourceKind,
    stream: Option<cpal::Stream>,
}

impl CpalBackend {
    /// Creates a backend with an explicit packaging trust policy.
    #[must_use]
    pub fn with_packaging_policy(packaging_policy: PackagingPolicy) -> Self {
        Self {
            host: platform_default_host(),
            packaging_policy,
        }
    }

    fn source_devices(
        &self,
        kind: SourceKind,
    ) -> Result<Vec<(String, cpal::Device, String)>, AudioError> {
        match kind {
            SourceKind::Microphone => self
                .host
                .input_devices()
                .map_err(map_cpal_build_error)?
                .map(|device| named_device(device, kind))
                .collect(),
            SourceKind::System => self.system_devices(),
        }
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn system_devices(&self) -> Result<Vec<(String, cpal::Device, String)>, AudioError> {
        self.host
            .output_devices()
            .map_err(map_cpal_build_error)?
            .map(|device| named_device(device, SourceKind::System))
            .collect()
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    fn system_devices(&self) -> Result<Vec<(String, cpal::Device, String)>, AudioError> {
        self.host
            .input_devices()
            .map_err(map_cpal_build_error)?
            .map(|device| named_device(device, SourceKind::System))
            .collect()
    }

    fn usable_system_devices(&self) -> Result<Vec<(String, cpal::Device, String)>, AudioError> {
        if !platform_system_audio_allowed(self.packaging_policy) {
            return Ok(Vec::new());
        }
        let devices = self.source_devices(SourceKind::System)?;
        #[cfg(target_os = "linux")]
        {
            let backend = self.host.id().name().to_ascii_lowercase();
            Ok(devices
                .into_iter()
                .filter(|(_, device, name)| {
                    let name = name.to_ascii_lowercase();
                    let explicit_monitor = name.contains("monitor")
                        || name.contains("sink")
                        || name.contains("loopback")
                        || name.contains("snd_aloop");
                    if backend.contains("pipewire") {
                        device.supports_output() || explicit_monitor
                    } else {
                        explicit_monitor
                    }
                })
                .collect())
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(devices)
        }
    }
}

fn platform_default_host() -> cpal::Host {
    #[cfg(target_os = "linux")]
    {
        let mut hosts = cpal::available_hosts();
        hosts.sort_by_key(|host| linux_host_priority(host.name()));
        for host in hosts {
            if let Ok(backend) = cpal::host_from_id(host) {
                return backend;
            }
        }
    }
    cpal::default_host()
}

#[cfg(any(test, target_os = "linux"))]
fn linux_host_priority(name: &str) -> u8 {
    match name.to_ascii_lowercase().as_str() {
        "pipewire" => 0,
        "pulseaudio" => 1,
        "alsa" => 2,
        _ => 3,
    }
}

fn named_device(
    device: cpal::Device,
    kind: SourceKind,
) -> Result<(String, cpal::Device, String), AudioError> {
    let name = device
        .description()
        .map_err(map_cpal_build_error)?
        .name()
        .to_owned();
    let label = match kind {
        SourceKind::Microphone => "mic",
        SourceKind::System => "system",
    };
    let device_id = device.id().map_err(map_cpal_build_error)?;
    let id = format!("cpal:{label}:{device_id}");
    Ok((id, device, name))
}

fn supported_capture_configs(
    device: &cpal::Device,
    kind: SourceKind,
) -> Result<std::vec::IntoIter<cpal::SupportedStreamConfigRange>, AudioError> {
    let configs: Vec<cpal::SupportedStreamConfigRange> = match kind {
        SourceKind::Microphone => device
            .supported_input_configs()
            .map_err(map_cpal_build_error)?
            .collect(),
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        SourceKind::System => device
            .supported_output_configs()
            .map_err(map_cpal_build_error)?
            .collect(),
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        SourceKind::System => device
            .supported_input_configs()
            .map_err(map_cpal_build_error)?
            .collect(),
    };
    Ok(configs.into_iter())
}

impl AudioBackend for CpalBackend {
    type Stream = CpalStream;

    fn capabilities(&self) -> Result<Vec<AudioCapability>, AudioError> {
        let microphone_devices = self.source_devices(SourceKind::Microphone)?;
        let system_devices = self.usable_system_devices()?;
        let microphone = capability_state(&microphone_devices, SourceKind::Microphone);
        let system = capability_state(&system_devices, SourceKind::System);
        Ok(vec![
            AudioCapability {
                source: SourceKind::Microphone,
                state: microphone,
                availability: platform_availability(SourceKind::Microphone, microphone),
                permission: PermissionState::NotApplicable,
                probe_effect: ProbeEffect::None,
                backend: self.host.id().name().to_owned(),
            },
            AudioCapability {
                source: SourceKind::System,
                state: system,
                availability: platform_availability(SourceKind::System, system),
                permission: PermissionState::NotApplicable,
                probe_effect: ProbeEffect::None,
                backend: self.host.id().name().to_owned(),
            },
        ])
    }

    fn permissions(&self) -> Result<Vec<AudioCapability>, AudioError> {
        let backend = self.host.id().name().to_owned();
        [SourceKind::Microphone, SourceKind::System]
            .into_iter()
            .map(|source| {
                let devices = match source {
                    SourceKind::Microphone => self.source_devices(source),
                    SourceKind::System => self.usable_system_devices(),
                };
                let state = match devices {
                    Ok(devices) => permission_state(&devices, source),
                    Err(AudioError::PermissionDenied | AudioError::PermissionRequired) => {
                        CapabilityState::PermissionRequired
                    },
                    Err(error) => return Err(error),
                };
                Ok(AudioCapability {
                    source,
                    state,
                    availability: if state == CapabilityState::Unsupported {
                        Availability::Unsupported
                    } else {
                        Availability::Available
                    },
                    permission: platform_permission_state(source, state),
                    probe_effect: ProbeEffect::None,
                    backend: backend.clone(),
                })
            })
            .collect()
    }

    fn enumerate(
        &self,
        kind: SourceKind,
    ) -> Result<Vec<AudioDevice>, AudioError> {
        let devices = match kind {
            SourceKind::Microphone => self.source_devices(kind)?,
            SourceKind::System => self.usable_system_devices()?,
        };
        devices
            .into_iter()
            .map(|(id, _device, display_name)| {
                Ok(AudioDevice {
                    id,
                    display_name,
                    backend: self.host.id().name().to_owned(),
                    kind,
                    persistent: true,
                })
            })
            .collect()
    }

    fn open(
        &self,
        request: &OpenSource,
    ) -> Result<Self::Stream, AudioError> {
        let candidates = match request.kind {
            SourceKind::Microphone => self.source_devices(request.kind)?,
            SourceKind::System => self.usable_system_devices()?,
        };
        let (id, device, _name) = self
            .source_devices(request.kind)?
            .into_iter()
            .find(|(id, _, _)| id == &request.device_id)
            .ok_or(AudioError::DeviceNotFound)?;
        if !candidates.iter().any(|(candidate, _, _)| candidate == &id) {
            return Err(AudioError::Unsupported);
        }
        let supported = supported_capture_configs(&device, request.kind)?
            .filter(|range| {
                matches!(
                    range.sample_format(),
                    cpal::SampleFormat::I16 | cpal::SampleFormat::U16 | cpal::SampleFormat::F32
                )
            })
            .min_by_key(|range| {
                let channel_penalty = u32::from(range.channels().abs_diff(request.channels));
                let rate_distance = if request.sample_rate < range.min_sample_rate() {
                    range.min_sample_rate() - request.sample_rate
                } else {
                    request.sample_rate.saturating_sub(range.max_sample_rate())
                };
                let format_penalty = match range.sample_format() {
                    cpal::SampleFormat::I16 => 0,
                    cpal::SampleFormat::F32 => 1,
                    cpal::SampleFormat::U16 => 2,
                    _ => 3,
                };
                (channel_penalty, rate_distance, format_penalty)
            })
            .ok_or(AudioError::UnsupportedFormat)?;
        let sample_format = supported.sample_format();
        let selected_rate = request
            .sample_rate
            .clamp(supported.min_sample_rate(), supported.max_sample_rate());
        let config = supported.with_sample_rate(selected_rate).config();
        let source_id = stable_source_id(&id);
        Ok(CpalStream {
            device,
            config,
            sample_format,
            source_id,
            source_kind: request.kind,
            stream: None,
        })
    }
}

const fn availability_from_legacy(state: CapabilityState) -> Availability {
    match state {
        CapabilityState::Supported | CapabilityState::PermissionRequired => Availability::Available,
        CapabilityState::Unsupported => Availability::Unsupported,
    }
}

// This is const on Linux, but the macOS branch performs a runtime OS probe.
#[allow(clippy::missing_const_for_fn)]
fn platform_availability(
    source: SourceKind,
    state: CapabilityState,
) -> Availability {
    #[cfg(target_os = "macos")]
    if source == SourceKind::System && !macos_14_6_or_newer() {
        return Availability::OsTooOld;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = source;
    availability_from_legacy(state)
}

// This is const on Linux, but the macOS branch queries AVFoundation.
#[allow(clippy::missing_const_for_fn)]
fn platform_permission_state(
    source: SourceKind,
    state: CapabilityState,
) -> PermissionState {
    #[cfg(target_os = "macos")]
    if source == SourceKind::Microphone {
        return macos_microphone_permission_state();
    }
    permission_from_legacy(state, source)
}

const fn permission_from_legacy(
    state: CapabilityState,
    source: SourceKind,
) -> PermissionState {
    match state {
        CapabilityState::Supported => {
            if matches!(source, SourceKind::System) {
                PermissionState::Unobservable
            } else {
                PermissionState::Granted
            }
        },
        CapabilityState::PermissionRequired => PermissionState::NotDetermined,
        CapabilityState::Unsupported => PermissionState::NotApplicable,
    }
}

fn capability_state(
    devices: &[(String, cpal::Device, String)],
    kind: SourceKind,
) -> CapabilityState {
    if devices.is_empty() {
        return CapabilityState::Unsupported;
    }
    for (_, device, _) in devices {
        match supported_capture_configs(device, kind) {
            Ok(configs) => {
                if configs.into_iter().next().is_some() {
                    return CapabilityState::Supported;
                }
            },
            Err(AudioError::PermissionDenied | AudioError::PermissionRequired) => {
                // The device/backend capability exists even though its separate
                // permission probe is currently denied.
                return CapabilityState::Supported;
            },
            Err(_) => {},
        }
    }
    CapabilityState::Unsupported
}

fn permission_state(
    devices: &[(String, cpal::Device, String)],
    kind: SourceKind,
) -> CapabilityState {
    if devices.is_empty() {
        return CapabilityState::Unsupported;
    }
    let mut permission_required = false;
    for (_, device, _) in devices {
        match supported_capture_configs(device, kind) {
            Ok(configs) => {
                if configs.into_iter().next().is_some() {
                    return CapabilityState::Supported;
                }
            },
            Err(AudioError::PermissionDenied | AudioError::PermissionRequired) => {
                permission_required = true;
            },
            Err(_) => {},
        }
    }
    if permission_required {
        CapabilityState::PermissionRequired
    } else {
        CapabilityState::Unsupported
    }
}

impl AudioStream for CpalStream {
    fn native_sample_format(&self) -> NativeSampleFormat {
        match self.sample_format {
            cpal::SampleFormat::U16 => NativeSampleFormat::U16,
            cpal::SampleFormat::F32 => NativeSampleFormat::F32,
            _ => NativeSampleFormat::I16,
        }
    }

    fn sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    fn channels(&self) -> u16 {
        self.config.channels
    }

    fn start(
        &mut self,
        sink: Box<dyn FrameSink>,
    ) -> Result<(), AudioError> {
        if self.stream.is_some() {
            return Ok(());
        }
        let sink: Arc<dyn FrameSink> = Arc::from(sink);
        let stream = match self.sample_format {
            cpal::SampleFormat::I16 => self.build_stream::<i16>(sink, NativeSampleFormat::I16)?,
            cpal::SampleFormat::U16 => self.build_stream::<u16>(sink, NativeSampleFormat::U16)?,
            cpal::SampleFormat::F32 => self.build_stream::<f32>(sink, NativeSampleFormat::F32)?,
            _ => return Err(AudioError::UnsupportedFormat),
        };
        stream.play().map_err(map_cpal_runtime_error)?;
        self.stream = Some(stream);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AudioError> {
        if let Some(stream) = &self.stream {
            stream.pause().map_err(map_cpal_runtime_error)?;
        }
        self.stream = None;
        Ok(())
    }
}

impl CpalStream {
    fn build_stream<T>(
        &self,
        sink: Arc<dyn FrameSink>,
        native_format: NativeSampleFormat,
    ) -> Result<cpal::Stream, AudioError>
    where
        T: cpal::SizedSample,
        i16: cpal::FromSample<T>,
    {
        let sample_rate = self.config.sample_rate;
        let channels = self.config.channels;
        let source_id = self.source_id;
        let source_kind = self.source_kind;
        let error_sink = Arc::clone(&sink);
        let mut sequence = 0_u64;
        let mut capture_anchor = None;
        let mut timeline_offset_ns = 0_u64;
        let mut last_capture_timestamp_ns = 0_u64;
        let mut converted = vec![0_i16; 16_384];
        self.device
            .build_input_stream(
                self.config,
                move |samples: &[T], info: &cpal::InputCallbackInfo| {
                    let callback_arrival_timestamp_ns = process_monotonic_ns();
                    if samples.len() > converted.len() {
                        let _ignored = sink.try_send(
                            FrameMetadata {
                                sequence,
                                source_id,
                                source_kind,
                                sample_rate,
                                channels,
                                sample_format: native_format,
                                overflow: true,
                                dropped_frames: 1,
                                ..FrameMetadata::default()
                            },
                            &[],
                        );
                        return;
                    }
                    for (output, input) in converted.iter_mut().zip(samples) {
                        *output = input.to_sample::<i16>();
                    }
                    let capture = info.timestamp().capture;
                    let anchor = *capture_anchor.get_or_insert(capture);
                    let elapsed = capture
                        .checked_duration_since(anchor)
                        .and_then(|duration| u64::try_from(duration.as_nanos()).ok());
                    let discontinuity = elapsed.is_none();
                    let capture_timestamp_ns = if let Some(elapsed) = elapsed {
                        timeline_offset_ns.saturating_add(elapsed)
                    } else {
                        capture_anchor = Some(capture);
                        timeline_offset_ns = last_capture_timestamp_ns;
                        last_capture_timestamp_ns
                    };
                    last_capture_timestamp_ns = capture_timestamp_ns;
                    let metadata = FrameMetadata {
                        sequence,
                        source_id,
                        source_kind,
                        sample_rate,
                        channels,
                        sample_format: native_format,
                        payload_sample_format: NativeSampleFormat::I16,
                        capture_timestamp_ns,
                        callback_arrival_timestamp_ns,
                        discontinuity,
                        ..FrameMetadata::default()
                    };
                    let _ignored = sink.try_send(metadata, &converted[..samples.len()]);
                    sequence = sequence.saturating_add(1);
                },
                move |error| {
                    let failure = classify_cpal_runtime_error_kind(error.kind());
                    let _ignored = error_sink.try_send(
                        FrameMetadata {
                            source_id,
                            source_kind,
                            sample_rate,
                            channels,
                            sample_format: native_format,
                            device_lost: failure == RuntimeFailure::DeviceLost,
                            overflow: failure == RuntimeFailure::BufferOverflow,
                            runtime_failure: Some(failure),
                            ..FrameMetadata::default()
                        },
                        &[],
                    );
                },
                None,
            )
            .map_err(map_cpal_build_error)
    }
}

fn stable_source_id(id: &str) -> [u8; 16] {
    let digest = Sha256::digest(id.as_bytes());
    let mut source_id = [0_u8; 16];
    source_id.copy_from_slice(&digest[..16]);
    source_id
}

fn process_monotonic_ns() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN
        .get_or_init(Instant::now)
        .elapsed()
        .as_nanos()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Current process-monotonic timestamp used by callback metadata.
#[must_use]
pub fn process_timeline_now_ns() -> u64 {
    process_monotonic_ns()
}

#[cfg(target_os = "macos")]
fn platform_system_audio_allowed(_policy: PackagingPolicy) -> bool {
    macos_14_6_or_newer()
}

#[cfg(target_os = "linux")]
const fn platform_system_audio_allowed(policy: PackagingPolicy) -> bool {
    // Runtime heuristics are diagnostic only. The distribution policy is the
    // authority because absence of sandbox markers cannot prove host access.
    matches!(policy, PackagingPolicy::DirectAllowed)
}

#[cfg(test)]
fn linux_cgroup_is_sandboxed(cgroup: &str) -> bool {
    if cgroup.trim().is_empty() {
        return true;
    }
    let normalized = cgroup.to_ascii_lowercase();
    [
        "flatpak",
        "snap.",
        "docker",
        "libpod",
        "podman",
        "kubepods",
        "lxc",
        "systemd-nspawn",
        "firejail",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_system_audio_allowed(_policy: PackagingPolicy) -> bool {
    true
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn macos_microphone_permission_state() -> PermissionState {
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};

    // SAFETY: AVMediaTypeAudio is an immutable AVFoundation framework
    // constant, and the selector explicitly accepts that media type.
    let Some(media_type) = (unsafe { AVMediaTypeAudio }) else {
        return PermissionState::Unobservable;
    };
    // SAFETY: The class method is side-effect-free for a valid audio media
    // type; it does not request access or create a capture device.
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) };
    match status {
        AVAuthorizationStatus::Authorized => PermissionState::Granted,
        AVAuthorizationStatus::NotDetermined => PermissionState::NotDetermined,
        AVAuthorizationStatus::Denied => PermissionState::Denied,
        AVAuthorizationStatus::Restricted => PermissionState::Restricted,
        _ => PermissionState::Unobservable,
    }
}

#[cfg(target_os = "macos")]
fn macos_14_6_or_newer() -> bool {
    let Ok(output) = std::process::Command::new("/usr/bin/sw_vers")
        .args(["-productVersion"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    version_is_at_least_14_6(std::str::from_utf8(&output.stdout).unwrap_or_default())
}

#[cfg(any(test, target_os = "macos"))]
fn version_is_at_least_14_6(version: &str) -> bool {
    let mut parts = version.trim().split('.');
    let major = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    let minor = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    major > 14 || (major == 14 && minor >= 6)
}

fn map_cpal_build_error(error: cpal::Error) -> AudioError {
    let kind = error.kind();
    drop(error);
    match kind {
        cpal::ErrorKind::PermissionDenied => AudioError::PermissionDenied,
        cpal::ErrorKind::DeviceNotAvailable => AudioError::DeviceNotFound,
        cpal::ErrorKind::UnsupportedConfig | cpal::ErrorKind::UnsupportedOperation => {
            AudioError::UnsupportedFormat
        },
        _ => AudioError::StreamBuildFailed,
    }
}

fn map_cpal_runtime_error(error: cpal::Error) -> AudioError {
    let failure = classify_cpal_runtime_error_kind(error.kind());
    drop(error);
    failure.audio_error()
}

const fn classify_cpal_runtime_error_kind(kind: cpal::ErrorKind) -> RuntimeFailure {
    match kind {
        cpal::ErrorKind::PermissionDenied => RuntimeFailure::PermissionDenied,
        cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::DeviceChanged => {
            RuntimeFailure::DeviceLost
        },
        cpal::ErrorKind::Xrun => RuntimeFailure::BufferOverflow,
        _ => RuntimeFailure::StreamRuntimeFailed,
    }
}

/// Bounded drift estimate between a source clock and the session timeline.
#[derive(Clone, Copy, Debug)]
pub struct DriftEstimator {
    anchor_timestamp_ns: Option<u64>,
    observed_frames: u64,
    sample_rate: u32,
    filtered_ppm: f64,
}

impl DriftEstimator {
    /// Creates an estimator for one native source rate.
    ///
    /// # Errors
    ///
    /// Rejects a zero sample rate.
    pub const fn new(sample_rate: u32) -> Result<Self, AudioError> {
        if sample_rate == 0 {
            return Err(AudioError::UnsupportedFormat);
        }
        Ok(Self {
            anchor_timestamp_ns: None,
            observed_frames: 0,
            sample_rate,
            filtered_ppm: 0.0,
        })
    }

    /// Observes a callback endpoint and returns the smoothed clock drift in ppm.
    /// Discontinuities reset the anchor instead of producing an extreme correction.
    #[allow(clippy::cast_precision_loss)]
    pub fn observe(
        &mut self,
        timestamp_ns: u64,
        frame_count: u64,
        discontinuity: bool,
    ) -> f64 {
        if discontinuity || self.anchor_timestamp_ns.is_none() {
            self.anchor_timestamp_ns = Some(timestamp_ns);
            self.observed_frames = frame_count;
            self.filtered_ppm = 0.0;
            return 0.0;
        }
        let anchor = self.anchor_timestamp_ns.unwrap_or(timestamp_ns);
        let elapsed_ns = timestamp_ns.saturating_sub(anchor);
        if elapsed_ns == 0 {
            return self.filtered_ppm;
        }
        let expected_ns =
            self.observed_frames as f64 * 1_000_000_000.0 / f64::from(self.sample_rate);
        let measured_ppm = ((expected_ns / elapsed_ns as f64) - 1.0) * 1_000_000.0;
        let bounded = measured_ppm.clamp(-2_000.0, 2_000.0);
        self.filtered_ppm = self.filtered_ppm.mul_add(0.9, bounded * 0.1);
        self.observed_frames = self.observed_frames.saturating_add(frame_count);
        self.filtered_ppm
    }
}

/// Stateful channel mapper and linear resampler producing canonical 16 kHz mono PCM.
pub struct CanonicalNormalizer {
    channels: u16,
    source_rate: u32,
    phase: f64,
    drift_ppm: f64,
}

impl CanonicalNormalizer {
    /// Creates a canonical normalizer.
    ///
    /// # Errors
    ///
    /// Rejects zero channels or sample rate.
    pub const fn new(
        source_rate: u32,
        channels: u16,
    ) -> Result<Self, AudioError> {
        if source_rate == 0 || channels == 0 {
            return Err(AudioError::UnsupportedFormat);
        }
        Ok(Self {
            channels,
            source_rate,
            phase: 0.0,
            drift_ppm: 0.0,
        })
    }

    /// Sets a bounded asynchronous correction derived from [`DriftEstimator`].
    pub const fn set_drift_ppm(
        &mut self,
        drift_ppm: f64,
    ) {
        self.drift_ppm = drift_ppm.clamp(-2_000.0, 2_000.0);
    }

    /// Downmixes, applies drift correction, and resamples into caller storage.
    ///
    /// Returns the number of output samples. No internal allocation occurs.
    ///
    /// # Errors
    ///
    /// Rejects incomplete interleaved frames or insufficient output capacity.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::while_float
    )]
    pub fn process(
        &mut self,
        input: &[i16],
        output: &mut [i16],
    ) -> Result<usize, AudioError> {
        let channels = usize::from(self.channels);
        if !input.len().is_multiple_of(channels) {
            return Err(AudioError::UnsupportedFormat);
        }
        let frames = input.len() / channels;
        if frames == 0 {
            return Ok(0);
        }
        let step = f64::from(self.source_rate) / 16_000.0 * (1.0 + self.drift_ppm / 1_000_000.0);
        let mut produced = 0_usize;
        while self.phase < frames as f64 {
            let frame = self.phase.floor() as usize;
            let next = frame.saturating_add(1).min(frames - 1);
            let fraction = self.phase - frame as f64;
            let current = downmix_frame(input, frame, channels);
            let following = downmix_frame(input, next, channels);
            let sample = (f64::from(following) - f64::from(current))
                .mul_add(fraction, f64::from(current))
                .round()
                .clamp(f64::from(i16::MIN), f64::from(i16::MAX));
            let slot = output.get_mut(produced).ok_or(AudioError::InvalidBuffer)?;
            *slot = sample as i16;
            produced += 1;
            self.phase += step;
        }
        self.phase -= frames as f64;
        Ok(produced)
    }
}

fn downmix_frame(
    input: &[i16],
    frame: usize,
    channels: usize,
) -> i16 {
    let start = frame * channels;
    let sum = input[start..start + channels]
        .iter()
        .map(|sample| i64::from(*sample))
        .sum::<i64>();
    let divisor = i64::try_from(channels).unwrap_or(i64::MAX);
    i16::try_from((sum / divisor).clamp(i64::from(i16::MIN), i64::from(i16::MAX))).unwrap_or_else(
        |_| {
            if sum.is_negative() {
                i16::MIN
            } else {
                i16::MAX
            }
        },
    )
}

/// Mixes two canonical tracks with saturating addition.
pub fn mix_canonical(
    microphone: &[i16],
    system: &[i16],
    output: &mut [i16],
) -> usize {
    let count = microphone.len().min(system.len()).min(output.len());
    for index in 0..count {
        output[index] = microphone[index].saturating_add(system[index]);
    }
    count
}

#[derive(Debug)]
struct FrameSlot {
    metadata: FrameMetadata,
    samples: Box<[i16]>,
}

#[derive(Clone, Copy)]
struct FrameRecycle {
    frame_samples: usize,
}

impl Recycle<FrameSlot> for FrameRecycle {
    fn new_element(&self) -> FrameSlot {
        FrameSlot {
            metadata: FrameMetadata::default(),
            samples: vec![0; self.frame_samples].into_boxed_slice(),
        }
    }

    fn recycle(
        &self,
        slot: &mut FrameSlot,
    ) {
        slot.metadata = FrameMetadata::default();
    }
}

/// Allocation-free producer handle intended for the realtime callback.
pub struct FrameProducer {
    sender: Sender<FrameSlot, FrameRecycle>,
    frame_samples: usize,
    dropped_frames: Arc<AtomicU64>,
    device_lost: Arc<AtomicBool>,
    runtime_failure: Arc<AtomicU8>,
    discontinuities: Arc<AtomicU64>,
    discontinuity_timestamp_ns: Arc<AtomicU64>,
}

/// Non-realtime consumer handle. It can be moved independently to a worker.
pub struct FrameConsumer {
    receiver: Receiver<FrameSlot, FrameRecycle>,
    frame_samples: usize,
    dropped_frames: Arc<AtomicU64>,
    device_lost: Arc<AtomicBool>,
    runtime_failure: Arc<AtomicU8>,
    discontinuities: Arc<AtomicU64>,
    discontinuity_timestamp_ns: Arc<AtomicU64>,
}

/// Creates preallocated, split producer/consumer handles.
///
/// # Errors
///
/// Rejects zero capacity or zero-sized frames.
pub fn frame_ring(
    capacity: usize,
    frame_samples: usize,
) -> Result<(FrameProducer, FrameConsumer), AudioError> {
    if capacity == 0 || frame_samples == 0 {
        return Err(AudioError::InvalidBuffer);
    }
    let recycle = FrameRecycle { frame_samples };
    let (sender, receiver) = with_recycle(capacity, recycle);
    let dropped_frames = Arc::new(AtomicU64::new(0));
    let device_lost = Arc::new(AtomicBool::new(false));
    let runtime_failure = Arc::new(AtomicU8::new(0));
    let discontinuities = Arc::new(AtomicU64::new(0));
    let discontinuity_timestamp_ns = Arc::new(AtomicU64::new(0));
    Ok((
        FrameProducer {
            sender,
            frame_samples,
            dropped_frames: Arc::clone(&dropped_frames),
            device_lost: Arc::clone(&device_lost),
            runtime_failure: Arc::clone(&runtime_failure),
            discontinuities: Arc::clone(&discontinuities),
            discontinuity_timestamp_ns: Arc::clone(&discontinuity_timestamp_ns),
        },
        FrameConsumer {
            receiver,
            frame_samples,
            dropped_frames,
            device_lost,
            runtime_failure,
            discontinuities,
            discontinuity_timestamp_ns,
        },
    ))
}

impl FrameProducer {
    /// Rejects the current frame when full and increments the durable metric.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::UnsupportedFormat`] for a mismatched payload and
    /// [`AudioError::BufferOverflow`] when the bounded ring is full.
    pub fn try_push(
        &self,
        metadata: FrameMetadata,
        samples: &[i16],
    ) -> Result<(), AudioError> {
        if metadata.device_lost {
            self.device_lost.store(true, Ordering::Release);
        }
        if let Some(failure) = metadata.runtime_failure {
            let incoming = runtime_failure_code(failure);
            let _ignored =
                self.runtime_failure
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                        (runtime_failure_priority(incoming) > runtime_failure_priority(current))
                            .then_some(incoming)
                    });
        }
        if metadata.device_lost || metadata.runtime_failure.is_some() {
            return Ok(());
        }
        if samples.len() > self.frame_samples {
            return Err(AudioError::UnsupportedFormat);
        }
        let mut slot = self.sender.try_send_ref().map_err(|_| {
            self.dropped_frames.fetch_add(1, Ordering::Relaxed);
            if metadata.discontinuity {
                self.discontinuities.fetch_add(1, Ordering::Relaxed);
                self.discontinuity_timestamp_ns
                    .store(metadata.callback_arrival_timestamp_ns, Ordering::Release);
            }
            AudioError::BufferOverflow
        })?;
        slot.metadata = FrameMetadata {
            sample_count: u32::try_from(samples.len())
                .map_err(|_| AudioError::UnsupportedFormat)?,
            ..metadata
        };
        slot.samples[..samples.len()].copy_from_slice(samples);
        Ok(())
    }

    /// Number of frames rejected since construction.
    #[must_use]
    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames.load(Ordering::Relaxed)
    }
}

impl FrameSink for FrameProducer {
    fn try_send(
        &self,
        metadata: FrameMetadata,
        samples: &[i16],
    ) -> Result<(), AudioError> {
        self.try_push(metadata, samples)
    }
}

impl FrameConsumer {
    /// Copies the oldest complete frame into caller-owned storage.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::UnsupportedFormat`] when the output size differs.
    pub fn try_pop(
        &self,
        output: &mut [i16],
    ) -> Result<Option<FrameMetadata>, AudioError> {
        if output.len() < self.frame_samples {
            return Err(AudioError::UnsupportedFormat);
        }
        let Ok(slot) = self.receiver.try_recv_ref() else {
            return Ok(None);
        };
        let sample_count = usize::try_from(slot.metadata.sample_count)
            .map_err(|_| AudioError::UnsupportedFormat)?;
        output[..sample_count].copy_from_slice(&slot.samples[..sample_count]);
        let metadata = slot.metadata;
        Ok(Some(metadata))
    }

    /// Number of rejected frames since construction.
    #[must_use]
    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames.load(Ordering::Relaxed)
    }

    /// Takes the number of rejected frames since the previous observation.
    #[must_use]
    pub fn take_dropped_frames(&self) -> u64 {
        self.dropped_frames.swap(0, Ordering::AcqRel)
    }

    /// Takes a device-lost control event independently of ring capacity.
    #[must_use]
    pub fn take_device_lost(&self) -> bool {
        self.device_lost.swap(false, Ordering::AcqRel)
    }

    /// Takes a fatal/control stream event independently of audio ring capacity.
    #[must_use]
    pub fn take_runtime_failure(&self) -> Option<RuntimeFailure> {
        runtime_failure_from_code(self.runtime_failure.swap(0, Ordering::AcqRel))
    }

    /// Takes the number of clock discontinuities since the previous observation.
    #[must_use]
    pub fn take_discontinuities(&self) -> u64 {
        self.discontinuities.swap(0, Ordering::AcqRel)
    }

    /// Timestamp associated with the most recently dropped discontinuity.
    #[must_use]
    pub fn discontinuity_timestamp_ns(&self) -> u64 {
        self.discontinuity_timestamp_ns.load(Ordering::Acquire)
    }

    /// Current buffered frame count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.receiver.len()
    }

    /// Whether there are no buffered frames.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }
}

const fn runtime_failure_code(failure: RuntimeFailure) -> u8 {
    match failure {
        RuntimeFailure::PermissionDenied => 1,
        RuntimeFailure::DeviceLost => 2,
        RuntimeFailure::BufferOverflow => 3,
        RuntimeFailure::StreamRuntimeFailed => 4,
    }
}

const fn runtime_failure_from_code(code: u8) -> Option<RuntimeFailure> {
    match code {
        1 => Some(RuntimeFailure::PermissionDenied),
        2 => Some(RuntimeFailure::DeviceLost),
        3 => Some(RuntimeFailure::BufferOverflow),
        4 => Some(RuntimeFailure::StreamRuntimeFailed),
        _ => None,
    }
}

const fn runtime_failure_priority(code: u8) -> u8 {
    match code {
        // Permission and generic runtime failures are fatal and must never be
        // hidden by a later xrun notification.
        1 | 4 => 3,
        2 => 2,
        3 => 1,
        _ => 0,
    }
}

/// Stable platform adapter failures.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum AudioError {
    #[error("audio source is unsupported")]
    Unsupported,
    #[error("audio permission is required")]
    PermissionRequired,
    #[error("audio permission was denied")]
    PermissionDenied,
    #[error("audio device was not found")]
    DeviceNotFound,
    #[error("audio device was lost")]
    DeviceLost,
    #[error("audio format is unsupported")]
    UnsupportedFormat,
    #[error("audio stream could not be built")]
    StreamBuildFailed,
    #[error("audio stream failed")]
    StreamRuntimeFailed,
    #[error("audio callback buffer overflowed")]
    BufferOverflow,
    #[error("audio clock was discontinuous")]
    ClockDiscontinuity,
    #[error("audio buffer configuration is invalid")]
    InvalidBuffer,
}

impl AudioError {
    /// Stable presentation code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::PermissionDenied | Self::PermissionRequired => "KOE-AUDIO-PERMISSION-DENIED",
            Self::DeviceLost | Self::DeviceNotFound => "KOE-AUDIO-DEVICE-LOST",
            Self::BufferOverflow => "KOE-AUDIO-OVERFLOW",
            Self::ClockDiscontinuity => "KOE-AUDIO-CLOCK-DISCONTINUITY",
            Self::StreamBuildFailed => "KOE-AUDIO-STREAM-BUILD-FAILED",
            Self::StreamRuntimeFailed => "KOE-AUDIO-STREAM-RUNTIME-FAILED",
            Self::InvalidBuffer | Self::Unsupported | Self::UnsupportedFormat => {
                "KOE-AUDIO-UNSUPPORTED"
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AudioError, CanonicalNormalizer, DriftEstimator, FrameMetadata, RuntimeFailure,
        classify_cpal_runtime_error_kind, frame_ring, linux_cgroup_is_sandboxed,
        linux_host_priority, mix_canonical, stable_source_id, version_is_at_least_14_6,
    };

    #[test]
    fn cpal_runtime_errors_remain_typed() {
        assert_eq!(
            classify_cpal_runtime_error_kind(cpal::ErrorKind::PermissionDenied),
            RuntimeFailure::PermissionDenied
        );
        assert_eq!(
            classify_cpal_runtime_error_kind(cpal::ErrorKind::DeviceNotAvailable),
            RuntimeFailure::DeviceLost
        );
        assert_eq!(
            classify_cpal_runtime_error_kind(cpal::ErrorKind::DeviceChanged),
            RuntimeFailure::DeviceLost
        );
        assert_eq!(
            classify_cpal_runtime_error_kind(cpal::ErrorKind::Xrun),
            RuntimeFailure::BufferOverflow
        );
        assert_eq!(
            classify_cpal_runtime_error_kind(cpal::ErrorKind::BackendError),
            RuntimeFailure::StreamRuntimeFailed
        );
    }

    #[test]
    fn linux_sandbox_detection_does_not_depend_only_on_environment() {
        assert!(linux_cgroup_is_sandboxed(
            "0::/user.slice/app-flatpak-org.example.App.scope"
        ));
        assert!(linux_cgroup_is_sandboxed(
            "0::/kubepods.slice/kubepods-burstable.slice"
        ));
        assert!(!linux_cgroup_is_sandboxed("0::/user.slice/user-1000.slice"));
    }

    #[test]
    fn overflow_control_cannot_overwrite_a_pending_fatal_failure() {
        let (producer, consumer) = frame_ring(1, 2).expect("ring");
        producer
            .try_push(
                FrameMetadata {
                    runtime_failure: Some(RuntimeFailure::StreamRuntimeFailed),
                    ..FrameMetadata::default()
                },
                &[],
            )
            .expect("fatal control");
        producer
            .try_push(
                FrameMetadata {
                    runtime_failure: Some(RuntimeFailure::BufferOverflow),
                    ..FrameMetadata::default()
                },
                &[],
            )
            .expect("overflow control");
        assert_eq!(
            consumer.take_runtime_failure(),
            Some(RuntimeFailure::StreamRuntimeFailed)
        );
    }

    #[test]
    fn stream_build_and_runtime_codes_are_distinct() {
        assert_eq!(
            AudioError::StreamBuildFailed.code(),
            "KOE-AUDIO-STREAM-BUILD-FAILED"
        );
        assert_eq!(
            AudioError::StreamRuntimeFailed.code(),
            "KOE-AUDIO-STREAM-RUNTIME-FAILED"
        );
    }

    #[test]
    fn full_ring_rejects_current_frame_and_preserves_oldest() {
        let (producer, consumer) = frame_ring(1, 2).unwrap_or_else(|error| panic!("{error}"));
        let first = FrameMetadata {
            sequence: 7,
            ..FrameMetadata::default()
        };
        assert_eq!(producer.try_push(first, &[1, 2]), Ok(()));
        assert_eq!(
            producer.try_push(FrameMetadata::default(), &[3, 4]),
            Err(AudioError::BufferOverflow)
        );
        let mut output = [0; 2];
        let popped = consumer
            .try_pop(&mut output)
            .unwrap_or_else(|error| panic!("{error}"))
            .unwrap_or_else(|| panic!("frame"));
        assert_eq!(popped.sequence, first.sequence);
        assert_eq!(popped.sample_count, 2);
        assert_eq!(output, [1, 2]);
        assert_eq!(consumer.dropped_frames(), 1);
    }

    #[test]
    fn mismatched_payload_is_rejected() {
        let (producer, consumer) = frame_ring(2, 2).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            producer.try_push(FrameMetadata::default(), &[1, 2, 3]),
            Err(AudioError::UnsupportedFormat)
        );
        assert!(consumer.is_empty());
    }

    #[test]
    fn device_lost_bypasses_a_full_audio_ring() {
        let (producer, consumer) = frame_ring(1, 2).unwrap_or_else(|error| panic!("{error}"));
        producer
            .try_push(FrameMetadata::default(), &[1, 2])
            .unwrap_or_else(|error| panic!("{error}"));
        producer
            .try_push(
                FrameMetadata {
                    device_lost: true,
                    ..FrameMetadata::default()
                },
                &[],
            )
            .unwrap_or_else(|error| panic!("{error}"));

        assert!(consumer.take_device_lost());
        assert_eq!(consumer.len(), 1);
    }

    #[test]
    fn fatal_runtime_failure_bypasses_a_full_audio_ring() {
        let (producer, consumer) = frame_ring(1, 2).unwrap_or_else(|error| panic!("{error}"));
        producer
            .try_push(FrameMetadata::default(), &[1, 2])
            .unwrap_or_else(|error| panic!("{error}"));
        producer
            .try_push(
                FrameMetadata {
                    runtime_failure: Some(RuntimeFailure::PermissionDenied),
                    ..FrameMetadata::default()
                },
                &[],
            )
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            consumer.take_runtime_failure(),
            Some(RuntimeFailure::PermissionDenied)
        );
        assert_eq!(consumer.len(), 1);
    }

    #[test]
    fn source_ids_hash_the_entire_backend_identifier() {
        assert_ne!(
            stable_source_id("cpal:mic:abcdefghijklmnop-one"),
            stable_source_id("cpal:mic:abcdefghijklmnop-two")
        );
        assert_eq!(stable_source_id("same"), stable_source_id("same"));
    }

    #[test]
    fn macos_system_capture_gate_starts_at_14_6() {
        assert!(!version_is_at_least_14_6("14.5.9"));
        assert!(version_is_at_least_14_6("14.6"));
        assert!(version_is_at_least_14_6("15.0"));
        assert!(!version_is_at_least_14_6("invalid"));
    }

    #[test]
    fn linux_backend_order_prefers_pipewire_then_pulse_then_alsa() {
        assert!(linux_host_priority("PipeWire") < linux_host_priority("PulseAudio"));
        assert!(linux_host_priority("PulseAudio") < linux_host_priority("ALSA"));
        assert!(linux_host_priority("ALSA") < linux_host_priority("JACK"));
    }

    #[test]
    fn dropped_frame_counter_can_be_checkpointed_without_double_counting() {
        let (producer, consumer) = frame_ring(1, 1).unwrap_or_else(|error| panic!("{error}"));
        producer
            .try_push(FrameMetadata::default(), &[1])
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            producer.try_push(FrameMetadata::default(), &[2]),
            Err(AudioError::BufferOverflow)
        );
        assert_eq!(consumer.take_dropped_frames(), 1);
        assert_eq!(consumer.take_dropped_frames(), 0);
    }

    #[test]
    fn discontinuity_side_channel_only_counts_dropped_markers() {
        let (producer, consumer) = frame_ring(1, 1).unwrap_or_else(|error| panic!("{error}"));
        let marker = FrameMetadata {
            discontinuity: true,
            ..FrameMetadata::default()
        };
        producer
            .try_push(marker, &[1])
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(consumer.take_discontinuities(), 0);
        assert_eq!(
            producer.try_push(marker, &[2]),
            Err(AudioError::BufferOverflow)
        );
        assert_eq!(consumer.take_discontinuities(), 1);
    }

    #[test]
    fn stereo_48k_is_downmixed_and_resampled_to_16k() {
        let mut normalizer =
            CanonicalNormalizer::new(48_000, 2).unwrap_or_else(|error| panic!("{error}"));
        let input = [
            1_000, 3_000, 2_000, 4_000, 3_000, 5_000, 4_000, 6_000, 5_000, 7_000, 6_000, 8_000,
        ];
        let mut output = [0_i16; 4];
        let count = normalizer
            .process(&input, &mut output)
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(count, 2);
        assert_eq!(&output[..count], &[2_000, 5_000]);
    }

    #[test]
    fn canonical_mix_clips_instead_of_wrapping() {
        let mut output = [0_i16; 2];
        assert_eq!(
            mix_canonical(&[30_000, -30_000], &[10_000, -10_000], &mut output),
            2
        );
        assert_eq!(output, [i16::MAX, i16::MIN]);
    }

    #[test]
    fn drift_estimator_is_bounded_and_resets_on_gap() {
        let mut estimator = DriftEstimator::new(48_000).unwrap_or_else(|error| panic!("{error}"));
        assert!(estimator.observe(1_000, 480, false).abs() < f64::EPSILON);
        let estimate = estimator.observe(10_999_000, 480, false);
        assert!(estimate.abs() < 2_000.0);
        assert!(estimator.observe(20_000_000, 480, true).abs() < f64::EPSILON);
    }
}
