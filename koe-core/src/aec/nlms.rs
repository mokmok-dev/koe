//! Normalized Least Mean Squares adaptive filter.

const EPS: f32 = 1e-8;

/// FIR adaptive filter updated with the NLMS rule.
#[derive(Debug, Clone)]
pub(super) struct NlmsFilter {
    taps: Vec<f32>,
    /// Circular far-end history; index `write` is the next slot to overwrite.
    history: Vec<f32>,
    write: usize,
    /// Recursively maintained `||x||²` over the history window.
    power: f32,
    step_size: f32,
}

impl NlmsFilter {
    pub(super) fn new(
        length: usize,
        step_size: f32,
    ) -> Self {
        let length = length.max(1);
        Self {
            taps: vec![0.0; length],
            history: vec![0.0; length],
            write: 0,
            power: 0.0,
            step_size,
        }
    }

    #[cfg(test)]
    pub(super) fn taps(&self) -> &[f32] {
        &self.taps
    }

    pub(super) fn reset(&mut self) {
        self.taps.fill(0.0);
        self.history.fill(0.0);
        self.write = 0;
        self.power = 0.0;
    }

    /// Pushes one far-end sample and returns the echo estimate `y`.
    pub(super) fn predict(
        &mut self,
        far: f32,
    ) -> f32 {
        let len = self.taps.len();
        let outgoing = self.history[self.write];
        self.history[self.write] = far;
        self.power = far
            .mul_add(far, outgoing.mul_add(-outgoing, self.power))
            .max(0.0);
        self.write = (self.write + 1) % len;

        let mut y = 0.0_f32;
        // Newest sample sits at `write - 1`; tap 0 multiplies the newest sample.
        let mut idx = self.write.checked_sub(1).unwrap_or(len - 1);
        for tap in &self.taps {
            y = tap.mul_add(self.history[idx], y);
            idx = idx.checked_sub(1).unwrap_or(len - 1);
        }
        y
    }

    /// NLMS coefficient update given residual `error` (near − estimate).
    pub(super) fn adapt(
        &mut self,
        error: f32,
    ) {
        let len = self.taps.len();
        let norm = self.step_size * error / (self.power + EPS);
        let mut idx = self.write.checked_sub(1).unwrap_or(len - 1);
        for tap in &mut self.taps {
            *tap = norm.mul_add(self.history[idx], *tap);
            idx = idx.checked_sub(1).unwrap_or(len - 1);
        }
    }

    /// Maximum absolute value currently stored in the far-end history.
    pub(super) fn max_abs_history(&self) -> f32 {
        self.history
            .iter()
            .fold(0.0_f32, |acc, &sample| acc.max(sample.abs()))
    }
}
