//! Acoustic echo cancellation (NLMS + Geigel double-talk + comfort noise).

mod comfort;
mod double_talk;
mod nlms;

use comfort::ComfortNoise;
use double_talk::GeigelDetector;
use nlms::NlmsFilter;

/// Configuration for the echo canceller.
#[derive(Debug, Clone)]
pub struct AecConfig {
    /// Adaptive filter length in taps (~85 ms at 48 kHz when 4096).
    pub filter_length: usize,
    /// Preferred processing block size in samples (~5.3 ms when 256).
    ///
    /// [`AcousticEchoCanceller::process_block`] accepts any equal-length pair;
    /// this field documents the latency target for callers.
    pub block_size: usize,
    /// NLMS step size (μ), normalized by far-end power.
    pub step_size: f32,
    /// Double-talk detection threshold in dB above far-end peak.
    pub double_talk_threshold_db: f32,
    /// Whether to inject comfort noise during echo-only periods.
    pub comfort_noise: bool,
}

impl Default for AecConfig {
    fn default() -> Self {
        Self {
            filter_length: 4096,
            block_size: 256,
            step_size: 0.01,
            double_talk_threshold_db: 6.0,
            comfort_noise: true,
        }
    }
}

/// Acoustic echo canceller: removes far-end leakage from the near-end mic.
pub struct AcousticEchoCanceller {
    config: AecConfig,
    filter: NlmsFilter,
    detector: GeigelDetector,
    comfort: ComfortNoise,
    /// Smoothed near-end power for ERLE.
    near_power: f32,
    /// Smoothed residual power for ERLE.
    error_power: f32,
    erle_db: f32,
    /// Far-end activity gate for comfort-noise / echo-only detection.
    far_power: f32,
}

impl AcousticEchoCanceller {
    /// Creates a new echo canceller with the given configuration.
    #[must_use]
    pub fn new(config: AecConfig) -> Self {
        let filter = NlmsFilter::new(config.filter_length, config.step_size);
        let detector = GeigelDetector::from_db(config.double_talk_threshold_db);
        let comfort = ComfortNoise::new(config.comfort_noise);
        Self {
            config,
            filter,
            detector,
            comfort,
            near_power: 1e-8,
            error_power: 1e-8,
            erle_db: 0.0,
            far_power: 0.0,
        }
    }

    /// Processes one block of far-end (reference) and near-end (mic) audio.
    ///
    /// Returns echo-cancelled near-end samples. Length equals
    /// `far_end.len().min(near_end.len())`. Channels are the caller's
    /// responsibility — pass one channel (or a downmix) per call.
    #[must_use]
    pub fn process_block(
        &mut self,
        far_end: &[f32],
        near_end: &[f32],
    ) -> Vec<f32> {
        let len = far_end.len().min(near_end.len());
        let mut out = Vec::with_capacity(len);

        for i in 0..len {
            let far = far_end[i];
            let near = near_end[i];

            self.comfort.observe_near(near);
            self.far_power = 0.01f32.mul_add(far * far, 0.99 * self.far_power);

            let y = self.filter.predict(far);
            let error = near - y;

            let max_far = self.filter.max_abs_history();
            let double_talk = self.detector.is_double_talk(near, max_far);
            if !double_talk {
                self.filter.adapt(error);
            }

            // Echo-only: far-end is active and Geigel did not see near-end speech.
            let echo_only = !double_talk && self.far_power > 1e-6;
            let sample = self.comfort.maybe_mix(error, echo_only);
            out.push(sample);

            self.near_power = 0.01f32.mul_add(near * near, 0.99 * self.near_power);
            self.error_power = 0.01f32.mul_add(error * error, 0.99 * self.error_power);
        }

        if self.near_power > 1e-12 && self.error_power > 0.0 {
            self.erle_db = 10.0 * (self.near_power / self.error_power).log10();
        }

        out
    }

    /// Resets adaptive filter state, comfort noise, and ERLE estimates.
    pub fn reset(&mut self) {
        self.filter.reset();
        self.detector.reset();
        self.comfort.reset();
        self.near_power = 1e-8;
        self.error_power = 1e-8;
        self.erle_db = 0.0;
        self.far_power = 0.0;
    }

    /// Echo Return Loss Enhancement in dB (smoothed).
    #[must_use]
    pub const fn erle(&self) -> f32 {
        self.erle_db
    }

    /// Active configuration.
    #[must_use]
    pub const fn config(&self) -> &AecConfig {
        &self.config
    }

    /// Current comfort-noise floor estimate.
    #[must_use]
    pub const fn noise_floor(&self) -> f32 {
        self.comfort.noise_floor()
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_precision_loss,
    reason = "test helpers use small sample counts that fit f32"
)]
mod tests {
    use super::*;

    fn sine(
        n: usize,
        freq_hz: f32,
        sample_rate: f32,
        amp: f32,
    ) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / sample_rate;
                (2.0 * std::f32::consts::PI * freq_hz * t).sin() * amp
            })
            .collect()
    }

    /// Applies a pure delay + gain echo path (single-tap FIR).
    fn apply_echo(
        far: &[f32],
        delay: usize,
        gain: f32,
    ) -> Vec<f32> {
        let mut out = vec![0.0; far.len()];
        for (i, &sample) in far.iter().enumerate() {
            let j = i + delay;
            if j < out.len() {
                out[j] = sample * gain;
            }
        }
        out
    }

    fn rms(signal: &[f32]) -> f32 {
        if signal.is_empty() {
            return 0.0;
        }
        let sum: f32 = signal.iter().map(|s| s * s).sum();
        (sum / signal.len() as f32).sqrt()
    }

    fn erle_db(
        reference: &[f32],
        residual: &[f32],
    ) -> f32 {
        let ref_p: f32 = reference.iter().map(|s| s * s).sum();
        let err_p: f32 = residual.iter().map(|s| s * s).sum::<f32>().max(1e-20);
        10.0 * (ref_p / err_p).log10()
    }

    #[test]
    fn default_config_matches_spec() {
        let cfg = AecConfig::default();
        assert_eq!(cfg.filter_length, 4096);
        assert_eq!(cfg.block_size, 256);
        assert!((cfg.step_size - 0.01).abs() < f32::EPSILON);
        assert!((cfg.double_talk_threshold_db - 6.0).abs() < f32::EPSILON);
        assert!(cfg.comfort_noise);
    }

    #[test]
    fn synthetic_loopback_cancels_echo_with_erle_above_20db() {
        // Smaller filter keeps the test fast while still covering multi-tap NLMS.
        let config = AecConfig {
            filter_length: 128,
            block_size: 64,
            step_size: 0.25,
            double_talk_threshold_db: 6.0,
            comfort_noise: false,
        };
        let mut aec = AcousticEchoCanceller::new(config);

        let sample_rate = 16_000.0_f32;
        let total = 16_000; // 1 second
        let delay = 20usize;
        let gain = 0.6_f32;
        let far = sine(total, 440.0, sample_rate, 0.5);
        let near = apply_echo(&far, delay, gain);

        let mut residual = Vec::with_capacity(total);
        for chunk in far.chunks(64).zip(near.chunks(64)) {
            let (f, n) = chunk;
            residual.extend(aec.process_block(f, n));
        }

        // Evaluate ERLE on the last 25% after convergence.
        let start = total * 3 / 4;
        let measured = erle_db(&near[start..], &residual[start..]);
        assert!(
            measured > 20.0,
            "ERLE {measured:.1} dB below 20 dB target (aec.erle={})",
            aec.erle()
        );
        assert!(
            rms(&residual[start..]) < 0.05 * rms(&near[start..]),
            "residual too loud: rms_out={} rms_near={}",
            rms(&residual[start..]),
            rms(&near[start..])
        );
    }

    #[test]
    fn double_talk_freezes_adaptation_but_keeps_filtering() {
        let config = AecConfig {
            filter_length: 64,
            block_size: 32,
            step_size: 0.3,
            double_talk_threshold_db: 6.0,
            comfort_noise: false,
        };
        let mut aec = AcousticEchoCanceller::new(config);

        let far = sine(4_000, 300.0, 16_000.0, 0.4);
        let echo = apply_echo(&far, 8, 0.7);

        // Converge on echo-only.
        for (f, n) in far.chunks(32).zip(echo.chunks(32)) {
            let _ = aec.process_block(f, n);
        }
        let taps_before: Vec<f32> = aec.filter.taps().to_vec();

        // Loud constant near-end speech on top of the same echo → Geigel double-talk.
        let mut near_dt = echo[4_000 - 2_000..4_000].to_vec();
        for n in &mut near_dt {
            *n += 1.0;
        }
        let far_dt = &far[4_000 - 2_000..4_000];

        let mut filtered = Vec::new();
        for (f, n) in far_dt.chunks(32).zip(near_dt.chunks(32)) {
            filtered.extend(aec.process_block(f, n));
        }

        let taps_after = aec.filter.taps();
        let max_delta = taps_before
            .iter()
            .zip(taps_after.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f32, f32::max);
        assert!(
            max_delta < 1e-4,
            "filter adapted during double-talk (max Δ={max_delta})"
        );

        // Filtering still runs: residual speech should not equal raw mic.
        let diff_vs_raw: f32 = filtered
            .iter()
            .zip(near_dt.iter())
            .map(|(a, b)| (a - b).abs())
            .sum();
        assert!(
            diff_vs_raw > 1.0,
            "expected filtering to alter the near-end during double-talk"
        );
    }

    #[test]
    fn converges_from_zero_within_one_second() {
        let config = AecConfig {
            filter_length: 64,
            block_size: 64,
            step_size: 0.4,
            double_talk_threshold_db: 12.0, // less sensitive so adaptation runs
            comfort_noise: false,
        };
        let mut aec = AcousticEchoCanceller::new(config);

        let total = 8_000; // 0.5 s at 16 kHz — well under 1 s budget
        let far = sine(total, 500.0, 16_000.0, 0.5);
        let near = apply_echo(&far, 10, 0.8);

        let mut residual = Vec::with_capacity(total);
        for (f, n) in far.chunks(64).zip(near.chunks(64)) {
            residual.extend(aec.process_block(f, n));
        }

        let start = total * 3 / 4;
        let measured = erle_db(&near[start..], &residual[start..]);
        assert!(
            measured > 15.0,
            "failed to converge within 1s window (ERLE={measured:.1} dB)"
        );
        assert!(aec.filter.taps().iter().any(|t| t.abs() > 0.1));
    }

    #[test]
    fn reset_clears_filter_and_erle() {
        let mut aec = AcousticEchoCanceller::new(AecConfig {
            filter_length: 32,
            block_size: 16,
            step_size: 0.5,
            comfort_noise: true,
            ..AecConfig::default()
        });
        let far = vec![0.3; 256];
        let near = vec![0.2; 256];
        let _ = aec.process_block(&far, &near);
        assert!(aec.filter.taps().iter().any(|t| t.abs() > 0.0));

        aec.reset();
        assert!(aec.filter.taps().iter().all(|&t| t == 0.0));
        assert!((aec.erle() - 0.0).abs() < f32::EPSILON);
        assert!((aec.noise_floor() - 1e-4).abs() < 1e-9);
    }

    #[test]
    fn comfort_noise_changes_echo_only_output() {
        let base = AecConfig {
            filter_length: 32,
            block_size: 16,
            step_size: 0.01,
            double_talk_threshold_db: 6.0,
            comfort_noise: false,
        };
        let mut silent = AcousticEchoCanceller::new(base.clone());
        let mut noisy = AcousticEchoCanceller::new(AecConfig {
            comfort_noise: true,
            ..base
        });

        // Converge both, then compare echo-only residual with/without noise.
        let far = sine(2_048, 400.0, 16_000.0, 0.4);
        let near = apply_echo(&far, 4, 0.5);
        for (f, n) in far.chunks(16).zip(near.chunks(16)) {
            let _ = silent.process_block(f, n);
            let _ = noisy.process_block(f, n);
        }

        let far_tail = &far[far.len() - 64..];
        let near_tail = &near[near.len() - 64..];
        let out_silent = silent.process_block(far_tail, near_tail);
        let out_noisy = noisy.process_block(far_tail, near_tail);
        let diff: f32 = out_silent
            .iter()
            .zip(out_noisy.iter())
            .map(|(a, b)| (a - b).abs())
            .sum();
        assert!(diff > 0.0, "comfort noise should alter echo-only output");
    }

    #[test]
    fn mismatched_lengths_process_common_prefix() {
        let mut aec = AcousticEchoCanceller::new(AecConfig {
            filter_length: 16,
            block_size: 8,
            comfort_noise: false,
            ..AecConfig::default()
        });
        let out = aec.process_block(&[0.1, 0.2, 0.3], &[0.1, 0.2]);
        assert_eq!(out.len(), 2);
    }
}
