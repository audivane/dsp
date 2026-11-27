//! Rational resampling ratios: output rate / input rate = `up / down`,
//! reduced by their greatest common divisor.
//!
//! The engine's block arithmetic for rational resampling lives here so the
//! drivers, the daemon and dsp-core count frames the same way.
//!
//! Frame grid: output frame `m` sits at time `m * down` and input frame `k`
//! at time `k * up` on the intermediate grid (rate `input * up`). Output `m`
//! needs input frames up to `floor(m * down / up)`, so after `K` input frames
//! exactly `ceil(K * up / down)` output frames exist. The number a block
//! produces therefore depends only on the input position (modulo `down`) and
//! the block length, never on the block history: every chain that has seen
//! the same number of input frames emits the same number of output frames.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Largest numerator or denominator the engine accepts (a 1 Hz grid at
/// 768 kHz still fits, but kernels and validation limits are far lower).
pub const MAX_RATIO_TERM: u32 = 1 << 20;

/// `output rate / input rate = up / down`, always reduced, both terms `>= 1`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RateRatio {
    /// L: interpolation factor (numerator).
    pub up: u32,
    /// M: decimation factor (denominator).
    pub down: u32,
}

impl Default for RateRatio {
    fn default() -> Self {
        Self::ONE
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

impl RateRatio {
    /// 1/1: no resampling.
    pub const ONE: Self = Self { up: 1, down: 1 };

    /// `up / down` reduced. Zero terms are treated as 1.
    pub fn new(up: u32, down: u32) -> Self {
        let (u, d) = (u64::from(up.max(1)), u64::from(down.max(1)));
        let g = gcd(u, d);
        Self {
            up: (u / g) as u32,
            down: (d / g) as u32,
        }
    }

    /// An integer upsampling ratio (`r / 1`; 0 is treated as 1).
    pub fn integer(r: u32) -> Self {
        Self {
            up: r.max(1),
            down: 1,
        }
    }

    /// The ratio that takes `input_rate` to `output_rate`.
    pub fn from_rates(input_rate: u32, output_rate: u32) -> Self {
        Self::new(output_rate, input_rate)
    }

    pub fn is_unity(self) -> bool {
        self.up == 1 && self.down == 1
    }

    /// `down == 1`: a plain integer upsampling ratio (unity included).
    pub fn is_integer(self) -> bool {
        self.down == 1
    }

    /// Output rate below the input rate.
    pub fn is_decimating(self) -> bool {
        self.up < self.down
    }

    /// The integer ratio when [`Self::is_integer`], else `None`.
    pub fn as_integer(self) -> Option<u32> {
        self.is_integer().then_some(self.up)
    }

    /// `up / down` as a float.
    pub fn as_f64(self) -> f64 {
        f64::from(self.up) / f64::from(self.down)
    }

    /// `input_rate * up / down` when that is a whole number of Hz.
    pub fn output_rate(self, input_rate: u32) -> Option<u32> {
        let num = u64::from(input_rate) * u64::from(self.up);
        let d = u64::from(self.down);
        (num % d == 0)
            .then(|| num / d)
            .and_then(|r| u32::try_from(r).ok())
    }

    /// `n` input-rate frames as output-rate frames, rounded to the nearest
    /// (for nominal sizes such as an ALSA period from a graph quantum).
    pub fn scale_to_output(self, n: u32) -> u32 {
        let num = u64::from(n) * u64::from(self.up);
        let d = u64::from(self.down);
        ((num + d / 2) / d).min(u64::from(u32::MAX)) as u32
    }

    /// `n` output-rate frames as input-rate frames, rounded to the nearest.
    pub fn scale_to_input(self, n: u32) -> u32 {
        let num = u64::from(n) * u64::from(self.down);
        let u = u64::from(self.up);
        ((num + u / 2) / u).min(u64::from(u32::MAX)) as u32
    }

    /// The ratio packed into one `u64` (`up << 32 | down`), for an atomic.
    pub fn to_bits(self) -> u64 {
        (u64::from(self.up) << 32) | u64::from(self.down)
    }

    /// Inverse of [`Self::to_bits`] (zero terms read as 1).
    pub fn from_bits(bits: u64) -> Self {
        Self::new((bits >> 32) as u32, bits as u32)
    }

    /// Output frames that exist after `k` input frames: `ceil(k * up / down)`.
    pub fn frames_out_total(self, k: u64) -> u64 {
        let num = u128::from(k) * u128::from(self.up);
        num.div_ceil(u128::from(self.down)) as u64
    }

    /// Output frames a block of `n_in` input frames produces when `pos`
    /// input frames came before it (see the module docs).
    pub fn frames_out(self, pos: u64, n_in: usize) -> usize {
        if self.down == 1 {
            return n_in * self.up as usize;
        }
        let r = pos % u64::from(self.down);
        (self.frames_out_total(r + n_in as u64) - self.frames_out_total(r)) as usize
    }

    /// The fewest input frames after position `pos` whose block produces at
    /// least `n_out` output frames. The block may produce up to
    /// `ceil(up / down) - 1` frames more than asked (when one input frame
    /// completes several output frames).
    pub fn frames_in_for(self, pos: u64, n_out: usize) -> usize {
        if n_out == 0 {
            return 0;
        }
        let (up, down) = (u128::from(self.up), u128::from(self.down));
        let r = u128::from(pos % u64::from(self.down));
        // Need ceil((r + n) up / down) >= c0 + n_out, i.e.
        // (r + n) up > (c0 + n_out - 1) down.
        let c0 = (r * up).div_ceil(down);
        let k = c0 + n_out as u128 - 1;
        let need_total = (k * down) / up + 1;
        need_total.saturating_sub(r) as usize
    }

    /// Most output frames any block of `n_in` input frames can produce:
    /// `ceil(n_in * up / down)`.
    pub fn max_frames_out(self, n_in: usize) -> usize {
        self.frames_out_total(n_in as u64) as usize
    }

    /// Most input frames [`Self::frames_in_for`] can ask for to produce
    /// `n_out` output frames.
    pub fn max_frames_in_for(self, n_out: usize) -> usize {
        if n_out == 0 {
            return 0;
        }
        // Worst case over every phase r: floor((c0 + n_out - 1) down / up) + 1 - r.
        let (up, down) = (u128::from(self.up), u128::from(self.down));
        ((n_out as u128 * down).div_ceil(up) + 1) as usize
    }
}

impl fmt::Display for RateRatio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.down == 1 {
            write!(f, "x{}", self.up)
        } else {
            write!(f, "{}/{}", self.up, self.down)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brute_out(r: RateRatio, pos: u64, n: usize) -> usize {
        // Output m exists once input floor(m down / up) has arrived.
        let covered = |k: u64| {
            (0..)
                .take_while(|&m: &u64| (m * u64::from(r.down)) / u64::from(r.up) < k)
                .count()
        };
        covered(pos + n as u64) - covered(pos)
    }

    #[test]
    fn block_counts_match_the_frame_grid() {
        for (up, down) in [
            (160, 147),
            (147, 160),
            (1, 2),
            (1, 4),
            (3, 2),
            (2, 3),
            (8, 1),
            (320, 147),
        ] {
            let r = RateRatio::new(up, down);
            for pos in 0..(2 * u64::from(r.down)).min(400) {
                for n in [0usize, 1, 2, 3, 7, 64, 147, 160, 511] {
                    let want = brute_out(r, pos, n);
                    assert_eq!(r.frames_out(pos, n), want, "{r} pos {pos} n {n}");
                    assert!(want <= r.max_frames_out(n));
                }
                for n_out in [1usize, 2, 3, 100, 1024] {
                    let n = r.frames_in_for(pos, n_out);
                    assert!(r.frames_out(pos, n) >= n_out, "{r} pos {pos} n_out {n_out}");
                    if n > 0 {
                        assert!(r.frames_out(pos, n - 1) < n_out, "{r} not minimal");
                    }
                    assert!(n <= r.max_frames_in_for(n_out), "{r} pos {pos} {n} > max");
                    assert!(
                        r.frames_out(pos, n) < n_out + (r.up as usize).div_ceil(r.down as usize)
                    );
                }
            }
        }
    }

    #[test]
    fn long_runs_track_the_exact_rate() {
        let r = RateRatio::from_rates(44_100, 48_000);
        let mut pos = 0u64;
        let mut out = 0u64;
        for i in 0..10_000usize {
            let n = 100 + (i * 37) % 900;
            out += r.frames_out(pos, n) as u64;
            pos += n as u64;
        }
        assert_eq!(out, r.frames_out_total(pos));
    }
}
