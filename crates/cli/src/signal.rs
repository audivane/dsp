//! Deterministic test signals and small measurement helpers.

/// xorshift64* — tiny, deterministic, good enough for test noise.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / 9_007_199_254_740_992.0)
    }

    /// Uniform in [-1, 1).
    pub fn next_bipolar(&mut self) -> f64 {
        2.0 * self.next_f64() - 1.0
    }
}

/// Peak in dBFS (`NEG_INFINITY` for digital silence).
pub fn peak_dbfs(peak: f64) -> f64 {
    if peak > 0.0 {
        20.0 * peak.log10()
    } else {
        f64::NEG_INFINITY
    }
}
