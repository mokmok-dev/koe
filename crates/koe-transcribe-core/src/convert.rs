//! Conversion from a capture format to the engine's 16 kHz mono stream.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::f32::consts::PI;

use crate::config::AudioSpec;

/// Sample rate every engine receives.
pub const ENGINE_RATE: u32 = 16_000;

/// Taps in the resampling kernel.
const TAPS: usize = 48;

/// Fractional positions the kernel holds per input sample.
const PHASES: usize = 256;

/// Distance from the oldest tap to the kernel centre, in input samples.
///
/// The centre sits between the two middle taps, which makes the filter linear
/// phase; its output therefore lags its window by this many samples.
const CENTER: f32 = 23.5;

/// Whole frames in a slice.
///
/// A trailing partial frame is not counted; a zero channel count yields zero.
#[must_use]
pub const fn frames(samples: usize, channels: u16) -> usize {
    if channels == 0 {
        return 0;
    }
    samples / channels as usize
}

/// Downmixes and resamples capture audio into the engine's stream.
///
/// The output for a given input does not depend on how the caller splits that
/// input across [`push`](Self::push) calls.
pub struct Converter {
    sample_rate: u32,
    channels: usize,
    passthrough: bool,
    kernel: Box<[[f32; TAPS]]>,
    /// Mono input not yet released, oldest first. Index `i` is input sample
    /// `buffer_start + i - (TAPS - 1)`, so the leading zeros stand in for the
    /// samples before the recording and the first output keeps a full window.
    buffer: Vec<f32>,
    /// Input index the first buffered sample corresponds to, offset by the
    /// zero prefix.
    buffer_start: u64,
    total_in: u64,
    emitted: u64,
}

impl Converter {
    /// Builds a converter for `spec`.
    #[must_use]
    pub fn new(spec: AudioSpec) -> Self {
        let sample_rate = spec.sample_rate();
        // `AudioSpec` validates this range; the check keeps the resampling
        // ratio bounded if a converter is ever built from a hand-made spec.
        debug_assert!(crate::config::MIN_SAMPLE_RATE <= sample_rate);
        let channels = usize::from(spec.channels());
        Self {
            sample_rate,
            channels,
            passthrough: sample_rate == ENGINE_RATE && channels == 1,
            kernel: build_kernel(sample_rate),
            buffer: vec![0.0; TAPS - 1],
            buffer_start: 0,
            total_in: 0,
            emitted: 0,
        }
    }

    /// Whether the input already is the engine's format.
    #[must_use]
    pub const fn is_passthrough(&self) -> bool {
        self.passthrough
    }

    /// Appends the converted form of `interleaved` to `out`.
    ///
    /// Exactly `floor(total input frames * ENGINE_RATE / sample rate) -
    /// already emitted` samples are appended. A trailing partial frame is
    /// ignored.
    pub fn push(&mut self, interleaved: &[f32], out: &mut Vec<f32>) {
        let count = interleaved.len() / self.channels;
        if count == 0 {
            return;
        }
        if self.passthrough {
            out.extend_from_slice(&interleaved[..count]);
            return;
        }

        self.release_consumed();
        for frame in 0..count {
            let start = frame * self.channels;
            let sum: f32 = interleaved[start..start + self.channels].iter().sum();
            #[allow(clippy::cast_precision_loss)]
            self.buffer.push(sum / self.channels as f32);
        }
        self.total_in += u64::try_from(count).unwrap_or(u64::MAX);

        let target = self.total_in * u64::from(ENGINE_RATE) / u64::from(self.sample_rate);
        for _ in 0..target.saturating_sub(self.emitted) {
            let Some(sample) = self.interpolate(self.emitted) else {
                break;
            };
            out.push(sample);
            self.emitted += 1;
        }
    }

    /// One output sample: the window ending at the input position `k` maps to.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn interpolate(&self, k: u64) -> Option<f32> {
        let scaled = k * u64::from(self.sample_rate);
        let anchor = scaled / u64::from(ENGINE_RATE);
        let remainder = scaled % u64::from(ENGINE_RATE);
        let phase = (remainder * PHASES as u64 / u64::from(ENGINE_RATE)) as usize;

        let offset = usize::try_from(anchor.checked_sub(self.buffer_start)?).ok()?;
        let window = self.buffer.get(offset..offset + TAPS)?;
        let mut sample = 0.0;
        for (value, tap) in window.iter().zip(&self.kernel[phase]) {
            sample += value * tap;
        }
        Some(sample)
    }

    #[cfg(test)]
    const fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// Drops input no future window can reach.
    fn release_consumed(&mut self) {
        let next = self.emitted * u64::from(self.sample_rate) / u64::from(ENGINE_RATE);
        if next <= self.buffer_start {
            return;
        }
        let drop = usize::try_from(next - self.buffer_start)
            .unwrap_or(usize::MAX)
            .min(self.buffer.len());
        self.buffer.drain(..drop);
        self.buffer_start = next;
    }
}

/// Builds the windowed-sinc kernel, one row per fractional phase.
///
/// Each row is scaled to unit DC gain so a phase change cannot modulate level.
#[allow(clippy::cast_precision_loss)]
fn build_kernel(sample_rate: u32) -> Box<[[f32; TAPS]]> {
    let cutoff = 0.45 * sample_rate.min(ENGINE_RATE) as f32;
    let cycles = cutoff / sample_rate as f32;
    // The cutoff is 0.45 of the lower rate, so the kernel never reaches Nyquist.
    debug_assert!(cycles <= 0.5);
    let mut kernel = vec![[0.0_f32; TAPS]; PHASES];
    for (phase, row) in kernel.iter_mut().enumerate() {
        let fraction = phase as f32 / PHASES as f32;
        for (tap, weight) in row.iter_mut().enumerate() {
            let distance = tap as f32 - CENTER - fraction;
            let window = libm::fmaf(
                -0.46,
                libm::cosf(2.0 * PI * tap as f32 / (TAPS - 1) as f32),
                0.54,
            );
            let gain = if distance == 0.0 {
                2.0 * cycles
            } else {
                libm::sinf(2.0 * PI * cycles * distance) / (PI * distance)
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
        clippy::cast_sign_loss,
        clippy::needless_range_loop
    )]

    use super::*;
    use proptest::prelude::*;

    fn spec(sample_rate: u32, channels: u16) -> AudioSpec {
        AudioSpec::new(sample_rate, channels).unwrap()
    }

    /// Amplitude of `tone` in `samples`, which must hold whole cycles.
    fn amplitude(samples: &[f32], cycles: usize) -> f64 {
        let count = samples.len();
        let step = 2.0 * std::f64::consts::PI * cycles as f64 / count as f64;
        let coefficient = 2.0 * step.cos();
        let (mut previous, mut previous_previous) = (0.0, 0.0);
        for sample in samples {
            let current = coefficient.mul_add(previous, f64::from(*sample)) - previous_previous;
            previous_previous = previous;
            previous = current;
        }
        let power = (coefficient * previous).mul_add(
            -previous_previous,
            previous.mul_add(previous, previous_previous * previous_previous),
        );
        2.0 * power.max(0.0).sqrt() / count as f64
    }

    fn tone(sample_rate: u32, frequency: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|frame| {
                (0.5 * (2.0 * std::f64::consts::PI * frequency * frame as f64
                    / f64::from(sample_rate))
                .sin()) as f32
            })
            .collect()
    }

    #[test]
    fn passthrough_appends_the_input_verbatim() {
        let mut converter = Converter::new(spec(ENGINE_RATE, 1));
        assert!(converter.is_passthrough());
        let input: Vec<f32> = (0..100).map(|index| index as f32 * 0.01).collect();
        let mut out = vec![7.0];
        converter.push(&input, &mut out);
        assert_eq!(out[0], 7.0);
        assert_eq!(&out[1..], input.as_slice());
    }

    #[test]
    fn only_sixteen_kilohertz_mono_is_passthrough() {
        assert!(Converter::new(spec(16_000, 1)).is_passthrough());
        assert!(!Converter::new(spec(16_000, 2)).is_passthrough());
        assert!(!Converter::new(spec(48_000, 1)).is_passthrough());
    }

    #[test]
    fn passthrough_keeps_working_across_calls() {
        let mut converter = Converter::new(spec(ENGINE_RATE, 1));
        let mut out = Vec::new();
        converter.push(&[0.25, 0.5], &mut out);
        converter.push(&[0.75], &mut out);
        converter.push(&[], &mut out);
        assert_eq!(out, [0.25, 0.5, 0.75]);
    }

    #[test]
    fn a_misaligned_tail_is_ignored() {
        let mut converter = Converter::new(spec(48_000, 2));
        let mut out = Vec::new();
        converter.push(&[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.0], &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(frames(3, 2), 1);
        assert_eq!(frames(3, 0), 0);
    }

    #[test]
    fn downmixes_every_channel_count_to_its_mean() {
        for channels in [1u16, 2, 6] {
            let samples: Vec<f32> = (0..usize::from(channels)).map(|_| 0.25).collect();
            let input: Vec<f32> = samples.repeat(400);
            let mean = input.iter().sum::<f32>() / 400.0 / f32::from(channels);

            let mut converter = Converter::new(spec(ENGINE_RATE, channels));
            let mut out = Vec::new();
            converter.push(&input, &mut out);

            assert_eq!(out.len(), 400);
            for sample in &out[TAPS..] {
                assert!(
                    (sample - mean).abs() < 1e-5,
                    "{channels}: {sample} vs {mean}"
                );
            }
        }
    }

    #[test]
    fn resampling_delivers_the_documented_sample_count() {
        for (rate, channels) in [(48_000, 2), (44_100, 1), (8_000, 2), (16_000, 2)] {
            let mut converter = Converter::new(spec(rate, channels));
            let input_frames = 4_800;
            let input = vec![0.1; input_frames * usize::from(channels)];
            let mut out = Vec::new();
            converter.push(&input, &mut out);
            assert_eq!(
                out.len(),
                input_frames * ENGINE_RATE as usize / rate as usize,
                "rate {rate}"
            );
        }
    }

    #[test]
    fn a_one_kilohertz_tone_survives_resampling() {
        let frames = 48_000;
        let mono = tone(48_000, 1_000.0, frames);
        let input: Vec<f32> = mono.iter().flat_map(|sample| [*sample, *sample]).collect();

        let mut converter = Converter::new(spec(48_000, 2));
        let mut out = Vec::new();
        converter.push(&input, &mut out);
        assert_eq!(out.len(), ENGINE_RATE as usize);

        let expected = amplitude(&mono[24_000..], 500);
        let actual = amplitude(&out[8_000..], 500);
        assert!(
            (actual - expected).abs() / expected < 0.01,
            "expected {expected}, got {actual}"
        );
    }

    /// Magnitude of one kernel row at `frequency`, over the input rate.
    fn response(kernel: &[f32; TAPS], frequency: f64, rate: u32) -> f64 {
        let (mut real, mut imaginary) = (0.0, 0.0);
        for (tap, weight) in kernel.iter().enumerate() {
            let angle = 2.0 * std::f64::consts::PI * frequency * tap as f64 / f64::from(rate);
            real = f64::mul_add(f64::from(*weight), angle.cos(), real);
            imaginary = f64::mul_add(f64::from(*weight), -angle.sin(), imaginary);
        }
        real.hypot(imaginary)
    }

    #[test]
    fn frames_counts_whole_channel_groups() {
        assert_eq!(frames(10, 3), 3);
        assert_eq!(frames(9, 3), 3);
        assert_eq!(frames(48_002, 2), 24_001);
    }

    #[test]
    fn every_kernel_phase_holds_the_speech_band_and_rejects_aliases() {
        for rate in [48_000u32, 44_100] {
            let converter = Converter::new(spec(rate, 1));
            for phase in [0usize, 64, 128, 192, 255] {
                let row = &converter.kernel[phase];
                for pass in [300.0, 1_000.0, 3_000.0] {
                    let gain = response(row, pass, rate);
                    assert!(
                        (gain - 1.0).abs() < 0.02,
                        "{rate} Hz, phase {phase}, {pass} Hz: {gain}"
                    );
                }
                for stop in [10_000.0, 12_000.0] {
                    let gain = response(row, stop, rate);
                    assert!(gain < 0.01, "{rate} Hz, phase {phase}, {stop} Hz: {gain}");
                }
            }
        }
    }

    /// One output sample computed with exact arithmetic from the design
    /// constants, at the same 1/256 phase resolution the table uses.
    fn reference(source: &[f32], rate: u32, k: usize) -> f64 {
        let scaled = k as u64 * u64::from(rate);
        let anchor = usize::try_from(scaled / 16_000).unwrap();
        let phase = (scaled % 16_000) * 256 / 16_000;
        let fraction = phase as f64 / 256.0;
        let cycles = 0.45 * f64::from(rate.min(16_000)) / f64::from(rate);

        let mut weighted = 0.0;
        let mut weights = 0.0;
        for tap in 0..48_usize {
            let Some(index) = anchor
                .checked_sub(47)
                .and_then(|base| base.checked_add(tap))
            else {
                continue;
            };
            let distance = tap as f64 - 23.5 - fraction;
            let window = f64::mul_add(
                -0.46,
                (2.0 * std::f64::consts::PI * tap as f64 / 47.0).cos(),
                0.54,
            );
            let gain = if distance == 0.0 {
                2.0 * cycles
            } else {
                (2.0 * std::f64::consts::PI * cycles * distance).sin()
                    / (std::f64::consts::PI * distance)
            };
            let weight = gain * window;
            weighted = f64::mul_add(f64::from(source[index]), weight, weighted);
            weights += weight;
        }
        weighted / weights
    }

    #[test]
    fn the_table_matches_an_exact_sinc_reference() {
        let source: Vec<f32> = (0..44_100)
            .map(|index| (index as f32 * 0.013).sin() * 0.5)
            .collect();
        let mut converter = Converter::new(spec(44_100, 1));
        let mut out = Vec::new();
        converter.push(&source, &mut out);

        let mut worst = 0.0_f64;
        for k in 2_000..4_000 {
            let expected = reference(&source, 44_100, k);
            worst = worst.max((f64::from(out[k]) - expected).abs());
        }
        assert!(worst < 1e-5, "worst {worst}");
    }

    #[test]
    fn a_fractional_ratio_tone_keeps_its_level() {
        let source = tone(44_100, 3_000.0, 44_100);
        let mut converter = Converter::new(spec(44_100, 1));
        let mut out = Vec::new();
        converter.push(&source, &mut out);

        let expected = amplitude(&source[22_050..], 1_500);
        let actual = amplitude(&out[8_000..], 1_500);
        assert!(
            (actual - expected).abs() / expected < 0.02,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn an_out_of_band_tone_is_removed_before_decimation() {
        let source = tone(48_000, 11_000.0, 48_000);
        let mut converter = Converter::new(spec(48_000, 1));
        let mut out = Vec::new();
        converter.push(&source, &mut out);

        let input_level = amplitude(&source[24_000..], 5_500);
        let aliased = amplitude(&out[8_000..], 2_500);
        assert!(
            aliased / input_level < 0.01,
            "alias level {aliased} against source {input_level}"
        );
    }

    #[test]
    fn the_input_history_stays_bounded() {
        let mut converter = Converter::new(spec(48_000, 1));
        let mut out = Vec::new();
        for _ in 0..2_400 {
            converter.push(&[0.1], &mut out);
        }
        assert!(
            converter.buffered() < 512,
            "history grew to {} samples",
            converter.buffered()
        );
    }

    #[test]
    fn a_steady_tone_passes_without_level_drift() {
        let mut converter = Converter::new(spec(48_000, 1));
        let mono = tone(48_000, 1_000.0, 12_000);
        let mut out = Vec::new();
        for chunk in mono.chunks(96) {
            converter.push(chunk, &mut out);
        }
        let early = amplitude(&out[160..1_760], 100);
        let late = amplitude(&out[2_400..4_000], 100);
        assert!((early - late).abs() / early < 0.001, "{early} vs {late}");
    }

    proptest! {
        #[test]
        fn chunking_does_not_change_the_output(
            rate in prop::sample::select(vec![8_000u32, 11_025, 16_000, 22_050, 44_100, 48_000, 96_000, 192_000, 384_000]),
            channels in 1u16..=8,
            cuts in proptest::collection::vec(0usize..97, 0..12),
        ) {
            let frames = 512;
            let input: Vec<f32> = (0..frames * usize::from(channels))
                .map(|index| ((index as f32) * 0.017).sin() * 0.5)
                .collect();

            let mut whole = Converter::new(spec(rate, channels));
            let mut expected = Vec::new();
            whole.push(&input, &mut expected);

            let mut chunked = Converter::new(spec(rate, channels));
            let mut actual = Vec::new();
            let mut start = 0usize;
            for cut in cuts {
                let end = (start + cut).min(frames);
                chunked.push(&input[start * usize::from(channels)..end * usize::from(channels)], &mut actual);
                start = end;
            }
            chunked.push(&input[start * usize::from(channels)..], &mut actual);

            prop_assert_eq!(actual.len(), frames * ENGINE_RATE as usize / rate as usize);
            prop_assert_eq!(&actual, &expected);
        }

        #[test]
        fn every_whole_frame_count_produces_the_exact_output_count(
            frames in 0usize..600,
            rate in prop::sample::select(vec![8_000u32, 16_000, 44_100, 48_000]),
        ) {
            let mut converter = Converter::new(spec(rate, 1));
            let input = vec![0.0; frames];
            let mut out = Vec::new();
            converter.push(&input, &mut out);
            prop_assert_eq!(out.len(), frames * ENGINE_RATE as usize / rate as usize);
        }
    }
}
