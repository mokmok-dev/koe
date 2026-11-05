//! The single-file recorder and its per-source queues.

use std::path::Path;
use std::time::Duration;

use koe_capture_core::{FrameQueue, Source, StreamSpec};
use koe_record_core::{Channels, Error, Input, Mixer, RecordingSpec};

use crate::file::{self, AudioFile};
use crate::format::Format;

/// Frames mixed per block written to the file.
const MAX_BLOCK_FRAMES: usize = 4_096;

/// What a finished recording holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    /// Frames mixed into the file.
    pub frames: u64,
    /// Samples that the hard clamp pulled back into `-1.0..=1.0`.
    pub clipped_samples: u64,
    /// Playback time the mixed frames represent.
    pub duration: Duration,
}

/// Writes the microphone and the system audio into one file.
///
/// A recorder owns no threads: [`write`](Self::write) takes `&mut self`. A
/// caller that reads both blocking [`CaptureStream`](koe_capture_core::CaptureStream)s
/// on two threads must serialize access, for example with a
/// [`Mutex<Recorder>`](std::sync::Mutex); each read block is mixed as it
/// arrives.
///
/// Pausing a source compresses the paused interval out of the file: the mix
/// advances only as fast as every live source delivers.
pub struct Recorder {
    spec: RecordingSpec,
    mixer: Mixer,
    file: Box<dyn AudioFile>,
    sources: [SourceSlot; 2],
    microphone: Vec<f32>,
    system_audio: Vec<f32>,
    mixed: Vec<f32>,
    frames: u64,
    clipped_samples: u64,
}

#[derive(Debug)]
struct SourceSlot {
    registration: Option<Registration>,
    ended: bool,
}

/// The stream a source was registered with.
#[derive(Debug)]
struct Registration {
    spec: StreamSpec,
    channels: Channels,
    queue: FrameQueue,
}

impl Recorder {
    /// Creates a recording at `path`, inferring the format from its extension.
    ///
    /// The file is created or truncated and its headers are written before the
    /// call returns.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownFormat`] for an extension no format claims,
    /// [`Error::UnsupportedRate`] when the format cannot carry the sample rate,
    /// and the file's or the codec's error when the recording cannot start.
    pub fn create(path: impl AsRef<Path>, spec: RecordingSpec) -> Result<Self, Error> {
        let path = path.as_ref();
        let extension = path
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default();
        let format = Format::from_extension(extension).ok_or_else(|| Error::UnknownFormat {
            extension: extension.into(),
        })?;
        format.validate(spec)?;
        let file = file::open(path, spec, format)?;

        let width = usize::from(spec.channels().count()) * MAX_BLOCK_FRAMES;
        Ok(Self {
            spec,
            mixer: Mixer::new(spec.channels()),
            file,
            sources: [SourceSlot::empty(), SourceSlot::empty()],
            microphone: vec![0.0; width],
            system_audio: vec![0.0; width],
            mixed: vec![0.0; width],
            frames: 0,
            clipped_samples: 0,
        })
    }

    /// Registers the stream a source writes with.
    ///
    /// The stream must deliver the recording's rate and one or two channels.
    /// Registering the same stream again after
    /// [`end`](Self::end) re-arms the source.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RateMismatch`] when the stream rate differs from the
    /// recording, [`Error::UnsupportedChannels`] when it delivers more than two
    /// channels, and [`Error::SourceAlreadyRegistered`] when the source is
    /// already registered with another stream.
    pub fn add_source(&mut self, source: Source, spec: &StreamSpec) -> Result<(), Error> {
        let rate = self.spec.sample_rate();
        if spec.sample_rate() != rate {
            return Err(Error::RateMismatch {
                expected: rate,
                actual: spec.sample_rate(),
            });
        }
        let channels = Channels::from_count(spec.channels()).ok_or(Error::UnsupportedChannels {
            source,
            channels: spec.channels(),
        })?;

        let slot = &mut self.sources[index(source)];
        if let Some(registration) = slot.registration.as_mut() {
            if registration.spec == *spec {
                slot.ended = false;
                return Ok(());
            }
            return Err(Error::SourceAlreadyRegistered(source));
        }

        let capacity = usize::try_from(rate).map_err(|_| Error::InvalidSpec {
            sample_rate: rate,
            channels: spec.channels(),
        })?;
        slot.registration = Some(Registration {
            spec: *spec,
            channels,
            queue: FrameQueue::new(usize::from(channels.count()), capacity),
        });
        slot.ended = false;
        Ok(())
    }

    /// Sets the gain mixed in from one source.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidGain`] when `gain` is not a finite number between
    /// 0.0 and [`MAX_GAIN`](koe_record_core::MAX_GAIN) inclusive.
    pub fn set_gain(&mut self, source: Source, gain: f32) -> Result<(), Error> {
        self.mixer.set_gain(source, gain)
    }

    /// Queues interleaved samples from one source and mixes what pairs up.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SourceNotRegistered`] when the source has no stream,
    /// [`Error::SourceEnded`] after [`end`](Self::end),
    /// [`Error::MisalignedBuffer`] for a partial frame, [`Error::InputOverflow`]
    /// when the source's queue cannot hold the block, and the file's error when
    /// a mixed block cannot be written.
    pub fn write(&mut self, source: Source, samples: &[f32]) -> Result<(), Error> {
        {
            let slot = &mut self.sources[index(source)];
            let Some(registration) = slot.registration.as_mut() else {
                return Err(Error::SourceNotRegistered(source));
            };
            if slot.ended {
                return Err(Error::SourceEnded(source));
            }
            let channels = usize::from(registration.channels.count());
            if !samples.len().is_multiple_of(channels) {
                return Err(Error::MisalignedBuffer {
                    len: samples.len(),
                    channels,
                });
            }
            if samples.len() / channels > registration.queue.free_frames() {
                return Err(Error::InputOverflow(source));
            }
            let pushed = registration.queue.push(samples);
            if pushed != samples.len() / channels {
                return Err(Error::ContainerInvariant { operation: "queue" });
            }
        }
        self.drain()
    }

    /// Stops mixing a source; whatever it delivered is dropped and its channel
    /// becomes silence.
    ///
    /// Calling it twice is not an error. Queued frames are discarded because
    /// mixing them after a resume would move them later in the timeline.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SourceNotRegistered`] when the source has no stream.
    pub fn end(&mut self, source: Source) -> Result<(), Error> {
        let slot = &mut self.sources[index(source)];
        let Some(registration) = slot.registration.as_mut() else {
            return Err(Error::SourceNotRegistered(source));
        };
        registration.queue = FrameQueue::new(
            usize::from(registration.channels.count()),
            registration.queue.capacity_frames(),
        );
        slot.ended = true;
        Ok(())
    }

    /// Mixes what is left, completes the file, and reports the recording.
    ///
    /// Frames that never paired with another live source stay in their queue
    /// and are not written.
    ///
    /// # Errors
    ///
    /// Returns the file's error when the last block or the file itself cannot
    /// be written.
    pub fn finish(mut self) -> Result<Summary, Error> {
        self.drain()?;
        let nanoseconds =
            self.frames
                .checked_mul(1_000_000_000)
                .ok_or(Error::ContainerInvariant {
                    operation: "duration",
                })?
                / u64::from(self.spec.sample_rate());
        let summary = Summary {
            frames: self.frames,
            clipped_samples: self.clipped_samples,
            duration: Duration::from_nanos(nanoseconds),
        };
        self.file.finish()?;
        Ok(summary)
    }

    /// Mixes the next block the live sources have in common.
    fn drain(&mut self) -> Result<(), Error> {
        loop {
            let microphone = self.sources[index(Source::Microphone)].available();
            let system_audio = self.sources[index(Source::SystemAudio)].available();
            let frames = match (microphone, system_audio) {
                (Some(microphone), Some(system_audio)) => microphone.min(system_audio),
                (Some(microphone), None) => microphone,
                (None, Some(system_audio)) => system_audio,
                (None, None) => return Ok(()),
            }
            .min(MAX_BLOCK_FRAMES);
            if frames == 0 {
                return Ok(());
            }

            let microphone = self.pop(Source::Microphone, frames)?;
            let system_audio = self.pop(Source::SystemAudio, frames)?;
            let microphone = microphone.map(|channels| Input {
                channels,
                samples: &self.microphone[..frames * usize::from(channels.count())],
            });
            let system_audio = system_audio.map(|channels| Input {
                channels,
                samples: &self.system_audio[..frames * usize::from(channels.count())],
            });

            let length = frames * usize::from(self.spec.channels().count());
            let report = self
                .mixer
                .mix(microphone, system_audio, &mut self.mixed[..length])?;
            self.file.write(&self.mixed[..length])?;
            self.frames = self
                .frames
                .checked_add(
                    u64::try_from(frames).map_err(|_| Error::ContainerInvariant {
                        operation: "mix block",
                    })?,
                )
                .ok_or(Error::ContainerInvariant {
                    operation: "mix block",
                })?;
            self.clipped_samples = self
                .clipped_samples
                .checked_add(u64::try_from(report.clipped).map_err(|_| {
                    Error::ContainerInvariant {
                        operation: "mix block",
                    }
                })?)
                .ok_or(Error::ContainerInvariant {
                    operation: "mix block",
                })?;
        }
    }

    fn pop(&mut self, source: Source, frames: usize) -> Result<Option<Channels>, Error> {
        let slot = &mut self.sources[index(source)];
        if slot.ended {
            return Ok(None);
        }
        let Some(registration) = slot.registration.as_mut() else {
            return Ok(None);
        };
        let channels = usize::from(registration.channels.count());
        let buffer = match source {
            Source::Microphone => &mut self.microphone,
            Source::SystemAudio => &mut self.system_audio,
        };
        let popped = registration
            .queue
            .pop_into(&mut buffer[..frames * channels]);
        if popped != frames {
            return Err(Error::ContainerInvariant { operation: "queue" });
        }
        Ok(Some(registration.channels))
    }
}

impl SourceSlot {
    const fn empty() -> Self {
        Self {
            registration: None,
            ended: false,
        }
    }

    /// Frames the source can still contribute, or `None` when it is not live.
    fn available(&self) -> Option<usize> {
        if self.ended {
            return None;
        }
        self.registration
            .as_ref()
            .map(|registration| registration.queue.available_frames())
    }
}

const fn index(source: Source) -> usize {
    match source {
        Source::Microphone => 0,
        Source::SystemAudio => 1,
    }
}
