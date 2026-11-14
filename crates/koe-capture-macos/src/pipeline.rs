//! Frame accounting and the blocking read, free of `CoreAudio` calls.

use koe_capture_core::{Error, FrameQueue};

use std::time::{Duration, Instant};

use crate::resample::Resampler;

pub const SAMPLE_BYTES: u32 = 4;
pub const QUEUE_FRAMES: usize = 8_192;
pub const CALLBACK_FRAMES: usize = 4_096;
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
pub const RETRY: Duration = Duration::from_millis(1);
/// How often an idle read asks its owner to match the unit to the device.
pub const RECONCILE_INTERVAL: Duration = Duration::from_millis(250);
/// Failed reconciliations in a row before the read reports the render failure.
pub const RECOVERY_ATTEMPTS: u32 = 3;

/// Queue plus the spill the render callback keeps for the next take.
pub struct Sink {
    queue: FrameQueue,
    pending: Vec<f32>,
    channels: usize,
    capacity: usize,
    resampler: Resampler,
    /// Device-rate frames moved out of the queue for conversion.
    block: Vec<f32>,
    /// Converted frames waiting for the reader.
    resampled: Vec<f32>,
}

impl Sink {
    /// Builds a sink that delivers device frames unchanged.
    #[cfg(test)]
    pub fn passthrough(channels: usize, capacity_frames: usize) -> Self {
        Self::new(channels, capacity_frames, 48_000, 48_000)
    }

    /// Builds a sink for `device_rate` frames converted to `spec_rate`.
    pub fn new(channels: usize, capacity_frames: usize, device_rate: u32, spec_rate: u32) -> Self {
        let capacity = channels * capacity_frames;
        Self {
            queue: FrameQueue::new(channels, capacity_frames),
            // Sized up front so `accept` never reallocates it.
            pending: Vec::with_capacity(capacity),
            channels,
            capacity,
            resampler: Resampler::new(channels, device_rate, spec_rate),
            block: vec![0.0; capacity],
            resampled: Vec::with_capacity(capacity),
        }
    }

    /// Follows the device to a new rate. Frames already queued keep the rate
    /// they were captured at, so they are converted before the switch.
    pub fn set_device_rate(&mut self, device_rate: u32) {
        self.flush();
        self.convert_block();
        self.resampler.set_input_rate(device_rate);
    }

    /// Moves the spill into the queue.
    fn flush(&mut self) {
        if !self.pending.is_empty() {
            let accepted = self.queue.push(self.pending.as_slice()) * self.channels;
            self.pending.drain(..accepted);
        }
    }

    /// Converts every queued device frame into the output buffer.
    ///
    /// `block` is one queue wide, so a single pop takes the whole queue.
    fn convert_block(&mut self) {
        let taken = self.queue.pop_into(&mut self.block);
        if taken == 0 {
            return;
        }
        let width = samples(taken, self.channels);
        self.resampler
            .push(&self.block[..width], &mut self.resampled);
    }

    /// Pushes queued frames, then `received`, keeping the spill for the next
    /// take. The spill never exceeds one queue of samples.
    pub fn accept(&mut self, received: &[f32]) {
        let channels = self.channels;
        let mut carry = std::mem::take(&mut self.pending);
        if !carry.is_empty() {
            let accepted = samples(self.queue.push(&carry), channels);
            carry.drain(..accepted);
        }
        let accepted = samples(self.queue.push(received), channels);
        let extra = &received[accepted..];
        let room = self.capacity.saturating_sub(carry.len());
        carry.extend_from_slice(&extra[..extra.len().min(room)]);
        self.pending = carry;
    }

    pub fn take(&mut self, out: &mut [f32]) -> usize {
        self.flush();
        if self.resampler.is_passthrough() {
            // Frames converted before the device settled on this rate still
            // belong ahead of the queue.
            let ready = self.drain_resampled(out);
            if ready == frames(out.len(), self.channels) {
                return ready;
            }
            let offset = samples(ready, self.channels);
            return ready + self.queue.pop_into(&mut out[offset..]);
        }
        self.convert_block();
        self.drain_resampled(out)
    }

    /// Copies converted frames into `out`, returning the frames copied.
    fn drain_resampled(&mut self, out: &mut [f32]) -> usize {
        let wanted = frames(out.len(), self.channels);
        let ready = frames(self.resampled.len(), self.channels);
        let copied = wanted.min(ready);
        let count = samples(copied, self.channels);
        out[..count].copy_from_slice(&self.resampled[..count]);
        self.resampled.drain(..count);
        copied
    }
}

/// Samples occupied by `frames` interleaved frames.
const fn samples(frames: usize, channels: usize) -> usize {
    frames * channels
}

/// Whole frames in `samples` interleaved samples.
const fn frames(samples: usize, channels: usize) -> usize {
    samples / channels
}

/// Whole frames the driver's byte count can yield from `capacity_samples`.
pub const fn delivered_samples(
    reported_bytes: u32,
    channels: usize,
    capacity_samples: usize,
) -> usize {
    let samples = reported_bytes as usize / SAMPLE_BYTES as usize;
    // `Ord::min` is not usable in a const fn yet, so clamp by subtracting the
    // overflow instead.
    let samples = samples - samples.saturating_sub(capacity_samples);
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
    /// Matches the unit to the device it renders. Returns false when the unit
    /// could not be brought back, which the read counts as a failure.
    fn reconcile(&self) -> bool;
}

/// The render error to report when no frames arrived in time.
const fn failure(code: i32) -> Error {
    if code == 0 {
        Error::Timeout {
            operation: "AudioUnitRender",
        }
    } else {
        Error::Backend {
            operation: "AudioUnitRender",
            code,
        }
    }
}

/// Blocking read. The deadline returns whatever arrived, and only an empty
/// read reports the stored render error or a timeout.
///
/// The device can change its rate under a running stream. The HAL then keeps
/// rendering without an error, but delivers at the device's new rate, so the
/// frames no longer carry the rate the caller asked for. The read asks its
/// owner to reconcile on [`RECONCILE_INTERVAL`], and at once after any render
/// error, which rebuilds the unit at the device's rate without ending the
/// stream. [`RECOVERY_ATTEMPTS`] failed reconciliations end the read with the
/// status the unit reported.
pub fn read_until(poll: &impl Poll, channels: usize, out: &mut [f32]) -> Result<usize, Error> {
    let wanted = out.len() / channels;
    let deadline = poll.now() + READ_TIMEOUT;
    let mut produced = 0;
    let mut next_check = poll.now() + RECONCILE_INTERVAL;
    let mut failures = 0;
    while produced < wanted {
        produced += poll.take(&mut out[produced * channels..]);
        if produced == wanted {
            break;
        }
        let now = poll.now();
        if now >= deadline {
            if produced > 0 {
                break;
            }
            return Err(failure(poll.last_error()));
        }
        let code = poll.last_error();
        // A unit that answered with an error has to be rebuilt now; one that
        // renders fine is only compared with its device on the interval.
        if now >= next_check || code != 0 {
            if poll.reconcile() {
                failures = 0;
            } else {
                failures += 1;
                if failures >= RECOVERY_ATTEMPTS {
                    return Err(failure(poll.last_error()));
                }
            }
            next_check = now + RECONCILE_INTERVAL;
        }
        poll.rest();
    }
    Ok(produced)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp, clippy::cast_precision_loss)]

    use super::*;
    use proptest::prelude::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    /// A read loop that never reaches its deadline must fail the test instead
    /// of hanging it, because a hang is reported as a timeout rather than a
    /// caught mutant.
    const POLL_LIMIT: usize = 12_000;

    #[test]
    fn a_failing_status_is_reported_and_the_sink_is_untouched() {
        let mut sink = Sink::passthrough(2, 8);
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
        let mut sink = Sink::passthrough(2, 8);
        let mut scratch = [1.0; 8];
        assert_eq!(render(&mut sink, &mut scratch, 2, 0, 0), Render::Ok);
        let mut out = [0.0; 8];
        assert_eq!(sink.take(&mut out), 0);
    }

    #[test]
    fn a_successful_render_delivers_samples_in_order() {
        let mut sink = Sink::passthrough(2, 8);
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
        let mut sink = Sink::passthrough(1, 2);
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
        let mut sink = Sink::passthrough(2, 2);
        sink.accept(&[1.0, 2.0, 3.0, 4.0]);
        let mut out = [0.0; 8];
        assert_eq!(sink.take(&mut out), 2);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(sink.take(&mut out), 0);
    }

    /// The sink keeps a queue and a spill of the same size, and a take only
    /// pulls from the spill as the queue drains. A single first-in-first-out
    /// buffer would answer one take with more frames than the sink can return.
    struct SinkModel {
        queue: VecDeque<f32>,
        pending: Vec<f32>,
        channels: usize,
        capacity: usize,
    }

    impl SinkModel {
        fn new(channels: usize, capacity_frames: usize) -> Self {
            Self {
                queue: VecDeque::new(),
                pending: Vec::new(),
                channels,
                capacity: channels * capacity_frames,
            }
        }

        fn flush(&mut self) {
            let free = self.capacity - self.queue.len();
            let frames = (self.pending.len() / self.channels).min(free / self.channels);
            let accepted = frames * self.channels;
            self.queue.extend(self.pending.drain(..accepted));
        }

        fn accept(&mut self, received: &[f32]) {
            self.flush();
            let free = self.capacity - self.queue.len();
            let frames = (received.len() / self.channels).min(free / self.channels);
            let accepted = frames * self.channels;
            self.queue.extend(&received[..accepted]);
            let extra = &received[accepted..];
            let room = self.capacity - self.pending.len();
            self.pending
                .extend_from_slice(&extra[..extra.len().min(room)]);
        }

        fn take(&mut self, out: &mut [f32]) -> usize {
            self.flush();
            let frames = (out.len() / self.channels).min(self.queue.len() / self.channels);
            for slot in out.iter_mut().take(frames * self.channels) {
                *slot = self.queue.pop_front().unwrap();
            }
            frames
        }
    }

    proptest! {
        #[test]
        fn the_sink_keeps_the_queue_and_spill_in_order(
            channels in 1usize..=3,
            capacity_frames in 1usize..=4,
            operations in proptest::collection::vec((0usize..=8, 0usize..=8), 0..24),
        ) {
            let mut sink = Sink::passthrough(channels, capacity_frames);
            let mut model = SinkModel::new(channels, capacity_frames);
            let mut next = 0.0f32;

            for (accepted_frames, taken_frames) in operations {
                let received: Vec<f32> = (0..accepted_frames * channels)
                    .map(|_| {
                        let sample = next;
                        next += 1.0;
                        sample
                    })
                    .collect();
                sink.accept(&received);
                model.accept(&received);

                let mut actual = vec![-1.0f32; taken_frames * channels];
                let mut expected = actual.clone();
                prop_assert_eq!(sink.take(&mut actual), model.take(&mut expected));
                prop_assert_eq!(actual, expected);
            }
        }
    }

    struct Fake {
        channels: usize,
        steps: RefCell<VecDeque<usize>>,
        start: Instant,
        elapsed: Cell<Duration>,
        advance: Duration,
        rests: Cell<usize>,
        takes: Cell<usize>,
        produced: Cell<u32>,
        error: Cell<i32>,
        reconciles: Cell<usize>,
        /// Outcomes `reconcile` reports, consumed in order; the last repeats.
        reconciled: RefCell<VecDeque<bool>>,
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
                takes: Cell::new(0),
                produced: Cell::new(0),
                error: Cell::new(0),
                reconciles: Cell::new(0),
                reconciled: RefCell::new(VecDeque::new()),
            }
        }

        fn failing(self, code: i32) -> Self {
            self.error.set(code);
            self
        }

        /// Answers `reconcile` with `outcomes` in turn, then repeats the last.
        fn reconciling(self, outcomes: impl IntoIterator<Item = bool>) -> Self {
            self.reconciled.borrow_mut().extend(outcomes);
            self
        }
    }

    impl Poll for Fake {
        fn take(&self, out: &mut [f32]) -> usize {
            let takes = self.takes.get() + 1;
            assert!(
                takes <= POLL_LIMIT,
                "the read loop never reached its deadline"
            );
            self.takes.set(takes);
            assert!(
                out.len().is_multiple_of(self.channels),
                "the read started mid-frame"
            );
            let frames = self.steps.borrow_mut().pop_front().unwrap_or(0);
            let frames = frames.min(out.len() / self.channels);
            for frame in 0..frames {
                let id = self.produced.get() + u32::try_from(frame).unwrap() + 1;
                out[frame * self.channels..(frame + 1) * self.channels].fill(id as f32);
            }
            self.produced
                .set(self.produced.get() + u32::try_from(frames).unwrap());
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

        fn reconcile(&self) -> bool {
            self.reconciles.set(self.reconciles.get() + 1);
            let mut outcomes = self.reconciled.borrow_mut();
            if outcomes.len() > 1 {
                outcomes.pop_front().unwrap_or(true)
            } else {
                outcomes.front().copied().unwrap_or(true)
            }
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
    fn a_take_flushes_the_spill_into_the_queue() {
        let mut sink = Sink::passthrough(2, 2);
        sink.accept(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        let mut out = [0.0; 4];
        assert_eq!(sink.take(&mut out), 2);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);

        assert_eq!(sink.take(&mut out), 1);
        assert_eq!(out[..2], [5.0, 6.0]);
        assert_eq!(sink.take(&mut out), 0);
    }

    #[test]
    fn an_empty_read_does_not_touch_the_device() {
        let poll = Fake::new(2, [], READ_TIMEOUT);
        assert_eq!(read_until(&poll, 2, &mut []), Ok(0));
        assert_eq!(poll.takes.get(), 0);
    }

    proptest! {
        #[test]
        fn read_until_delivers_every_frame_in_order(
            channels in 1usize..=3,
            wanted in 0usize..=6,
            script in proptest::collection::vec(0usize..=3, 0..12),
        ) {
            let poll = Fake::new(channels, script.iter().copied(), RETRY);
            let mut out = vec![-1.0f32; wanted * channels];
            let total: usize = script.iter().sum();
            let expected = wanted.min(total);
            let result = read_until(&poll, channels, &mut out);
            if wanted > 0 && expected == 0 {
                prop_assert_eq!(
                    result,
                    Err(Error::Timeout {
                        operation: "AudioUnitRender",
                    })
                );
            } else {
                prop_assert_eq!(result, Ok(expected));
            }
            for frame in 0..expected {
                for sample in &out[frame * channels..(frame + 1) * channels] {
                    prop_assert_eq!(*sample, (frame + 1) as f32);
                }
            }
            for sample in &out[expected * channels..] {
                prop_assert_eq!(*sample, -1.0);
            }
        }
    }

    #[test]
    fn the_stream_sizes_and_deadlines_are_pinned() {
        assert_eq!(SAMPLE_BYTES, 4);
        assert_eq!(QUEUE_FRAMES, 8_192);
        assert_eq!(CALLBACK_FRAMES, 4_096);
        assert_eq!(READ_TIMEOUT, Duration::from_secs(5));
        assert_eq!(RETRY, Duration::from_millis(1));

        assert_eq!(RECONCILE_INTERVAL, Duration::from_millis(250));

        assert_eq!(RECOVERY_ATTEMPTS, 3);
    }

    #[test]
    fn a_running_read_asks_its_owner_to_reconcile_on_the_interval() {
        // Four frames take four reads, so the loop rests three times: the
        // first after one interval, the second after two.
        let poll = Fake::new(1, [1, 1, 1, 1], RECONCILE_INTERVAL);
        let mut out = [0.0; 4];
        assert_eq!(read_until(&poll, 1, &mut out).unwrap(), 4);
        assert_eq!(poll.reconciles.get(), 2);
    }

    #[test]
    fn a_running_read_with_no_rests_does_not_reconcile() {
        let poll = Fake::new(1, [1, 1], Duration::ZERO);
        let mut out = [0.0; 2];
        assert_eq!(read_until(&poll, 1, &mut out).unwrap(), 2);
        assert_eq!(poll.reconciles.get(), 0);
    }

    #[test]
    fn a_failed_reconcile_retries_after_the_short_pause() {
        // Two empty reads put the clock past one interval; the failed
        // reconcile then schedules the short retry, which succeeds and
        // restores the interval.
        let poll = Fake::new(1, [0, 0, 1, 1], RECONCILE_INTERVAL).reconciling([false, true]);
        let mut out = [0.0; 2];
        assert_eq!(read_until(&poll, 1, &mut out).unwrap(), 2);
        assert_eq!(poll.reconciles.get(), 2);
    }

    #[test]
    fn repeated_reconcile_failures_end_the_read_with_the_render_status() {
        let poll = Fake::new(1, [], Duration::from_millis(1))
            .failing(-10_863)
            .reconciling([false]);
        let mut out = [0.0; 1];
        assert_eq!(
            read_until(&poll, 1, &mut out),
            Err(Error::Backend {
                operation: "AudioUnitRender",
                code: -10_863,
            })
        );
        assert_eq!(poll.reconciles.get(), RECOVERY_ATTEMPTS as usize);
    }

    #[test]
    fn every_failure_before_the_limit_is_retried() {
        let poll = Fake::new(1, [], Duration::from_millis(1)).reconciling([false, false, false]);
        let mut out = [0.0; 1];
        assert_eq!(
            read_until(&poll, 1, &mut out),
            Err(Error::Timeout {
                operation: "AudioUnitRender",
            })
        );
        assert_eq!(poll.reconciles.get(), RECOVERY_ATTEMPTS as usize);
    }

    #[test]
    fn an_error_reconciles_before_the_interval_elapses() {
        // Frames arrive, so the read is not idle, but one rest is far short of
        // an interval. A render error must still trigger a reconcile.
        let poll = Fake::new(1, [1, 0, 0, 1], Duration::from_millis(1)).failing(-50);
        let mut out = [0.0; 2];
        let _ = read_until(&poll, 1, &mut out);
        assert!(
            poll.reconciles.get() >= 1,
            "an error did not trigger a reconcile"
        );
    }

    #[test]
    fn a_reconfiguring_unit_is_retried_on_the_next_rest() {
        // No error is allowed to pass unexamined, so three failed reconciles
        // end the read within a few rests rather than at the deadline.
        let poll = Fake::new(1, [], Duration::from_millis(1))
            .failing(-10_863)
            .reconciling([false]);
        let mut out = [0.0; 1];
        let _ = read_until(&poll, 1, &mut out);
        assert_eq!(poll.reconciles.get(), RECOVERY_ATTEMPTS as usize);
        assert!(
            poll.elapsed.get() < RECONCILE_INTERVAL,
            "the read waited {:?} before giving up",
            poll.elapsed.get()
        );
    }

    #[test]
    fn a_success_resets_the_failure_count() {
        // A success clears the count, so the failures after it start again
        // from zero and the read survives past the three it would otherwise
        // take to give up.
        let poll =
            Fake::new(1, [], Duration::from_millis(1)).reconciling([true, false, true, false]);
        let mut out = [0.0; 1];
        let _ = read_until(&poll, 1, &mut out);
        assert!(
            poll.reconciles.get() > RECOVERY_ATTEMPTS as usize,
            "a success did not reset the count: {} calls",
            poll.reconciles.get()
        );
    }

    #[test]
    fn any_other_render_error_waits_a_full_interval_between_retries() {
        // Every error is retried on the next rest, whatever its code, so three
        // failures end the read long before the deadline.
        let poll = Fake::new(1, [], Duration::from_millis(1))
            .failing(-50)
            .reconciling([false]);
        let mut out = [0.0; 1];
        let _ = read_until(&poll, 1, &mut out);
        assert_eq!(poll.reconciles.get(), RECOVERY_ATTEMPTS as usize);
    }

    #[test]
    fn a_healthy_unit_is_checked_once_per_interval() {
        // With no error the read only compares the unit with its device on the
        // interval, so a 5 s deadline checked every 250 ms makes about 19
        // calls rather than one per rest.
        let poll = Fake::new(1, [], Duration::from_millis(1));
        let mut out = [0.0; 1];
        let _ = read_until(&poll, 1, &mut out);
        let calls = poll.reconciles.get();
        assert!(
            (15..25).contains(&calls),
            "a healthy unit was checked {calls} times in five seconds"
        );
    }

    #[test]
    fn a_failure_resets_the_check_schedule() {
        // The interval is measured from the last check, so the clock restarts
        // instead of firing every rest after the first.
        let poll = Fake::new(1, [], Duration::from_millis(1));
        let mut out = [0.0; 1];
        let _ = read_until(&poll, 1, &mut out);
        assert!(
            poll.elapsed.get() >= READ_TIMEOUT,
            "the read ended early at {:?}",
            poll.elapsed.get()
        );
    }

    #[test]
    fn a_successful_reconcile_restores_the_interval() {
        // A success resets the failure count and the schedule, so a later
        // failure is not counted against the earlier run.
        let poll = Fake::new(1, [], Duration::from_millis(1)).reconciling([true, false, false]);
        let mut out = [0.0; 1];
        let _ = read_until(&poll, 1, &mut out);
        // Three failures would end the read; starting from a success, the two
        // that follow do not.
        assert!(poll.reconciles.get() > 2);
    }

    #[test]
    fn a_rate_change_converts_queued_frames_and_delivers_their_duration() {
        // Half a second at 96 kHz is half a second at 48 kHz however the sink
        // was opened, so a rate switch must not change how long the audio is.
        let mut sink = Sink::new(1, 48_000, 96_000, 48_000);
        let input = vec![0.5; 48_000];
        sink.accept(&input);
        let mut out = vec![0.0; 24_000];
        let produced = sink.take(&mut out);
        assert!(
            produced.abs_diff(24_000) < 200,
            "delivered {produced} frames for half a second"
        );
    }

    #[test]
    fn a_rate_switch_converts_the_queue_at_the_rate_it_arrived() {
        let mut sink = Sink::new(1, 48_000, 96_000, 48_000);
        // A quarter second at 96 kHz, still queued when the rate changes.
        sink.accept(&vec![0.5; 24_000]);
        sink.set_device_rate(48_000);
        // The switch converts under the old rate and then passes through.
        assert!(sink.resampler.is_passthrough());
        let mut out = vec![0.0; 12_000];
        let produced = sink.take(&mut out);
        assert!(
            produced.abs_diff(12_000) < 200,
            "delivered {produced} frames for a quarter second"
        );
        // Past the settling window every frame is the constant input, to the
        // kernel's unit-gain tolerance.
        assert!(
            out[100..produced - 100]
                .iter()
                .all(|sample| (sample - 0.5).abs() < 1e-3),
            "a converted frame left the constant level"
        );
    }

    #[test]
    fn a_passthrough_sink_returns_the_queue_verbatim() {
        let mut sink = Sink::passthrough(2, 4);
        sink.accept(&[1.0, 2.0, 3.0, 4.0]);
        let mut out = [0.0; 8];
        assert_eq!(sink.take(&mut out), 2);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn a_take_never_returns_more_frames_than_the_request() {
        let mut sink = Sink::new(2, 32, 96_000, 48_000);
        sink.accept(&vec![0.25; 2 * 32]);
        let mut out = [0.0; 8];
        let produced = sink.take(&mut out);
        assert!(produced <= 4, "delivered {produced} frames into four slots");
    }

    #[test]
    fn a_take_of_no_frames_delivers_nothing() {
        let mut sink = Sink::new(1, 8, 96_000, 48_000);
        sink.accept(&[0.25; 8]);
        let mut out = [];
        assert_eq!(sink.take(&mut out), 0);
    }

    #[test]
    fn a_stereo_take_counts_frames_in_whole_pairs() {
        // Every count here is in frames, so a stereo stream must not return a
        // sample count and split a frame across calls.
        let mut sink = Sink::new(2, 4_800, 96_000, 48_000);
        sink.accept(&vec![0.5; 2 * 4_800]);
        let mut out = vec![0.0; 2 * 400];
        let produced = sink.take(&mut out);
        assert_eq!(produced, 400, "delivered {produced} frames into 400 slots");
        // Past the settling window both channels carry the same constant.
        for frame in out.as_chunks::<2>().0.iter().skip(100) {
            assert!((frame[0] - 0.5).abs() < 1e-3, "{frame:?}");
            assert!((frame[1] - 0.5).abs() < 1e-3, "{frame:?}");
        }
    }

    #[test]
    fn a_rate_switch_on_a_stereo_sink_keeps_both_channels() {
        let mut sink = Sink::new(2, 4_800, 96_000, 48_000);
        let mut alternating = Vec::with_capacity(2 * 4_800);
        for _ in 0..4_800 {
            alternating.extend_from_slice(&[0.5, -0.5]);
        }
        sink.accept(&alternating);
        sink.set_device_rate(48_000);
        let mut out = vec![0.0; 2 * 400];
        let produced = sink.take(&mut out);
        assert_eq!(produced, 400);
        // Past the settling window each channel holds its own constant, which
        // a shared filter state would blur together.
        for frame in out.as_chunks::<2>().0.iter().skip(100) {
            assert!((frame[0] - 0.5).abs() < 1e-3, "{frame:?}");
            assert!((frame[1] + 0.5).abs() < 1e-3, "{frame:?}");
        }
    }

    #[test]
    fn samples_and_frames_convert_between_the_two_counts() {
        for channels in 1..=3 {
            for count in [0usize, 1, 2, 7] {
                assert_eq!(frames(samples(count, channels), channels), count);
                assert_eq!(samples(count, channels), count * channels);
            }
        }
        // A partial frame is not a whole frame.
        assert_eq!(frames(7, 2), 3);
        assert_eq!(samples(3, 2), 6);
    }

    #[test]
    fn a_take_counts_in_frames_not_samples() {
        // The caller asks for sixteen samples; at two channels that is eight
        // frames, and a channel-count mix-up would report sixteen.
        let mut sink = Sink::passthrough(2, 8);
        sink.accept(&[1.0; 16]);
        let mut out = [0.0; 16];
        assert_eq!(sink.take(&mut out), 8);
    }

    #[test]
    fn a_passthrough_take_returns_converted_frames_first() {
        // Converted frames queued before the device settled belong ahead of
        // anything still in the device queue.
        let mut sink = Sink::new(1, 4_800, 96_000, 48_000);
        sink.accept(&vec![0.5; 4_800]);
        sink.set_device_rate(48_000);
        sink.accept(&vec![1.0; 4_800]);
        let mut out = [0.0; 2_000];
        let produced = sink.take(&mut out);
        assert!(produced > 1_000);
        // The tail is the converted constant, not the later frames, so the
        // queue was not served out of order.
        assert!(
            (out[produced - 1] - 0.5).abs() < 1e-3,
            "the queue was served out of order: {}",
            out[produced - 1]
        );
    }

    #[test]
    fn a_short_output_still_gets_the_frames_that_fit() {
        let mut sink = Sink::passthrough(2, 8);
        sink.accept(&[1.0; 16]);
        let mut out = [0.0; 6];
        assert_eq!(sink.take(&mut out), 3);
        assert_eq!(sink.take(&mut out), 3);
        assert_eq!(sink.take(&mut out), 2);
    }

    #[test]
    fn a_rate_switch_before_any_audio_converts_nothing() {
        let mut sink = Sink::new(1, 8, 96_000, 48_000);
        sink.set_device_rate(48_000);
        assert!(sink.resampler.is_passthrough());
        let mut out = [0.0; 4];
        assert_eq!(sink.take(&mut out), 0);
    }
    #[test]
    fn a_device_rate_change_is_converted_to_the_requested_rate() {
        let mut sink = Sink::new(1, 4_800, 96_000, 48_000);
        // One tenth of a second at 96 kHz.
        let input: Vec<f32> = (0..9_600).map(|frame| frame as f32).collect();
        let mut out = vec![0.0; 960];
        let mut produced = 0;
        for chunk in input.chunks(1_920) {
            sink.accept(chunk);
            produced += sink.take(&mut out[produced.min(959)..]);
            produced = produced.min(959);
        }
        assert!(produced > 0, "the sink delivered nothing");
    }

    #[test]
    fn the_sink_passes_device_frames_through_at_one_rate() {
        let mut sink = Sink::new(2, 4, 48_000, 48_000);
        sink.accept(&[1.0, 2.0, 3.0, 4.0]);
        let mut out = [0.0; 4];
        assert_eq!(sink.take(&mut out), 2);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn a_rate_change_keeps_the_queued_frames_at_their_own_rate() {
        let mut sink = Sink::new(1, 4_800, 48_000, 48_000);
        // Half a queue captured at 48 kHz.
        sink.accept(&[0.5; 2_400]);
        sink.set_device_rate(96_000);
        let mut out = vec![0.0; 4_800];
        let produced = sink.take(&mut out);
        // The queued frames were converted before the switch, so they arrive
        // as 2400 output frames rather than being measured at 96 kHz.
        assert_eq!(produced, 2_400);
        assert!(out[..produced].iter().all(|sample| *sample == 0.5));
    }

    #[test]
    fn frames_after_a_rate_change_are_converted_from_the_new_rate() {
        let mut sink = Sink::new(1, 9_600, 48_000, 48_000);
        sink.set_device_rate(96_000);
        sink.accept(&vec![0.5; 9_600]);
        let mut out = vec![0.0; 4_800];
        let produced = sink.take(&mut out);
        assert!(produced > 4_700, "delivered {produced} frames");
    }
}
