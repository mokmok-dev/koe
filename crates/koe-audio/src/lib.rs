//! Platform-neutral audio boundary and allocation-free callback handoff.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use koe_core::{CapabilityState, SourceKind};
use serde::{Deserialize, Serialize};
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
    pub state: CapabilityState,
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
    pub discontinuity: bool,
    pub overflow: bool,
    pub device_lost: bool,
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
            discontinuity: false,
            overflow: false,
            device_lost: false,
            dropped_frames: 0,
            sample_count: 0,
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
                backend: "none".to_owned(),
            })
            .collect())
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
}

impl Default for CpalBackend {
    fn default() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }
}

/// Open CPAL microphone stream. The native stream is created on `start`.
pub struct CpalStream {
    device: cpal::Device,
    config: cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    source_id: [u8; 16],
    stream: Option<cpal::Stream>,
}

impl CpalBackend {
    fn microphone_devices(&self) -> Result<Vec<(String, cpal::Device, String)>, AudioError> {
        let devices = self
            .host
            .input_devices()
            .map_err(|_| AudioError::StreamBuildFailed)?;
        devices
            .enumerate()
            .map(|(index, device)| {
                let name = device.name().map_err(|_| AudioError::StreamBuildFailed)?;
                let id = format!("cpal:{index}:{name}");
                Ok((id, device, name))
            })
            .collect()
    }
}

impl AudioBackend for CpalBackend {
    type Stream = CpalStream;

    fn capabilities(&self) -> Result<Vec<AudioCapability>, AudioError> {
        let microphone = if self.microphone_devices()?.is_empty() {
            CapabilityState::Unsupported
        } else {
            CapabilityState::Supported
        };
        Ok(vec![
            AudioCapability {
                source: SourceKind::Microphone,
                state: microphone,
                backend: self.host.id().name().to_owned(),
            },
            AudioCapability {
                source: SourceKind::System,
                state: CapabilityState::Unsupported,
                backend: self.host.id().name().to_owned(),
            },
        ])
    }

    fn enumerate(
        &self,
        kind: SourceKind,
    ) -> Result<Vec<AudioDevice>, AudioError> {
        if kind != SourceKind::Microphone {
            return Ok(Vec::new());
        }
        self.microphone_devices()?
            .into_iter()
            .map(|(id, _device, display_name)| {
                Ok(AudioDevice {
                    id,
                    display_name,
                    backend: self.host.id().name().to_owned(),
                    kind,
                    persistent: false,
                })
            })
            .collect()
    }

    fn open(
        &self,
        request: &OpenSource,
    ) -> Result<Self::Stream, AudioError> {
        if request.kind != SourceKind::Microphone {
            return Err(AudioError::Unsupported);
        }
        let (id, device, _name) = self
            .microphone_devices()?
            .into_iter()
            .find(|(id, _, _)| id == &request.device_id)
            .ok_or(AudioError::DeviceNotFound)?;
        let supported = device
            .supported_input_configs()
            .map_err(|_| AudioError::PermissionDenied)?
            .filter(|range| {
                range.channels() == request.channels
                    && range.min_sample_rate().0 <= request.sample_rate
                    && range.max_sample_rate().0 >= request.sample_rate
            })
            .filter(|range| {
                matches!(
                    range.sample_format(),
                    cpal::SampleFormat::I16 | cpal::SampleFormat::U16 | cpal::SampleFormat::F32
                )
            })
            .min_by_key(|range| match range.sample_format() {
                cpal::SampleFormat::I16 => 0,
                cpal::SampleFormat::F32 => 1,
                cpal::SampleFormat::U16 => 2,
                _ => 3,
            })
            .ok_or(AudioError::UnsupportedFormat)?;
        let sample_format = supported.sample_format();
        let config = supported
            .with_sample_rate(cpal::SampleRate(request.sample_rate))
            .config();
        let mut source_id = [0_u8; 16];
        let id_bytes = id.as_bytes();
        let count = id_bytes.len().min(source_id.len());
        source_id[..count].copy_from_slice(&id_bytes[..count]);
        Ok(CpalStream {
            device,
            config,
            sample_format,
            source_id,
            stream: None,
        })
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
        stream.play().map_err(|_| AudioError::StreamRuntimeFailed)?;
        self.stream = Some(stream);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AudioError> {
        if let Some(stream) = &self.stream {
            stream
                .pause()
                .map_err(|_| AudioError::StreamRuntimeFailed)?;
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
        let sample_rate = self.config.sample_rate.0;
        let channels = self.config.channels;
        let source_id = self.source_id;
        let error_sink = Arc::clone(&sink);
        let mut sequence = 0_u64;
        let mut capture_anchor = None;
        let mut timeline_offset_ns = 0_u64;
        let mut last_capture_timestamp_ns = 0_u64;
        let mut converted = vec![0_i16; 16_384];
        self.device
            .build_input_stream(
                &self.config,
                move |samples: &[T], info: &cpal::InputCallbackInfo| {
                    if samples.len() > converted.len() {
                        let _ignored = sink.try_send(
                            FrameMetadata {
                                sequence,
                                source_id,
                                source_kind: SourceKind::Microphone,
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
                        .duration_since(&anchor)
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
                        source_kind: SourceKind::Microphone,
                        sample_rate,
                        channels,
                        sample_format: native_format,
                        payload_sample_format: NativeSampleFormat::I16,
                        capture_timestamp_ns,
                        discontinuity,
                        ..FrameMetadata::default()
                    };
                    let _ignored = sink.try_send(metadata, &converted[..samples.len()]);
                    sequence = sequence.saturating_add(1);
                },
                move |_error| {
                    let _ignored = error_sink.try_send(
                        FrameMetadata {
                            source_id,
                            source_kind: SourceKind::Microphone,
                            sample_rate,
                            channels,
                            sample_format: native_format,
                            device_lost: true,
                            ..FrameMetadata::default()
                        },
                        &[],
                    );
                },
                None,
            )
            .map_err(|_| AudioError::StreamBuildFailed)
    }
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
    discontinuities: Arc<AtomicU64>,
}

/// Non-realtime consumer handle. It can be moved independently to a worker.
pub struct FrameConsumer {
    receiver: Receiver<FrameSlot, FrameRecycle>,
    frame_samples: usize,
    dropped_frames: Arc<AtomicU64>,
    device_lost: Arc<AtomicBool>,
    discontinuities: Arc<AtomicU64>,
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
    let discontinuities = Arc::new(AtomicU64::new(0));
    Ok((
        FrameProducer {
            sender,
            frame_samples,
            dropped_frames: Arc::clone(&dropped_frames),
            device_lost: Arc::clone(&device_lost),
            discontinuities: Arc::clone(&discontinuities),
        },
        FrameConsumer {
            receiver,
            frame_samples,
            dropped_frames,
            device_lost,
            discontinuities,
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
            return Ok(());
        }
        if samples.len() > self.frame_samples {
            return Err(AudioError::UnsupportedFormat);
        }
        let mut slot = self.sender.try_send_ref().map_err(|_| {
            self.dropped_frames.fetch_add(1, Ordering::Relaxed);
            if metadata.discontinuity {
                self.discontinuities.fetch_add(1, Ordering::Relaxed);
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

    /// Takes the number of clock discontinuities since the previous observation.
    #[must_use]
    pub fn take_discontinuities(&self) -> u64 {
        self.discontinuities.swap(0, Ordering::AcqRel)
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
            Self::InvalidBuffer
            | Self::StreamBuildFailed
            | Self::StreamRuntimeFailed
            | Self::Unsupported
            | Self::UnsupportedFormat => "KOE-AUDIO-UNSUPPORTED",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioError, FrameMetadata, frame_ring};

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
}
