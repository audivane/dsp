//! The audio-thread half of measured A/B level matching.
//!
//! While a measurement runs, an [`crate::chain::AbChain`] feeds both sides'
//! time-aligned outputs (before the level gains) through BS.1770
//! K-weighting (the shelf and high pass of [`crate::loudness::LufsMeter`],
//! two biquads per channel per side) and emits one [`AbHop`] every 100 ms:
//! each side's K-weighted mean square over the hop, summed over channels.
//! The control plane builds the 400 ms gating blocks from four hops, gates
//! on A's blocks and decides when to lock (`daemon::control::ab_measure`).
//!
//! A measurement is named by an epoch (non-zero): a new epoch restarts the
//! filters and the partial hop, so hops of different measurements never
//! mix, and epoch 0 stops metering (nothing runs). Completed hops wait in a
//! small fixed ring until the caller drains them; `push`, `pop`, `reset`
//! and `set_epoch` never allocate.

use crate::loudness::{biquad_step, k_high_pass, k_shelf};

/// Length of one hop: a quarter of a BS.1770 gating block.
pub const HOP_SECONDS: f64 = 0.1;
/// Hops a meter keeps until drained (6.4 s): the audio thread drains it
/// after every block. When full, the oldest hop is dropped.
pub const HOP_RING: usize = 64;

/// One 100 ms hop of a measurement.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AbHop {
    /// The measurement it belongs to (non-zero).
    pub epoch: u32,
    /// A's K-weighted mean square over the hop, summed over channels.
    pub a: f64,
    /// B's, over the same frames.
    pub b: f64,
}

/// Per-side K-weighted hop energies (see the module docs).
pub struct AbHopMeter {
    shelf: [f64; 5],
    high_pass: [f64; 5],
    /// Per channel and side (A, B): `[shelf, high pass]` biquad states.
    state: Vec<[[[f64; 2]; 2]; 2]>,
    hop_frames: usize,
    count: usize,
    sum: [f64; 2],
    epoch: u32,
    ring: [AbHop; HOP_RING],
    head: usize,
    len: usize,
}

impl AbHopMeter {
    /// A meter for `channels` channels at `sample_rate` (the pair's output
    /// rate), stopped (epoch 0).
    pub fn new(sample_rate: f64, channels: usize) -> Self {
        let fs = if sample_rate.is_finite() && sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        Self {
            shelf: k_shelf(fs),
            high_pass: k_high_pass(fs),
            state: vec![[[[0.0; 2]; 2]; 2]; channels],
            hop_frames: ((HOP_SECONDS * fs).round() as usize).max(1),
            count: 0,
            sum: [0.0; 2],
            epoch: 0,
            ring: [AbHop::default(); HOP_RING],
            head: 0,
            len: 0,
        }
    }

    /// The running measurement, 0 when stopped.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Start measuring for `epoch` (0 stops). A change restarts the
    /// filters, the partial hop and the ring; the same epoch again is a
    /// no-op. Allocation-free.
    pub fn set_epoch(&mut self, epoch: u32) {
        if epoch != self.epoch {
            self.epoch = epoch;
            self.reset();
            self.head = 0;
            self.len = 0;
        }
    }

    /// Meter `frames` frames of both sides (planar, time-aligned). Does
    /// nothing while stopped. Allocation-free.
    pub fn push(&mut self, a: &[Vec<f64>], b: &[Vec<f64>], frames: usize) {
        if self.epoch == 0 {
            return;
        }
        let channels = self.state.len().min(a.len()).min(b.len());
        let mut done = 0;
        while done < frames {
            let take = (self.hop_frames - self.count).min(frames - done);
            for (side, chans) in [a, b].into_iter().enumerate() {
                let mut sum = 0.0;
                let mut finite = true;
                for (chan, state) in chans.iter().zip(self.state.iter_mut()).take(channels) {
                    let st = &mut state[side];
                    for &x in &chan[done..done + take] {
                        let y = biquad_step(&self.shelf, &mut st[0], x);
                        let y = biquad_step(&self.high_pass, &mut st[1], y);
                        sum += y * y;
                    }
                    finite &= st.iter().flatten().all(|v| v.is_finite());
                }
                if finite && sum.is_finite() {
                    self.sum[side] += sum;
                } else {
                    // A NaN or infinity would stay in the filters for good:
                    // this side starts again from rest, and the hop is
                    // marked invalid (NaN, which the control plane drops).
                    for state in &mut self.state {
                        state[side] = [[0.0; 2]; 2];
                    }
                    self.sum[side] = f64::NAN;
                }
            }
            self.count += take;
            done += take;
            if self.count == self.hop_frames {
                let n = self.hop_frames as f64;
                self.emit(AbHop {
                    epoch: self.epoch,
                    a: self.sum[0] / n,
                    b: self.sum[1] / n,
                });
                self.count = 0;
                self.sum = [0.0; 2];
            }
        }
    }

    fn emit(&mut self, hop: AbHop) {
        let tail = (self.head + self.len) % HOP_RING;
        self.ring[tail] = hop;
        if self.len == HOP_RING {
            self.head = (self.head + 1) % HOP_RING;
        } else {
            self.len += 1;
        }
    }

    /// The oldest completed hop not yet taken.
    pub fn pop(&mut self) -> Option<AbHop> {
        if self.len == 0 {
            return None;
        }
        let hop = self.ring[self.head];
        self.head = (self.head + 1) % HOP_RING;
        self.len -= 1;
        Some(hop)
    }

    /// Clear the filters and the partial hop (a discontinuity in the
    /// stream); completed hops and the epoch stay. Allocation-free.
    pub fn reset(&mut self) {
        self.state.fill([[[0.0; 2]; 2]; 2]);
        self.count = 0;
        self.sum = [0.0; 2];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loudness::LufsMeter;
    use std::f64::consts::PI;

    fn sine(fs: f64, f: f64, amp: f64, n: usize) -> Vec<f64> {
        (0..n)
            .map(|i| amp * (2.0 * PI * f * i as f64 / fs).sin())
            .collect()
    }

    #[test]
    fn stopped_meter_emits_nothing() {
        let mut m = AbHopMeter::new(48_000.0, 2);
        let x = vec![vec![0.5; 48_000]; 2];
        m.push(&x, &x, 48_000);
        assert_eq!(m.pop(), None);
    }

    #[test]
    fn hops_match_the_lufs_meter_and_any_block_size() {
        let fs = 48_000.0;
        let n = 48_000 * 4;
        let a = vec![sine(fs, 1000.0, 0.5, n), sine(fs, 997.0, 0.25, n)];
        let b = vec![sine(fs, 100.0, 0.1, n), sine(fs, 5000.0, 0.3, n)];
        let run = |sizes: &[usize]| {
            let mut m = AbHopMeter::new(fs, 2);
            m.set_epoch(7);
            let mut hops = Vec::new();
            let mut at = 0;
            let mut k = 0;
            while at < n {
                let take = sizes[k % sizes.len()].min(n - at);
                let sa: Vec<Vec<f64>> = a.iter().map(|c| c[at..at + take].to_vec()).collect();
                let sb: Vec<Vec<f64>> = b.iter().map(|c| c[at..at + take].to_vec()).collect();
                m.push(&sa, &sb, take);
                while let Some(h) = m.pop() {
                    hops.push(h);
                }
                at += take;
                k += 1;
            }
            hops
        };
        let whole = run(&[n]);
        assert_eq!(whole.len(), 40);
        assert!(whole.iter().all(|h| h.epoch == 7));
        for sizes in [&[1usize][..], &[7], &[1024], &[3, 500, 4801, 64]] {
            let got = run(sizes);
            assert_eq!(got.len(), whole.len());
            for (g, w) in got.iter().zip(&whole) {
                assert!((g.a - w.a).abs() <= 1e-12 * w.a, "{sizes:?}");
                assert!((g.b - w.b).abs() <= 1e-12 * w.b, "{sizes:?}");
            }
        }
        // Integrated over the whole signal (no gate acts on steady sines)
        // the hops give the LUFS meter's loudness.
        for (side, chans) in [(0, &a), (1, &b)] {
            let mut meter = LufsMeter::new(fs, 2);
            let refs: Vec<&[f64]> = chans.iter().map(Vec::as_slice).collect();
            meter.push(&refs);
            // The meter's blocks start at hop 4; ours average the same hops.
            let e: Vec<f64> = whole.iter().map(|h| [h.a, h.b][side]).collect();
            let blocks: Vec<f64> = (3..e.len())
                .map(|i| e[i - 3..=i].iter().sum::<f64>() / 4.0)
                .collect();
            let mean = blocks.iter().sum::<f64>() / blocks.len() as f64;
            let lufs = -0.691 + 10.0 * mean.log10();
            assert!(
                (lufs - meter.integrated_lufs()).abs() < 1e-9,
                "{lufs} vs {}",
                meter.integrated_lufs()
            );
        }
    }

    #[test]
    fn a_nan_invalidates_its_hop_and_clean_audio_counts_again() {
        let fs = 1000.0;
        let mut m = AbHopMeter::new(fs, 2);
        m.set_epoch(1);
        let clean = vec![sine(fs, 50.0, 0.5, 100); 2];
        let mut bad = clean.clone();
        bad[1][37] = f64::NAN;
        let mut hops = Vec::new();
        for b in [&clean, &bad, &clean, &clean] {
            m.push(&clean, b, 100);
            hops.push(m.pop().unwrap());
        }
        assert!(hops[0].a.is_finite() && hops[0].b.is_finite());
        assert!(hops[1].b.is_nan(), "the hop with the NaN is invalid");
        assert!(hops[1].a.is_finite(), "the other side is untouched");
        for h in &hops[2..] {
            assert!(h.a.is_finite() && h.b.is_finite(), "{h:?}");
            assert!(h.b > 0.0);
        }
        // An infinity likewise.
        let mut inf = clean.clone();
        inf[0][3] = f64::INFINITY;
        m.push(&inf, &clean, 100);
        assert!(m.pop().unwrap().a.is_nan());
        m.push(&clean, &clean, 100);
        assert!(m.pop().unwrap().a.is_finite());
    }

    #[test]
    fn a_new_epoch_restarts_and_the_ring_drops_the_oldest() {
        let fs = 1000.0;
        let mut m = AbHopMeter::new(fs, 1);
        m.set_epoch(1);
        let x = vec![vec![0.5; 100 * (HOP_RING + 3)]];
        m.push(&x, &x, x[0].len());
        let mut count = 0;
        while m.pop().is_some() {
            count += 1;
        }
        assert_eq!(count, HOP_RING);
        m.push(&x, &x, 150);
        m.set_epoch(2);
        assert_eq!(m.pop(), None, "a new epoch clears the ring");
        m.push(&x, &x, 50);
        m.push(&x, &x, 50);
        assert_eq!(m.pop().map(|h| h.epoch), Some(2));
        assert_eq!(m.pop(), None, "the partial hop of epoch 1 was dropped");
    }
}
