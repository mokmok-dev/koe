//! Fixed-capacity ring of whole sample frames.

/// A ring buffer that only ever stores complete interleaved frames.
#[derive(Clone, Debug)]
pub struct FrameQueue {
    samples: Box<[f32]>,
    head: usize,
    len: usize,
    channels: usize,
}

impl FrameQueue {
    /// Creates a queue holding `capacity_frames` frames of `channels` samples.
    ///
    /// # Panics
    ///
    /// Panics when `channels` or `capacity_frames` is zero. Both are internal
    /// invariants, never caller input.
    pub fn new(channels: usize, capacity_frames: usize) -> Self {
        assert!(channels > 0);
        assert!(capacity_frames > 0);
        Self {
            samples: vec![0.0; channels * capacity_frames].into_boxed_slice(),
            head: 0,
            len: 0,
            channels,
        }
    }

    /// Frames the queue can hold in total.
    pub const fn capacity_frames(&self) -> usize {
        self.samples.len() / self.channels
    }

    /// Whole frames currently stored.
    pub const fn available_frames(&self) -> usize {
        self.len / self.channels
    }

    /// Whole frames the queue can still accept.
    pub const fn free_frames(&self) -> usize {
        self.capacity_frames() - self.available_frames()
    }

    /// Appends whole frames, returning how many frames were accepted.
    ///
    /// A trailing partial frame in `samples` is ignored.
    pub fn push(&mut self, samples: &[f32]) -> usize {
        let frames = (samples.len() / self.channels).min(self.free_frames());
        let count = frames * self.channels;
        let capacity = self.samples.len();
        for (offset, sample) in samples.iter().take(count).enumerate() {
            let index = (self.head + self.len + offset) % capacity;
            self.samples[index] = *sample;
        }
        self.len += count;
        frames
    }

    /// Removes whole frames into `out`, returning the frames removed.
    ///
    /// A trailing partial frame slot in `out` stays untouched.
    pub fn pop_into(&mut self, out: &mut [f32]) -> usize {
        let frames = (out.len() / self.channels).min(self.available_frames());
        let count = frames * self.channels;
        let capacity = self.samples.len();
        for (offset, slot) in out.iter_mut().take(count).enumerate() {
            *slot = self.samples[(self.head + offset) % capacity];
        }
        self.head = (self.head + count) % capacity;
        self.len -= count;
        frames
    }

    #[cfg(test)]
    #[must_use]
    pub const fn head(&self) -> usize {
        self.head
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
    use proptest::prelude::*;
    use std::collections::VecDeque;

    #[test]
    fn reports_capacity_in_frames() {
        let queue = FrameQueue::new(2, 3);
        assert_eq!(queue.capacity_frames(), 3);
        assert_eq!(queue.available_frames(), 0);
        assert_eq!(queue.free_frames(), 3);
    }

    #[test]
    fn push_and_pop_round_trip() {
        let mut queue = FrameQueue::new(2, 2);
        assert_eq!(queue.push(&[1.0, 2.0, 3.0, 4.0]), 2);
        assert_eq!(queue.available_frames(), 2);
        assert_eq!(queue.free_frames(), 0);

        let mut out = [0.0; 4];
        assert_eq!(queue.pop_into(&mut out), 2);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(queue.available_frames(), 0);
    }

    #[test]
    fn push_rejects_frames_beyond_capacity() {
        let mut queue = FrameQueue::new(1, 2);
        assert_eq!(queue.push(&[1.0, 2.0, 3.0]), 2);
        assert_eq!(queue.push(&[4.0]), 0);
        let mut out = [0.0; 3];
        assert_eq!(queue.pop_into(&mut out), 2);
        assert_eq!(out, [1.0, 2.0, 0.0]);
    }

    #[test]
    fn partial_frames_are_ignored_on_both_ends() {
        let mut queue = FrameQueue::new(2, 2);
        assert_eq!(queue.push(&[1.0, 2.0, 3.0]), 1);
        let mut out = [0.0; 3];
        assert_eq!(queue.pop_into(&mut out), 1);
        assert_eq!(out, [1.0, 2.0, 0.0]);
    }

    #[test]
    fn wraps_around_without_reordering() {
        let mut queue = FrameQueue::new(2, 2);
        assert_eq!(queue.push(&[1.0, 1.5]), 1);
        let mut out = [0.0; 2];
        assert_eq!(queue.pop_into(&mut out), 1);
        assert_eq!(out, [1.0, 1.5]);

        assert_eq!(queue.push(&[2.0, 2.5, 3.0, 3.5]), 2);
        let mut out = [0.0; 4];
        assert_eq!(queue.pop_into(&mut out), 2);
        assert_eq!(out, [2.0, 2.5, 3.0, 3.5]);
    }

    #[test]
    fn head_never_leaves_the_ring() {
        let mut queue = FrameQueue::new(2, 2);
        assert_eq!(queue.push(&[1.0, 2.0]), 1);
        let mut out = [0.0; 2];
        assert_eq!(queue.pop_into(&mut out), 1);
        assert!(queue.head() < queue.capacity_frames() * 2);

        assert_eq!(queue.push(&[3.0, 4.0, 5.0, 6.0]), 2);
        assert_eq!(queue.pop_into(&mut out), 1);
        assert_eq!(queue.pop_into(&mut out), 1);
        assert!(queue.head() < queue.capacity_frames() * 2);
    }

    proptest! {
        #[test]
        fn matches_a_vecdeque_model(
            channels in 1usize..=4,
            capacity in 1usize..=8,
            operations in proptest::collection::vec((any::<bool>(), 0usize..12, 0usize..12), 0..40),
        ) {
            let mut queue = FrameQueue::new(channels, capacity);
            let mut model: VecDeque<f32> = VecDeque::new();
            let mut pushed_frame = 0.0f32;

            for (is_push, frames, out_frames) in operations {
                if is_push {
                    let sample_count = frames * channels;
                    let input: Vec<f32> = (0..sample_count)
                        .map(|offset| pushed_frame + offset as f32)
                        .collect();
                    let accepted = queue.push(&input);
                    let expected = frames.min(capacity - model.len() / channels);
                    prop_assert_eq!(accepted, expected);
                    for sample in input.iter().take(expected * channels) {
                        model.push_back(*sample);
                    }
                    pushed_frame += sample_count as f32;
                } else {
                    let mut out = vec![-1.0f32; out_frames * channels + channels - 1];
                    let popped = queue.pop_into(&mut out);
                    let expected = out_frames.min(model.len() / channels);
                    prop_assert_eq!(popped, expected);
                    for slot in 0..expected * channels {
                        prop_assert_eq!(out[slot], model.pop_front().unwrap());
                    }
                    for slot in expected * channels..out.len() {
                        prop_assert_eq!(out[slot], -1.0);
                    }
                }
                prop_assert_eq!(queue.available_frames(), model.len() / channels);
                prop_assert_eq!(queue.free_frames(), capacity - model.len() / channels);
                prop_assert!(queue.head() < capacity * channels);
            }
        }
    }
}
