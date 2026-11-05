//! The capture stream facade shared by every platform backend.

use crate::config::StreamSpec;
use crate::device::DeviceInfo;
use crate::error::{Error, Result};
use crate::queue::FrameQueue;

/// Frames each backend read may deliver in one call.
const SCRATCH_FRAMES: usize = 4_096;

/// A platform stream that produces interleaved `f32` samples.
pub trait RawStream {
    /// Blocking read of at most `out.len() / channels` frames.
    ///
    /// Returns the number of frames written, or zero once the stream ends.
    fn read(&mut self, out: &mut [f32]) -> Result<usize>;
}

/// An open capture stream.
///
/// Samples are interleaved `f32` in the range `-1.0..=1.0`.
pub struct CaptureStream {
    raw: Box<dyn RawStream>,
    device: DeviceInfo,
    spec: StreamSpec,
    queue: FrameQueue,
    scratch: Vec<f32>,
}

impl CaptureStream {
    #[must_use]
    pub fn new(raw: Box<dyn RawStream>, device: DeviceInfo, spec: StreamSpec) -> Self {
        let channels = usize::from(spec.channels());
        Self {
            raw,
            device,
            spec,
            queue: FrameQueue::new(channels, SCRATCH_FRAMES),
            scratch: vec![0.0; channels * SCRATCH_FRAMES],
        }
    }

    /// The device this stream was opened on.
    #[must_use]
    pub const fn device(&self) -> &DeviceInfo {
        &self.device
    }

    /// The format this stream delivers.
    #[must_use]
    pub const fn spec(&self) -> &StreamSpec {
        &self.spec
    }

    /// Blocking read of interleaved samples.
    ///
    /// `out` must contain whole frames. The return value counts frames, not
    /// samples.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MisalignedBuffer`] when `out` is not a multiple of the
    /// channel count, [`Error::BufferTooSmall`] when it holds no frame,
    /// [`Error::StreamClosed`] when the backend ends before any frame arrives,
    /// and the backend's own error when a read fails.
    pub fn read(&mut self, out: &mut [f32]) -> Result<usize> {
        let channels = usize::from(self.spec.channels());
        if out.is_empty() {
            return Err(Error::BufferTooSmall { frames: 0 });
        }
        if !out.len().is_multiple_of(channels) {
            return Err(Error::MisalignedBuffer {
                len: out.len(),
                channels,
            });
        }

        let wanted = out.len() / channels;
        let mut produced = 0;
        loop {
            produced += self.queue.pop_into(&mut out[produced * channels..]);
            if produced == wanted {
                return Ok(produced);
            }

            let frames = self.raw.read(&mut self.scratch)?;
            let capacity = self.scratch.len() / channels;
            if frames > capacity {
                return Err(Error::BackendInvariant { operation: "read" });
            }
            if frames == 0 {
                return if produced == 0 {
                    Err(Error::StreamClosed)
                } else {
                    Ok(produced)
                };
            }

            // A zero-frame read that slipped past the branch above would make
            // this loop spin forever, so the contract is checked twice.
            if frames == 0 {
                return Err(Error::BackendInvariant { operation: "read" });
            }
            let pushed = self.queue.push(&self.scratch[..frames * channels]);
            if pushed != frames {
                return Err(Error::BackendInvariant { operation: "push" });
            }
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn scratch_frames(&self) -> usize {
        self.scratch.len() / usize::from(self.spec.channels())
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::float_cmp,
        clippy::cast_precision_loss,
        clippy::needless_range_loop
    )]

    use super::*;
    use crate::{DeviceId, Source};
    use std::collections::VecDeque;

    enum Step {
        Frames(Vec<f32>),
        Error(Error),
        Closed,
        Overshoot(usize),
    }

    struct ScriptedStream {
        steps: VecDeque<Step>,
        channels: usize,
    }

    impl ScriptedStream {
        fn new(channels: usize, steps: impl IntoIterator<Item = Step>) -> Self {
            Self {
                steps: steps.into_iter().collect(),
                channels,
            }
        }
    }

    impl RawStream for ScriptedStream {
        fn read(&mut self, out: &mut [f32]) -> Result<usize> {
            match self.steps.pop_front() {
                Some(Step::Frames(samples)) => {
                    out[..samples.len()].copy_from_slice(&samples);
                    Ok(samples.len() / self.channels)
                }
                Some(Step::Error(error)) => Err(error),
                Some(Step::Closed) | None => Ok(0),
                Some(Step::Overshoot(frames)) => Ok(frames),
            }
        }
    }

    fn device(channels: u16) -> DeviceInfo {
        DeviceInfo::new(
            DeviceId::new("test").unwrap(),
            "test device",
            Source::Microphone,
            true,
            48_000,
            channels,
        )
    }

    fn stream(channels: u16, steps: impl IntoIterator<Item = Step>) -> CaptureStream {
        let raw = ScriptedStream::new(usize::from(channels), steps);
        let spec = StreamSpec::new(Source::Microphone, 48_000, channels).unwrap();
        CaptureStream::new(Box::new(raw), device(channels), spec)
    }

    #[test]
    fn exposes_device_and_spec() {
        let spec = StreamSpec::new(Source::SystemAudio, 44_100, 2).unwrap();
        let stream = CaptureStream::new(Box::new(ScriptedStream::new(2, [])), device(2), spec);
        assert_eq!(stream.device().id().as_str(), "test");
        assert_eq!(stream.spec(), &spec);
        assert_eq!(stream.scratch_frames(), SCRATCH_FRAMES);
    }

    #[test]
    fn reads_exact_frames() {
        let mut stream = stream(2, [Step::Frames(vec![1.0, 2.0, 3.0, 4.0])]);
        let mut out = [0.0; 4];
        assert_eq!(stream.read(&mut out).unwrap(), 2);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn spans_several_backend_reads() {
        let mut stream = stream(
            2,
            [Step::Frames(vec![1.0, 2.0]), Step::Frames(vec![3.0, 4.0])],
        );
        let mut out = [0.0; 4];
        assert_eq!(stream.read(&mut out).unwrap(), 2);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn stops_reading_once_the_request_is_full() {
        let mut stream = stream(
            2,
            [
                Step::Frames(vec![1.0, 2.0, 3.0, 4.0]),
                Step::Error(Error::Backend {
                    operation: "extra",
                    code: -9,
                }),
            ],
        );
        let mut out = [0.0; 4];
        assert_eq!(stream.read(&mut out).unwrap(), 2);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn accepts_a_read_that_fills_the_scratch_exactly() {
        let samples = vec![0.5; SCRATCH_FRAMES];
        let mut stream = stream(1, [Step::Frames(samples)]);
        let mut out = vec![0.0; SCRATCH_FRAMES];
        assert_eq!(stream.read(&mut out).unwrap(), SCRATCH_FRAMES);
        assert!(out.iter().all(|sample| *sample == 0.5));
    }

    #[test]
    fn buffers_backend_frames_larger_than_the_request() {
        let mut stream = stream(1, [Step::Frames(vec![1.0, 2.0, 3.0])]);
        let mut first = [0.0; 1];
        assert_eq!(stream.read(&mut first).unwrap(), 1);
        assert_eq!(first, [1.0]);
        let mut second = [0.0; 1];
        assert_eq!(stream.read(&mut second).unwrap(), 1);
        assert_eq!(second, [2.0]);
        let mut third = [0.0; 1];
        assert_eq!(stream.read(&mut third).unwrap(), 1);
        assert_eq!(third, [3.0]);
    }

    #[test]
    fn returns_partial_frames_when_the_stream_ends() {
        let mut stream = stream(1, [Step::Frames(vec![7.0]), Step::Closed]);
        let mut out = [0.0; 2];
        assert_eq!(stream.read(&mut out).unwrap(), 1);
        assert_eq!(out, [7.0, 0.0]);
    }

    #[test]
    fn closed_stream_is_an_error_when_nothing_arrived() {
        let mut stream = stream(1, [Step::Closed]);
        let mut out = [0.0; 1];
        assert_eq!(stream.read(&mut out), Err(Error::StreamClosed));
    }

    #[test]
    fn backend_errors_propagate() {
        let mut stream = stream(
            1,
            [Step::Error(Error::Backend {
                operation: "read",
                code: -1,
            })],
        );
        let mut out = [0.0; 1];
        assert_eq!(
            stream.read(&mut out),
            Err(Error::Backend {
                operation: "read",
                code: -1,
            })
        );
    }

    #[test]
    fn empty_output_is_rejected() {
        let mut stream = stream(2, []);
        assert_eq!(
            stream.read(&mut []),
            Err(Error::BufferTooSmall { frames: 0 })
        );
    }

    #[test]
    fn misaligned_output_is_rejected() {
        let mut stream = stream(2, []);
        assert_eq!(
            stream.read(&mut [0.0; 3]),
            Err(Error::MisalignedBuffer {
                len: 3,
                channels: 2,
            })
        );
    }

    #[test]
    fn backend_cannot_return_more_frames_than_requested() {
        let mut stream = stream(2, [Step::Overshoot(SCRATCH_FRAMES + 1)]);
        let mut out = [0.0; 2];
        assert_eq!(
            stream.read(&mut out),
            Err(Error::BackendInvariant { operation: "read" })
        );
    }

    #[test]
    fn scratch_holds_the_documented_frame_count() {
        assert_eq!(SCRATCH_FRAMES, 4_096);
        let spec = StreamSpec::new(Source::Microphone, 48_000, 2).unwrap();
        let stream = CaptureStream::new(Box::new(ScriptedStream::new(2, [])), device(2), spec);
        assert_eq!(stream.scratch_frames(), 4_096);
    }
}
