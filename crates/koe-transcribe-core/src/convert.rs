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
        while self.emitted < target {
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
