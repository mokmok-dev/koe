//! Acoustic echo cancellation (full NLMS in task 21).

/// Configuration for the echo canceller.
#[derive(Debug, Clone)]
pub struct AecConfig {
    /// Adaptive filter length in taps.
    pub filter_length: usize,
    /// Processing block size in samples.
    pub block_size: usize,
    /// NLMS step size.
    pub step_size: f32,
    /// Double-talk detection threshold in dB.
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

/// Passthrough AEC stub until task 21 implements NLMS filtering.
pub struct AcousticEchoCanceller {
    config: AecConfig,
    erle_db: f32,
}

impl AcousticEchoCanceller {
    /// Creates a new echo canceller with the given configuration.
    #[must_use]
    pub const fn new(config: AecConfig) -> Self {
        Self {
            config,
            erle_db: 0.0,
        }
    }

    /// Processes one block of far-end (reference) and near-end (mic) audio.
    ///
    /// Returns echo-cancelled near-end samples. The stub passes `near_end`
    /// through unchanged.
    #[must_use]
    pub fn process_block(
        &mut self,
        _far_end: &[f32],
        near_end: &[f32],
    ) -> Vec<f32> {
        near_end.to_vec()
    }

    /// Resets adaptive filter state.
    pub const fn reset(&mut self) {
        self.erle_db = 0.0;
    }

    /// Echo Return Loss Enhancement in dB.
    #[must_use]
    pub const fn erle(&self) -> f32 {
        self.erle_db
    }

    /// Active configuration.
    #[must_use]
    pub const fn config(&self) -> &AecConfig {
        &self.config
    }
}
