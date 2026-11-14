//! Conversion from the device's rate to the rate the caller requested.
//!
//! The HAL output unit renders only while its client format's rate equals the
//! device's own. Any other rate makes every `AudioUnitRender` answer
//! `kAudioUnitErr_CannotDoInCurrentContext` (-10863) and deliver nothing, so a
//! 44.1 kHz microphone or a 96 kHz interface could not be captured at all. The
//! unit therefore runs at the device's rate and this converts its frames to the
//! requested one.
//!
//! The kernel and phase math follow the converter in `koe-transcribe-core`,
//! widened from a mono 16 kHz target to any rate and channel count.

use std::f32::consts::PI;

/// Taps in the resampling kernel.
pub const TAPS: usize = 48;

/// Fractional positions the kernel holds per input frame.
pub const PHASES: usize = 256;

/// Distance from the oldest tap to the kernel centre, in input frames.
///
/// The centre sits between the two middle taps, which makes the filter linear
/// phase; its output therefore lags its window by this many frames.
const CENTER: f32 = 23.5;

/// Converts interleaved `f32` frames between two rates, keeping every channel.
///
/// The output for a given input does not depend on how the caller splits that
/// input across [`push`](Self::push) calls.
pub struct Resampler {
    channels: usize,
    input_rate: u32,
    output_rate: u32,
    passthrough: bool,
    kernel: Box<[[f32; TAPS]]>,
    /// Interleaved input frames, oldest first. `index i` is input frame
    /// `start + i - (TAPS - 1)`, so the leading zeros stand in for the frames
    /// before the stream and the first output keeps a full window.
    history: Vec<f32>,
    /// Input frame the history's first real frame follows.
    start: u64,
    received: u64,
    emitted: u64,
}

impl Resampler {
    /// Builds a converter for `channels` interleaved frames arriving at
    /// `input_rate` and leaving at `output_rate`.
    pub fn new(channels: usize, input_rate: u32, output_rate: u32) -> Self {
        let mut resampler = Self {
            channels,
            input_rate,
            output_rate,
            passthrough: input_rate == output_rate,
            kernel: build_kernel(input_rate, output_rate),
            history: vec![0.0; (TAPS - 1) * channels],
            start: 0,
            received: 0,
            emitted: 0,
        };
        resampler.reset_history();
        resampler
    }

    /// Whether frames pass through unchanged.
    #[must_use]
    pub const fn is_passthrough(&self) -> bool {
        self.passthrough
    }

    /// Starts a new segment at `input_rate`, dropping the window the previous
    /// rate left.
    ///
    /// The device can change its rate under a running stream. Frames already
    /// queued keep the old rate's timeline; the new segment counts from zero.
    pub fn set_input_rate(&mut self, input_rate: u32) {
        if input_rate == self.input_rate {
            return;
        }
        self.input_rate = input_rate;
        self.passthrough = input_rate == self.output_rate;
        self.kernel = build_kernel(input_rate, self.output_rate);
        self.received = 0;
        self.emitted = 0;
        self.start = 0;
        self.reset_history();
    }

    /// Appends the converted form of `interleaved` to `out`.
    ///
    /// A trailing partial frame is ignored.
    pub fn push(&mut self, interleaved: &[f32], out: &mut Vec<f32>) {
        let channels = self.channels;
        let frames = interleaved.len() / channels;
        if frames == 0 {
            return;
        }
        if self.passthrough {
            out.extend_from_slice(&interleaved[..frames * channels]);
            return;
        }

        self.release_consumed();
        self.history
            .extend_from_slice(&interleaved[..frames * channels]);
        self.received += u64::try_from(frames).unwrap_or(u64::MAX);

        let target = self.received * u64::from(self.output_rate) / u64::from(self.input_rate);
        out.reserve(
            usize::try_from(target.saturating_sub(self.emitted))
                .unwrap_or(0)
                .saturating_mul(channels),
        );
        while self.emitted < target {
            if !self.interpolate(&mut *out) {
                break;
            }
            self.emitted += 1;
        }
    }

    /// One output frame, when its window has arrived.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn interpolate(&self, out: &mut Vec<f32>) -> bool {
        let scaled = self.emitted * u64::from(self.input_rate);
        let anchor = scaled / u64::from(self.output_rate);
        let remainder = scaled % u64::from(self.output_rate);
        let phase = (remainder * PHASES as u64 / u64::from(self.output_rate)) as usize;
        let Some(offset) = anchor.checked_sub(self.start).map(|offset| offset as usize) else {
            return false;
        };
        let taps = &self.kernel[phase];
        let channels = self.channels;
        let Some(window) = self
            .history
            .get(offset * channels..(offset + TAPS) * channels)
        else {
            return false;
        };
        for channel in 0..channels {
            let mut sample = 0.0;
            for (tap, weight) in taps.iter().enumerate() {
                sample += window[tap * channels + channel] * weight;
            }
            out.push(sample);
        }
        true
    }

    /// Restores the all-zero window that stands in for the frames before a
    /// stream.
    fn reset_history(&mut self) {
        self.history.clear();
        self.history.resize((TAPS - 1) * self.channels, 0.0);
    }

    /// Drops input no future window can reach.
    fn release_consumed(&mut self) {
        let next = self.emitted * u64::from(self.input_rate) / u64::from(self.output_rate);
        if next <= self.start {
            return;
        }
        let drop = usize::try_from(next - self.start)
            .unwrap_or(usize::MAX)
            .min(self.history.len() / self.channels)
            * self.channels;
        self.history.drain(..drop);
        self.start = next;
    }
}

/// Builds the windowed-sinc kernel, one row per fractional phase.
///
/// Each row is scaled to unit DC gain so a phase change cannot modulate level.
#[allow(clippy::cast_precision_loss)]
fn build_kernel(input_rate: u32, output_rate: u32) -> Box<[[f32; TAPS]]> {
    let cutoff = 0.45 * input_rate.min(output_rate) as f32;
    let cycles = cutoff / input_rate as f32;
    let mut kernel = vec![[0.0_f32; TAPS]; PHASES];
    for (phase, row) in kernel.iter_mut().enumerate() {
        let fraction = phase as f32 / PHASES as f32;
        for (tap, weight) in row.iter_mut().enumerate() {
            let distance = tap as f32 - CENTER - fraction;
            let window = 0.46f32.mul_add(-(2.0 * PI * tap as f32 / (TAPS - 1) as f32).cos(), 0.54);
            let gain = if distance.abs() < f32::EPSILON {
                2.0 * cycles
            } else {
                (2.0 * PI * cycles * distance).sin() / (PI * distance)
            };
            *weight = gain * window;
        }
        let sum: f32 = row.iter().sum();
        if sum.abs() > f32::EPSILON {
            for weight in row.iter_mut() {
                *weight /= sum;
            }
        }
    }
    kernel.into_boxed_slice()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::float_cmp,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]

    use super::*;

    fn tone(rate: u32, frequency: f64, frames: usize, channels: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames * channels);
        for frame in 0..frames {
            let sample = (0.5
                * (2.0 * std::f64::consts::PI * frequency * frame as f64 / f64::from(rate)).sin())
                as f32;
            for _ in 0..channels {
                out.push(sample);
            }
        }
        out
    }

    /// Peak amplitude of a sine with the samples' RMS, which does not care
    /// whether the window holds whole cycles.
    fn amplitude(samples: &[f32]) -> f64 {
        let sum: f64 = samples
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum();
        (2.0 * sum / samples.len() as f64).sqrt()
    }

    #[test]
    fn equal_rates_pass_through_verbatim() {
        let mut resampler = Resampler::new(2, 48_000, 48_000);
        assert!(resampler.is_passthrough());
        let input = [0.25, -0.25, 0.5, -0.5];
        let mut out = Vec::new();
        resampler.push(&input, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn a_partial_frame_is_ignored() {
        let mut resampler = Resampler::new(2, 48_000, 48_000);
        let mut out = Vec::new();
        resampler.push(&[0.5, -0.5, 1.0, -1.0, 2.0], &mut out);
        assert_eq!(out, [0.5, -0.5, 1.0, -1.0]);
    }

    #[test]
    fn an_empty_push_appends_nothing() {
        let mut resampler = Resampler::new(2, 44_100, 48_000);
        let mut out = Vec::new();
        resampler.push(&[], &mut out);
        assert!(out.is_empty(), "an empty push appended {out:?}");
    }

    #[test]
    fn every_channel_is_converted_separately() {
        let mut resampler = Resampler::new(2, 24_000, 48_000);
        let frames = 4_800;
        let input = tone(24_000, 440.0, frames, 2);
        let mut out = Vec::new();
        resampler.push(&input, &mut out);
        assert_eq!(out.len() % 2, 0);
        for frame in out.as_chunks::<2>().0 {
            assert_eq!(frame[0], frame[1]);
        }
    }

    #[test]
    fn upsampling_delivers_the_ratio() {
        let mut resampler = Resampler::new(1, 44_100, 48_000);
        let mut out = Vec::new();
        resampler.push(&tone(44_100, 440.0, 44_100, 1), &mut out);
        // 44100 * 48000 / 44100 = 48000, less the frames still needed for the
        // last output's window.
        assert!(out.len() >= 47_940, "emitted {}", out.len());
        assert!(out.len() <= 48_000);
    }

    #[test]
    fn downsampling_delivers_the_ratio() {
        let mut resampler = Resampler::new(1, 96_000, 48_000);
        let mut out = Vec::new();
        resampler.push(&tone(96_000, 440.0, 96_000, 1), &mut out);
        assert!(out.len() >= 47_940, "emitted {}", out.len());
        assert!(out.len() <= 48_000);
    }

    #[test]
    fn a_passband_tone_keeps_its_amplitude_upsampling() {
        let mut resampler = Resampler::new(1, 44_100, 48_000);
        let mut out = Vec::new();
        resampler.push(&tone(44_100, 1_000.0, 44_100, 1), &mut out);
        // Skip the settling window at each end. 1000 cycles arrive in one
        // second, which is 48000 output samples.
        let steady = &out[1_000..out.len() - 1_000];
        let measured = amplitude(steady);
        assert!((measured - 0.5).abs() < 0.02, "amplitude {measured}");
    }

    #[test]
    fn a_passband_tone_keeps_its_amplitude_downsampling() {
        let mut resampler = Resampler::new(1, 96_000, 48_000);
        let mut out = Vec::new();
        resampler.push(&tone(96_000, 1_000.0, 96_000, 1), &mut out);
        let steady = &out[1_000..out.len() - 1_000];
        let measured = amplitude(steady);
        assert!((measured - 0.5).abs() < 0.02, "amplitude {measured}");
    }

    #[test]
    fn the_output_does_not_depend_on_the_push_split() {
        let input = tone(44_100, 440.0, 4_410, 2);
        let mut whole = Resampler::new(2, 44_100, 48_000);
        let mut expected = Vec::new();
        whole.push(&input, &mut expected);

        let mut split = Resampler::new(2, 44_100, 48_000);
        let mut actual = Vec::new();
        for chunk in input.chunks(2 * 37) {
            split.push(chunk, &mut actual);
        }
        assert_eq!(actual.len(), expected.len());
        for (left, right) in actual.iter().zip(&expected) {
            assert!((left - right).abs() < 1e-6, "{left} vs {right}");
        }
    }

    #[test]
    fn a_rate_change_starts_a_new_segment() {
        let mut resampler = Resampler::new(1, 48_000, 48_000);
        let mut out = Vec::new();
        resampler.push(&[0.5; 96], &mut out);
        assert_eq!(out.len(), 96);
        resampler.set_input_rate(96_000);
        assert!(!resampler.is_passthrough());
        out.clear();
        resampler.push(&tone(96_000, 1_000.0, 9_600, 1), &mut out);
        assert!(out.len() >= 4_700, "emitted {}", out.len());
        assert!(out.len() <= 4_800);
    }

    #[test]
    fn setting_the_same_rate_keeps_the_stream() {
        let mut resampler = Resampler::new(1, 44_100, 48_000);
        let mut out = Vec::new();
        resampler.push(&tone(44_100, 440.0, 4_410, 1), &mut out);
        let before = out.len();
        resampler.set_input_rate(44_100);
        resampler.push(&tone(44_100, 440.0, 4_410, 1), &mut out);
        // The second push continues the same timeline rather than restarting.
        assert!(out.len() > before + 4_000, "emitted {}", out.len());
    }

    #[test]
    fn the_kernel_has_unit_gain_at_every_phase() {
        for (input, output) in [(44_100, 48_000), (96_000, 48_000), (48_000, 44_100)] {
            let kernel = build_kernel(input, output);
            let rows: Vec<Vec<f32>> = kernel.iter().map(|row| row.to_vec()).collect();
            for row in rows {
                let sum: f32 = row.iter().sum();
                assert!((sum - 1.0).abs() < 1e-4, "sum {sum} for {input}->{output}");
            }
        }
    }
}
