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

/// Taps at the lowest decimation ratio.
const BASE_TAPS: usize = 48;

/// Fractional positions the kernel holds per input frame.
pub const PHASES: usize = 256;

/// A windowed-sinc table, one row per fractional phase.
///
/// The tap count grows with the decimation ratio: a filter's transition band
/// is fixed in input samples, so halving the rate doubles how wide it is in
/// input-rate terms. At the base 48 taps, decimating 96 kHz to 48 kHz leaves
/// the band reaching past the output Nyquist and folds 24 kHz back at about
/// -27 dB, so the table widens to match.
struct Kernel {
    taps: usize,
    /// `taps` weights per phase, row after row.
    rows: Box<[f32]>,
}

impl Kernel {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn new(input_rate: u32, output_rate: u32) -> Self {
        // The ratio the filter has to resolve; upsampling keeps the base.
        let ratio = f64::from(input_rate) / f64::from(output_rate.max(1));
        let mut taps = (BASE_TAPS as f64 * ratio.max(1.0)).round() as usize;
        // An even tap count puts the centre between two weights, so a phase
        // of exactly half a frame lands on a distance of zero.
        taps += taps % 2;
        let center = (taps - 1) as f32 / 2.0;
        let mut kernel = Self {
            taps,
            rows: vec![0.0; taps * PHASES].into_boxed_slice(),
        };

        let cutoff = 0.45 * input_rate.min(output_rate) as f32;
        let cycles = cutoff / input_rate as f32;
        for phase in 0..PHASES {
            let fraction = phase as f32 / PHASES as f32;
            let row = phase * taps..(phase + 1) * taps;
            let mut sum = 0.0;
            for (tap, weight) in kernel.rows[row.clone()].iter_mut().enumerate() {
                let distance = tap as f32 - center - fraction;
                let window =
                    0.46f32.mul_add(-(2.0 * PI * tap as f32 / (taps - 1) as f32).cos(), 0.54);
                // `sinc(0)` is 1 by its limit, which is what keeps this finite
                // at the centre tap.
                *weight = 2.0 * cycles * sinc(2.0 * cycles * distance) * window;
                sum += *weight;
            }
            // Unit DC gain at every phase, so moving between phases cannot
            // modulate the level.
            debug_assert!(sum.abs() > f32::EPSILON, "a kernel row summed to zero");
            for weight in &mut kernel.rows[row] {
                *weight /= sum;
            }
        }
        kernel
    }

    fn row(&self, phase: usize) -> &[f32] {
        let start = phase * self.taps;
        &self.rows[start..start + self.taps]
    }
}

/// The sinc kernel, `sin(pi x) / (pi x)`, with the value 1 at zero.
fn sinc(x: f32) -> f32 {
    if x == 0.0 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    }
}

/// Converts interleaved `f32` frames between two rates, keeping every channel.
///
/// The output for a given input does not depend on how the caller splits that
/// input across [`push`](Self::push) calls.
pub struct Resampler {
    channels: usize,
    input_rate: u32,
    output_rate: u32,
    passthrough: bool,
    kernel: Kernel,
    /// Interleaved input frames, oldest first. `index i` is input frame
    /// `start + i - (taps - 1)`, so the leading zeros stand in for the frames
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
            kernel: Kernel::new(input_rate, output_rate),
            history: Vec::new(),
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
        self.kernel = Kernel::new(input_rate, self.output_rate);
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
        // A bounded count, so a converter that failed to advance its output
        // counter could not spin here forever.
        let count = usize::try_from(target.saturating_sub(self.emitted)).unwrap_or(0);
        out.reserve(count.saturating_mul(channels));
        for _ in 0..count {
            if !self.interpolate(out) {
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
        let taps = self.kernel.taps;
        let channels = self.channels;
        let Some(window) = self
            .history
            .get(offset * channels..(offset + taps) * channels)
        else {
            return false;
        };
        let weights = self.kernel.row(phase);
        for channel in 0..channels {
            let mut sample = 0.0;
            for (tap, weight) in weights.iter().enumerate() {
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
        self.history
            .resize((self.kernel.taps - 1) * self.channels, 0.0);
    }

    /// Drops input no future window can reach.
    ///
    /// `next` is the input frame the next output's window begins at, and it
    /// cannot pass the frames received: the emitted count is bounded by
    /// `received * output_rate / input_rate`, so the history always holds what
    /// the drop asks for. The assertion states that bound instead of clamping
    /// to it.
    fn release_consumed(&mut self) {
        let next = self.emitted * u64::from(self.input_rate) / u64::from(self.output_rate);
        if next <= self.start {
            return;
        }
        debug_assert!(next < self.start + (self.history.len() / self.channels) as u64);
        let drop = usize::try_from(next - self.start).unwrap_or(usize::MAX) * self.channels;
        self.history.drain(..drop);
        self.start = next;
    }
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

    #[allow(clippy::cast_precision_loss)]
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

    /// RMS amplitude of a sine, which does not care whether the window holds
    /// whole cycles.
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
        let input = tone(24_000, 440.0, 4_800, 2);
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
    fn an_out_of_band_tone_is_removed_before_decimation() {
        // The cutoff sits at 0.45 of the 48 kHz output, so 23.5 kHz is past it
        // and would fold to 24.5 kHz if the filter let it through.
        let source = tone(96_000, 23_500.0, 96_000, 1);
        let mut resampler = Resampler::new(1, 96_000, 48_000);
        let mut out = Vec::new();
        resampler.push(&source, &mut out);
        let steady = &out[1_000..out.len() - 1_000];
        assert!(
            amplitude(steady) < 0.01,
            "aliased level {}",
            amplitude(steady)
        );
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

    /// Magnitude of one kernel row at `frequency`, over the input rate.
    fn response(row: &[f32], frequency: f64, rate: u32) -> f64 {
        let (mut real, mut imaginary) = (0.0, 0.0);
        for (tap, weight) in row.iter().enumerate() {
            let angle = 2.0 * std::f64::consts::PI * frequency * tap as f64 / f64::from(rate);
            real = f64::mul_add(f64::from(*weight), angle.cos(), real);
            imaginary = f64::mul_add(f64::from(*weight), -angle.sin(), imaginary);
        }
        real.hypot(imaginary)
    }

    #[test]
    fn every_kernel_phase_holds_the_passband_and_rejects_aliases() {
        for (input, output) in [(48_000, 44_100), (44_100, 48_000), (96_000, 48_000)] {
            let kernel = Kernel::new(input, output);
            let nyquist = 0.5 * f64::from(input.min(output));
            for phase in [0usize, 64, 128, 192, 255] {
                let row = kernel.row(phase);
                for pass in [0.02, 0.3, 0.7] {
                    let gain = response(row, pass * nyquist, input);
                    assert!(
                        (gain - 1.0).abs() < 0.02,
                        "{input}->{output} phase {phase}: gain {gain} at {pass} of Nyquist"
                    );
                }
                for stop in [0.98, 1.0] {
                    let gain = response(row, stop * nyquist, input);
                    assert!(
                        gain < 0.01,
                        "{input}->{output} phase {phase}: gain {gain} at {stop} of Nyquist"
                    );
                }
            }
        }
    }

    #[test]
    fn a_wider_decimation_uses_more_taps() {
        assert_eq!(Kernel::new(48_000, 48_000).taps, BASE_TAPS);
        assert_eq!(Kernel::new(44_100, 48_000).taps, BASE_TAPS);
        assert_eq!(Kernel::new(96_000, 48_000).taps, BASE_TAPS * 2);
        assert_eq!(Kernel::new(192_000, 48_000).taps, BASE_TAPS * 4);
        assert!(Kernel::new(48_000, 44_100).taps >= BASE_TAPS);
    }

    /// One output sample from exact arithmetic on the design constants, at the
    /// same 1/256 phase resolution the table uses.
    fn reference(source: &[f32], input_rate: u32, output_rate: u32, taps: usize, k: usize) -> f64 {
        let scaled = k as u64 * u64::from(input_rate);
        let anchor = usize::try_from(scaled / u64::from(output_rate)).unwrap();
        let phase = (scaled % u64::from(output_rate)) * PHASES as u64 / u64::from(output_rate);
        let fraction = phase as f64 / PHASES as f64;
        let cycles = 0.45 * f64::from(input_rate.min(output_rate)) / f64::from(input_rate);
        let center = (taps - 1) as f64 / 2.0;

        let mut weighted = 0.0;
        let mut weights = 0.0;
        for tap in 0..taps {
            let Some(index) = anchor
                .checked_sub(taps - 1)
                .and_then(|base| base.checked_add(tap))
            else {
                continue;
            };
            let distance = tap as f64 - center - fraction;
            let window = f64::mul_add(
                -0.46,
                (2.0 * std::f64::consts::PI * tap as f64 / (taps - 1) as f64).cos(),
                0.54,
            );
            let weight = if distance == 0.0 {
                2.0 * cycles * window
            } else {
                2.0 * cycles * (std::f64::consts::PI * 2.0 * cycles * distance).sin()
                    / (std::f64::consts::PI * 2.0 * cycles * distance)
                    * window
            };
            weighted = f64::mul_add(f64::from(source[index]), weight, weighted);
            weights += weight;
        }
        weighted / weights
    }

    #[test]
    fn the_table_matches_an_exact_sinc_reference() {
        let source: Vec<f32> = (0..48_000)
            .map(|index| (index as f32 * 0.013).sin() * 0.5)
            .collect();
        for (input, output) in [(44_100, 48_000), (96_000, 48_000)] {
            let mut resampler = Resampler::new(1, input, output);
            let taps = resampler.kernel.taps;
            let mut out = Vec::new();
            resampler.push(&source, &mut out);

            let mut worst = 0.0_f64;
            for k in 2_000..4_000 {
                assert!(
                    k < out.len(),
                    "{input}->{output} delivered {} frames",
                    out.len()
                );
                let expected = reference(&source, input, output, taps, k);
                worst = worst.max((f64::from(out[k]) - expected).abs());
            }
            assert!(worst < 1e-5, "{input}->{output}: worst {worst}");
        }
    }

    #[test]
    fn every_output_frame_is_delivered_exactly_once() {
        // A ramp identifies each frame, so a dropped output shows up as a gap
        // and a repeated one as a backwards step. A linear-phase kernel delays
        // its output by its centre, so each frame maps to an input position
        // that many frames earlier.
        let frames = 4_800usize;
        let source: Vec<f32> = (0..frames).map(|frame| frame as f32).collect();
        let mut resampler = Resampler::new(1, 48_000, 24_000);
        let mut out = Vec::new();
        resampler.push(&source, &mut out);

        assert_eq!(out.len(), frames / 2);
        let delay = (resampler.kernel.taps - 1) as f32 / 2.0;
        for (k, sample) in out.iter().enumerate().skip(resampler.kernel.taps) {
            let expected = (k * 2) as f32 - delay;
            assert!(
                (sample - expected).abs() < 0.5,
                "output {k} was {sample}, expected {expected}"
            );
            assert!(
                out[k] > out[k - 1],
                "output {k} did not advance past its predecessor"
            );
        }
    }

    #[test]
    fn the_history_is_released_as_the_stream_advances() {
        // A converting stream, since a passthrough one never buffers history.
        // Release runs as a push begins, so the stream has to arrive in more
        // than one block for it to have anything to release.
        let mut resampler = Resampler::new(1, 44_100, 48_000);
        let mut out = Vec::new();
        let block = vec![0.5; 441];
        for _ in 0..100 {
            resampler.push(&block, &mut out);
        }
        // One second of audio went through. The history holds the block just
        // pushed plus the window the next output needs; a stream that never
        // released would hold the whole second.
        assert!(
            resampler.history.len() < 1_000,
            "history kept {} samples",
            resampler.history.len()
        );
        assert!(out.len() > 47_000, "emitted {}", out.len());
    }

    #[test]
    fn a_stereo_release_leaves_a_whole_number_of_frames() {
        // The history holds interleaved samples, so a bound counted in samples
        // rather than frames would strand a half frame and shift every later
        // channel pair.
        let mut resampler = Resampler::new(2, 44_100, 48_000);
        let mut out = Vec::new();
        let block = tone(44_100, 440.0, 441, 2);
        for _ in 0..100 {
            resampler.push(&block, &mut out);
            assert_eq!(
                resampler.history.len() % 2,
                0,
                "the history ended mid-frame at {} samples",
                resampler.history.len()
            );
        }
        // 100 blocks of 441 frames is far more than the window needs.
        assert!(
            resampler.history.len() < 4 * 441,
            "the history grew to {} samples",
            resampler.history.len()
        );
    }

    #[test]
    fn a_stereo_stream_releases_in_whole_frames() {
        // Release counts frames, so a stereo history must not end mid-frame.
        let mut whole = Resampler::new(2, 44_100, 48_000);
        let mut expected = Vec::new();
        whole.push(&tone(44_100, 440.0, 44_100, 2), &mut expected);

        let mut split = Resampler::new(2, 44_100, 48_000);
        let mut actual = Vec::new();
        for block in tone(44_100, 440.0, 44_100, 2).chunks(2 * 441) {
            split.push(block, &mut actual);
        }
        assert_eq!(actual.len(), expected.len());
        for (index, (left, right)) in actual.iter().zip(&expected).enumerate() {
            assert!(
                (left - right).abs() < 1e-6,
                "sample {index}: {left} vs {right}"
            );
        }
        assert!(
            split.history.len() < 2_000,
            "history kept {} samples",
            split.history.len()
        );
    }

    #[test]
    fn released_history_does_not_change_the_output() {
        // Releasing is only an optimisation, so a long stream must sound the
        // same as the same frames pushed in one block.
        let mut one = Resampler::new(1, 44_100, 48_000);
        let mut whole = Vec::new();
        one.push(&tone(44_100, 440.0, 44_100, 1), &mut whole);

        let mut many = Resampler::new(1, 44_100, 48_000);
        let mut split = Vec::new();
        for block in tone(44_100, 440.0, 44_100, 1).chunks(441) {
            many.push(block, &mut split);
        }
        assert_eq!(split.len(), whole.len());
        for (left, right) in split.iter().zip(&whole) {
            assert!((left - right).abs() < 1e-6, "{left} vs {right}");
        }
    }

    #[test]
    fn the_released_window_keeps_the_frames_the_next_output_needs() {
        // The output at index k reads a window ending at its anchor, so the
        // history must still hold that window after the previous release.
        let input = tone(44_100, 1_000.0, 8_820, 1);
        let mut whole = Resampler::new(1, 44_100, 48_000);
        let mut expected = Vec::new();
        whole.push(&input, &mut expected);

        // Pushing one frame at a time releases after every frame, exercising
        // the boundary between releases.
        let mut single = Resampler::new(1, 44_100, 48_000);
        let mut actual = Vec::new();
        for sample in &input {
            single.push(std::slice::from_ref(sample), &mut actual);
        }
        assert_eq!(actual.len(), expected.len());
        for (index, (left, right)) in actual.iter().zip(&expected).enumerate() {
            assert!(
                (left - right).abs() < 1e-6,
                "frame {index}: {left} vs {right}"
            );
        }
    }

    #[test]
    fn the_stream_start_needs_no_release() {
        // Before any output exists there is nothing to release, and the history
        // must keep the zero prefix that stands in for the frames before the
        // stream.
        let mut resampler = Resampler::new(1, 96_000, 48_000);
        assert_eq!(resampler.history.len(), resampler.kernel.taps - 1);
        resampler.push(&[0.5], &mut Vec::new());
        assert_eq!(resampler.start, 0);
        assert!(resampler.history.len() >= resampler.kernel.taps - 1);
    }

    #[test]
    fn a_new_segment_starts_from_a_clean_window() {
        let mut resampler = Resampler::new(1, 48_000, 48_000);
        resampler.history.fill(9.0);
        resampler.set_input_rate(44_100);
        assert!(
            resampler.history.iter().all(|sample| *sample == 0.0),
            "a rate change kept {:?}",
            resampler.history
        );
        assert_eq!(resampler.history.len(), resampler.kernel.taps - 1);
        assert_eq!(resampler.received, 0);
        assert_eq!(resampler.emitted, 0);
    }

    #[test]
    fn every_channel_of_the_reset_window_is_zero() {
        let mut resampler = Resampler::new(2, 44_100, 48_000);
        resampler.history.fill(7.0);
        resampler.set_input_rate(96_000);
        assert_eq!(resampler.history.len(), (resampler.kernel.taps - 1) * 2);
        assert!(resampler.history.iter().all(|sample| *sample == 0.0));
    }
}
