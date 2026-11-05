//! Frame accounting and the blocking read, free of `CoreAudio` calls.

use koe_capture_core::{Error, FrameQueue};

use std::time::{Duration, Instant};

pub const SAMPLE_BYTES: u32 = 4;
pub const QUEUE_FRAMES: usize = 8_192;
pub const CALLBACK_FRAMES: usize = 4_096;
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
pub const RETRY: Duration = Duration::from_millis(1);

/// Queue plus the spill the render callback keeps for the next take.
pub struct Sink {
    queue: FrameQueue,
    pending: Vec<f32>,
    channels: usize,
    capacity: usize,
}

impl Sink {
    pub fn new(channels: usize, capacity_frames: usize) -> Self {
        let capacity = channels * capacity_frames;
        Self {
            queue: FrameQueue::new(channels, capacity_frames),
            // Sized up front so `accept` never reallocates it.
            pending: Vec::with_capacity(capacity),
            channels,
            capacity,
        }
    }

    /// Pushes queued frames, then `received`, keeping the spill for the next
    /// take. The spill never exceeds one queue of samples.
    pub fn accept(&mut self, received: &[f32]) {
        let channels = self.channels;
        let mut carry = std::mem::take(&mut self.pending);
        if !carry.is_empty() {
            let accepted = self.queue.push(&carry) * channels;
            carry.drain(..accepted);
        }
        let accepted = self.queue.push(received) * channels;
        let extra = &received[accepted..];
        let room = self.capacity.saturating_sub(carry.len());
        carry.extend_from_slice(&extra[..extra.len().min(room)]);
        self.pending = carry;
    }

    pub fn take(&mut self, out: &mut [f32]) -> usize {
        if !self.pending.is_empty() {
            let accepted = self.queue.push(self.pending.as_slice()) * self.channels;
            self.pending.drain(..accepted);
        }
        self.queue.pop_into(out)
    }
}

/// Whole frames the driver's byte count can yield from `capacity_samples`.
pub const fn delivered_samples(
    reported_bytes: u32,
    channels: usize,
    capacity_samples: usize,
) -> usize {
    let samples = reported_bytes as usize / SAMPLE_BYTES as usize;
    let samples = if samples > capacity_samples {
        capacity_samples
    } else {
        samples
    };
    samples - samples % channels
}

/// Outcome of one render, handed to the reader through the stream's error
/// slot. The callback answers zero either way so the HAL keeps the IO proc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Render {
    Ok,
    Failed(i32),
}

/// Consumes one render, leaving the sink untouched when the unit failed.
pub fn render(
    sink: &mut Sink,
    scratch: &mut [f32],
    channels: usize,
    status: i32,
    reported_bytes: u32,
) -> Render {
    if status != 0 {
        return Render::Failed(status);
    }
    let delivered = delivered_samples(reported_bytes, channels, scratch.len());
    if delivered != 0 {
        sink.accept(&scratch[..delivered]);
    }
    Render::Ok
}

/// The parts of the stream the read loop needs from its owner.
pub trait Poll {
    fn take(&self, out: &mut [f32]) -> usize;
    /// Last non-zero `AudioUnitRender` status, or zero.
    fn last_error(&self) -> i32;
    fn now(&self) -> Instant;
    fn rest(&self);
}

/// Blocking read. The deadline returns whatever arrived, and only an empty
/// read reports the stored render error or a timeout.
pub fn read_until(poll: &impl Poll, channels: usize, out: &mut [f32]) -> Result<usize, Error> {
    let wanted = out.len() / channels;
    let deadline = poll.now() + READ_TIMEOUT;
    let mut produced = 0;
    while produced < wanted {
        produced += poll.take(&mut out[produced * channels..]);
        if produced == wanted {
            break;
        }
        if poll.now() >= deadline {
            if produced > 0 {
                break;
            }
            let code = poll.last_error();
            return Err(if code == 0 {
                Error::Timeout {
                    operation: "AudioUnitRender",
                }
            } else {
                Error::Backend {
                    operation: "AudioUnitRender",
                    code,
                }
            });
        }
        poll.rest();
    }
    Ok(produced)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    #[test]
    fn a_failing_status_is_reported_and_the_sink_is_untouched() {
        let mut sink = Sink::new(2, 8);
        let mut scratch = [1.0; 8];
        assert_eq!(
            render(&mut sink, &mut scratch, 2, -50, 32),
            Render::Failed(-50)
        );
        let mut out = [0.0; 8];
        assert_eq!(sink.take(&mut out), 0);
    }

    #[test]
    fn a_render_without_bytes_leaves_the_sink_untouched() {
        let mut sink = Sink::new(2, 8);
        let mut scratch = [1.0; 8];
        assert_eq!(render(&mut sink, &mut scratch, 2, 0, 0), Render::Ok);
        let mut out = [0.0; 8];
        assert_eq!(sink.take(&mut out), 0);
    }

    #[test]
    fn a_successful_render_delivers_samples_in_order() {
        let mut sink = Sink::new(2, 8);
        let mut scratch = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        assert_eq!(render(&mut sink, &mut scratch, 2, 0, 32), Render::Ok);
        let mut out = [0.0; 8];
        assert_eq!(sink.take(&mut out), 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
    }

    #[test]
    fn delivered_samples_accepts_exactly_the_capacity() {
        assert_eq!(delivered_samples(32, 2, 8), 8);
    }

    #[test]
    fn delivered_samples_clamps_bytes_over_the_capacity() {
        assert_eq!(delivered_samples(64, 2, 8), 8);
    }

    #[test]
    fn delivered_samples_clamps_the_largest_byte_count() {
        assert_eq!(delivered_samples(u32::MAX, 2, 8), 8);
    }

    #[test]
    fn delivered_samples_floors_a_partial_frame() {
        assert_eq!(delivered_samples(12, 2, 8), 2);
        assert_eq!(delivered_samples(6, 2, 8), 0);
    }

    #[test]
    fn delivered_samples_of_no_bytes_is_zero() {
        assert_eq!(delivered_samples(0, 2, 8), 0);
    }

    #[test]
    fn a_spill_larger_than_the_queue_stays_in_order_and_bounded() {
        let mut sink = Sink::new(1, 2);
        sink.accept(&[1.0, 2.0, 3.0, 4.0, 5.0]);

        let mut out = [0.0; 4];
        assert_eq!(sink.take(&mut out), 2);
        assert_eq!(out, [1.0, 2.0, 0.0, 0.0]);

        assert_eq!(sink.take(&mut out), 2);
        assert_eq!(out, [3.0, 4.0, 0.0, 0.0]);

        assert_eq!(sink.take(&mut out), 0);
    }

    #[test]
    fn a_take_is_limited_by_the_queue() {
        let mut sink = Sink::new(2, 2);
        sink.accept(&[1.0, 2.0, 3.0, 4.0]);
        let mut out = [0.0; 8];
        assert_eq!(sink.take(&mut out), 2);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 0.0, 0.0, 0.0, 0.0]);
    }

    struct Fake {
        channels: usize,
        steps: RefCell<VecDeque<usize>>,
        start: Instant,
        elapsed: Cell<Duration>,
        advance: Duration,
        rests: Cell<usize>,
        error: Cell<i32>,
    }

    impl Fake {
        fn new(channels: usize, steps: impl IntoIterator<Item = usize>, advance: Duration) -> Self {
            Self {
                channels,
                steps: RefCell::new(steps.into_iter().collect()),
                start: Instant::now(),
                elapsed: Cell::new(Duration::ZERO),
                advance,
                rests: Cell::new(0),
                error: Cell::new(0),
            }
        }

        fn failing(self, code: i32) -> Self {
            self.error.set(code);
            self
        }
    }

    impl Poll for Fake {
        fn take(&self, out: &mut [f32]) -> usize {
            let frames = self.steps.borrow_mut().pop_front().unwrap_or(0);
            let frames = frames.min(out.len() / self.channels);
            out[..frames * self.channels].fill(1.0);
            frames
        }

        fn last_error(&self) -> i32 {
            self.error.get()
        }

        fn now(&self) -> Instant {
            self.start + self.elapsed.get()
        }

        fn rest(&self) {
            self.rests.set(self.rests.get() + 1);
            self.elapsed.set(self.elapsed.get() + self.advance);
        }
    }

    #[test]
    fn a_read_fills_across_takes() {
        let poll = Fake::new(2, [1, 1], RETRY);
        let mut out = [0.0; 4];
        assert_eq!(read_until(&poll, 2, &mut out).unwrap(), 2);
        assert_eq!(poll.rests.get(), 1);
    }

    #[test]
    fn a_zero_take_waits_for_the_next_frame() {
        let poll = Fake::new(1, [0, 0, 1], RETRY);
        let mut out = [0.0; 1];
        assert_eq!(read_until(&poll, 1, &mut out).unwrap(), 1);
        assert_eq!(poll.rests.get(), 2);
    }

    #[test]
    fn the_deadline_returns_partial_frames() {
        let poll = Fake::new(2, [1, 0], READ_TIMEOUT);
        let mut out = [0.0; 8];
        assert_eq!(read_until(&poll, 2, &mut out).unwrap(), 1);
        assert_eq!(out[..2], [1.0, 1.0]);
    }

    #[test]
    fn the_deadline_reports_a_stored_render_error() {
        let poll = Fake::new(1, [], READ_TIMEOUT).failing(-50);
        let mut out = [0.0; 1];
        assert_eq!(
            read_until(&poll, 1, &mut out),
            Err(Error::Backend {
                operation: "AudioUnitRender",
                code: -50,
            })
        );
    }

    #[test]
    fn a_read_without_frames_times_out() {
        let poll = Fake::new(1, [], READ_TIMEOUT);
        let mut out = [0.0; 1];
        assert_eq!(
            read_until(&poll, 1, &mut out),
            Err(Error::Timeout {
                operation: "AudioUnitRender",
            })
        );
    }

    #[test]
    fn the_stream_sizes_and_deadlines_are_pinned() {
        assert_eq!(SAMPLE_BYTES, 4);
        assert_eq!(QUEUE_FRAMES, 8_192);
        assert_eq!(CALLBACK_FRAMES, 4_096);
        assert_eq!(READ_TIMEOUT, Duration::from_secs(5));
        assert_eq!(RETRY, Duration::from_millis(1));
    }
}
