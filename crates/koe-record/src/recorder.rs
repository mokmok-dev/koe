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
    /// when the source's queue cannot hold the block,
    /// [`Error::RecordingTooLarge`] when the container's size field can no
    /// longer describe the audio, and the file's error when a mixed block
    /// cannot be written.
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
    /// and are not written. The container is completed before a failed last
    /// mix is reported, so the file stays readable.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RecordingTooLarge`] when the container's size field can
    /// no longer describe the audio, and the file's error when the last block
    /// or the file itself cannot be written.
    pub fn finish(mut self) -> Result<Summary, Error> {
        let drained = self.drain();
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
        let closed = self.file.finish();
        drained?;
        closed?;
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
            let Some(frames) = std::num::NonZeroUsize::new(frames) else {
                return Ok(());
            };
            let frames = frames.get();

            let microphone = if microphone.is_some() {
                Some(self.pop(Source::Microphone, frames)?)
            } else {
                None
            };
            let system_audio = if system_audio.is_some() {
                Some(self.pop(Source::SystemAudio, frames)?)
            } else {
                None
            };
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

    fn pop(&mut self, source: Source, frames: usize) -> Result<Channels, Error> {
        let slot = &mut self.sources[index(source)];
        if slot.ended {
            return Err(Error::ContainerInvariant { operation: "queue" });
        }
        let Some(registration) = slot.registration.as_mut() else {
            return Err(Error::ContainerInvariant { operation: "queue" });
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
        Ok(registration.channels)
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::file::AudioFile;

    /// A file that refuses every sample block and reports whether it closed.
    struct FailingFile {
        finished: Arc<AtomicBool>,
    }

    impl AudioFile for FailingFile {
        fn write(&mut self, _samples: &[f32]) -> Result<(), Error> {
            Err(Error::Io {
                operation: "write",
                code: 1,
            })
        }

        fn finish(self: Box<Self>) -> Result<(), Error> {
            self.finished.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn a_failed_last_mix_still_closes_the_file() {
        let finished = Arc::new(AtomicBool::new(false));
        let mut recorder = Recorder {
            spec: RecordingSpec::new(48_000, Channels::Stereo).unwrap(),
            mixer: Mixer::new(Channels::Stereo),
            file: Box::new(FailingFile {
                finished: Arc::clone(&finished),
            }),
            sources: [SourceSlot::empty(), SourceSlot::empty()],
            microphone: vec![0.0; MAX_BLOCK_FRAMES * 2],
            system_audio: vec![0.0; MAX_BLOCK_FRAMES * 2],
            mixed: vec![0.0; MAX_BLOCK_FRAMES * 2],
            frames: 0,
            clipped_samples: 0,
        };
        recorder
            .add_source(
                Source::Microphone,
                &StreamSpec::new(Source::Microphone, 48_000, 1).unwrap(),
            )
            .unwrap();

        // One frame beyond the drain cap stays queued, so the failure repeats.
        let samples = vec![0.25; MAX_BLOCK_FRAMES + 1];
        let write_error = Error::Io {
            operation: "write",
            code: 1,
        };
        assert_eq!(
            recorder.write(Source::Microphone, &samples).err().unwrap(),
            write_error
        );
        assert!(
            !finished.load(Ordering::SeqCst),
            "a failed write leaves the file open"
        );

        assert_eq!(recorder.finish().err().unwrap(), write_error);
        assert!(
            finished.load(Ordering::SeqCst),
            "the file closes even when the last mix fails"
        );
    }

    fn recording(extension: &str, spec: RecordingSpec) -> (tempfile::TempDir, Recorder) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("take.{extension}"));
        let recorder = Recorder::create(&path, spec).unwrap();
        (dir, recorder)
    }

    #[test]
    fn block_buffers_hold_every_output_channel() {
        for (channels, width) in [
            (Channels::Mono, MAX_BLOCK_FRAMES),
            (Channels::Stereo, MAX_BLOCK_FRAMES * 2),
        ] {
            let spec = RecordingSpec::new(48_000, channels).unwrap();
            let (_dir, recorder) = recording("wav", spec);
            assert_eq!(recorder.microphone.len(), width);
            assert_eq!(recorder.system_audio.len(), width);
            assert_eq!(recorder.mixed.len(), width);
        }
    }

    #[test]
    fn set_gain_rejects_values_outside_the_range() {
        let spec = RecordingSpec::new(48_000, Channels::Stereo).unwrap();
        let (_dir, mut recorder) = recording("wav", spec);
        for gain in [f32::NAN, f32::INFINITY, -0.1, crate::MAX_GAIN + 0.1] {
            assert!(
                recorder.set_gain(Source::Microphone, gain).is_err(),
                "{gain} must be rejected"
            );
        }
        for gain in [0.0, 1.0, crate::MAX_GAIN] {
            assert!(
                recorder.set_gain(Source::Microphone, gain).is_ok(),
                "{gain} must be accepted"
            );
        }
    }

    #[test]
    fn a_stereo_system_source_is_mixed_in_full() {
        let spec = RecordingSpec::new(48_000, Channels::Stereo).unwrap();
        let (_dir, mut recorder) = recording("wav", spec);
        recorder
            .add_source(
                Source::SystemAudio,
                &StreamSpec::new(Source::SystemAudio, 48_000, 2).unwrap(),
            )
            .unwrap();
        recorder
            .write(Source::SystemAudio, &[0.5, 0.5, 0.5, 0.5])
            .unwrap();
        let summary = recorder.finish().unwrap();
        assert_eq!(summary.frames, 2);
    }

    #[test]
    fn a_stereo_microphone_source_is_mixed_in_full() {
        let spec = RecordingSpec::new(48_000, Channels::Stereo).unwrap();
        let (_dir, mut recorder) = recording("wav", spec);
        recorder
            .add_source(
                Source::Microphone,
                &StreamSpec::new(Source::Microphone, 48_000, 2).unwrap(),
            )
            .unwrap();
        recorder
            .write(Source::Microphone, &[0.5, 0.5, 0.5, 0.5])
            .unwrap();
        let summary = recorder.finish().unwrap();
        assert_eq!(summary.frames, 2);
    }

    #[test]
    fn a_registered_source_without_samples_finishes_empty() {
        let spec = RecordingSpec::new(48_000, Channels::Stereo).unwrap();
        let (_dir, mut recorder) = recording("wav", spec);
        recorder
            .add_source(
                Source::Microphone,
                &StreamSpec::new(Source::Microphone, 48_000, 1).unwrap(),
            )
            .unwrap();
        let summary = recorder.finish().unwrap();
        assert_eq!(summary.frames, 0);
    }

    #[test]
    fn queued_frames_are_drained() {
        let spec = RecordingSpec::new(48_000, Channels::Stereo).unwrap();
        let (_dir, mut recorder) = recording("wav", spec);
        recorder
            .add_source(
                Source::Microphone,
                &StreamSpec::new(Source::Microphone, 48_000, 1).unwrap(),
            )
            .unwrap();
        recorder.write(Source::Microphone, &[0.25; 10]).unwrap();
        let summary = recorder.finish().unwrap();
        assert_eq!(summary.frames, 10);
    }
}
