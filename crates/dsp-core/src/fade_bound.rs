//! How far an upstream edit's fade at the convolution's input can take the
//! output past both settings' ([`crate::chain::Chain::take_over_upstream`]).
//!
//! The fade feeds the convolution `(1 - g) * old + g * new`, `g` a raised
//! cosine over `L` input frames. Each tap `k` of the IR sees the fade `k`
//! frames late: while it passes through the IR, taps before it carry the
//! new signal and taps after it the old. For a steady tone of frequency
//! `f` on input `i`, output row `r` plays
//! `sum_i H_ri(f) a_i - G_ri,n(f) d_i`, `a_i` the old signal at the
//! convolution's input, `d_i = a_i - b_i` the edit's change there, and
//! `G_ri,n(f) = sum_k h_ri[k] g(n - k) e^(-j 2 pi f k)` the IR's spectrum
//! under the fade at frame `n`. For any `c` in `[0, 1]`, `(1 - c)` of the
//! old output plus `c` of the new is no louder than the louder of the two,
//! and the output differs from it by `sum_i (G_ri,n - c H_ri) d_i`. So the
//! output passes both settings' by at most
//! `sum_i |G_ri,n(f) - c H_ri(f)| |d_i(f)|` for the best common `c`:
//! [`FadeBound`] holds the first factor (per row, input, fade length and
//! frequency band), the takeover the second (the EQ's response, old
//! against new, per input and band), and their product summed over the
//! inputs is the bound.
//!
//! What is measured, and how it holds between the measured points:
//! - Fade positions: `n` runs over the whole IR (before it `G = 0`, after
//!   it `G = H`, exactly). `G_n` is smooth in `n`: its second derivative is
//!   at most `pi^2 / 2L^2` times the IR's absolute sum over the taps under
//!   the fade's ramp, so between two measured positions `s` apart it stays
//!   within `s^2 / 8` of that of the line between them (with `c` taken
//!   along the line too). The distance is convex, so between the two it is
//!   at most the larger of theirs plus that; the steps keep it under
//!   [`POSITION_SLACK`], which is added to the bound.
//! - Frequencies: the IR's spectra at the FFT bins (zero-padded at least
//!   twice its length); a band takes the largest value over every bin in
//!   it and the bins just outside, so a tone between bins is bracketed.
//!   [`FADE_SAFETY`] covers the rest of the interpolation.
//! - The EQ change: each chain's response at [`FINE_POINTS`] frequencies
//!   (DC, then log-spaced from 1 Hz to Nyquist), taken per band as the
//!   largest over its points and the two next to it. A band narrower than
//!   about eight of those points (Q over [`MAX_RESOLVED_Q`]) is not
//!   resolved: no takeover ([`eq_resolved`]).
//! - A fade length not measured (an IR whose positions need more than
//!   `MAX_POSITIONS`), an IR longer than [`MAX_TAPS`] or any value that
//!   is not finite means no takeover at that length (none at all for the
//!   last two): the edit warms a new chain up.
//!
//! The bound is for one steady tone at the EQ's input of up to full scale
//! (the worst case over its phase and the edit's); a sum of tones passes by
//! at most the sum of theirs.

use crate::Complex64;
use crate::matrix::ConvIrs;
use audivane_protocol::EqProfile;
use realfft::RealFftPlanner;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The fade lengths the bound is measured at, ms: an upstream edit's
/// shortest fade ([`crate::chain::UPSTREAM_FADE_MIN_MS`]) to the longest
/// the bound may ask for ([`crate::chain::UPSTREAM_FADE_BOUND_MAX_MS`]).
pub const FADE_STEPS_MS: [f64; 7] = [10.0, 20.0, 40.0, 80.0, 160.0, 250.0, 500.0];

/// The most an upstream edit's fade may take the output past both
/// settings' by the bound, as a fraction of full scale (-18 dBFS).
pub const FADE_TRANSIENT_MAX: f64 = 0.125;

/// The bound's margin for what the frequency sampling leaves (a factor).
pub const FADE_SAFETY: f64 = 1.1;

/// What the fade can add between two measured positions, at most (per
/// unit of change; added to the bound).
pub const POSITION_SLACK: f64 = 0.01;

/// The frequencies an EQ's response is kept at: DC, then log-spaced from
/// 1 Hz to just under Nyquist.
pub const FINE_POINTS: usize = 8192;
/// Fine points per band.
const BAND_FINE: usize = 8;
/// The bands the bound is kept in.
pub const BANDS: usize = FINE_POINTS / BAND_FINE;

/// The narrowest EQ section the fine points resolve (about eight points
/// across its bandwidth): a takeover with a narrower one is refused.
pub const MAX_RESOLVED_Q: f64 = 100.0;

/// IRs longer than this (per entry) are not measured: no upstream edit
/// takes such a chain over (2.7 s at 48 kHz; a room calibration is well
/// under a second).
pub const MAX_TAPS: usize = 1 << 17;

/// Fade positions measured per output and fade length at most; a fade
/// that needs more is not used.
const MAX_POSITIONS: usize = 8192;

/// Positions measured per task (the work is spread over threads).
const CHUNK: usize = 48;

/// The smallest FFT the IR's spectra are taken with (fine bins for a
/// short IR).
const MIN_FFT: usize = 1 << 14;

/// [`FINE_POINTS`] frequencies (Hz) for `sample_rate`.
pub fn fine_grid(sample_rate: u32) -> Vec<f64> {
    let top = 0.4999 * f64::from(sample_rate);
    let (lo, hi) = (0.0f64, top.ln());
    std::iter::once(0.0)
        .chain((0..FINE_POINTS - 1).map(|p| {
            let t = p as f64 / (FINE_POINTS - 2) as f64;
            (lo + (hi - lo) * t).exp()
        }))
        .collect()
}

/// Whether the fine points resolve every section of `profile` (each
/// enabled band's Q at most [`MAX_RESOLVED_Q`]).
pub fn eq_resolved(profile: &EqProfile) -> bool {
    let bands = profile
        .bands
        .iter()
        .chain(profile.left.iter().flat_map(|c| &c.bands))
        .chain(profile.right.iter().flat_map(|c| &c.bands));
    bands
        .filter(|b| b.enabled)
        .all(|b| b.q.is_finite() && b.q <= MAX_RESOLVED_Q && b.freq_hz.is_finite())
}

/// The edit's size per input and band: the largest `|old - new|` of the
/// stages' responses (`old`, `new`: per channel, at [`fine_grid`]) over each
/// band's fine points and the two next to it, plus `flat` times the larger
/// response there (a stage changed whose response is not kept). Writes
/// `channels x BANDS` values into `out`. Real-time safe.
pub fn edit_size(old: &[Vec<Complex64>], new: &[Vec<Complex64>], flat: f64, out: &mut [f64]) {
    for ((o, n), dst) in old.iter().zip(new).zip(out.as_chunks_mut::<BANDS>().0) {
        for (b, d) in dst.iter_mut().enumerate() {
            let lo = (b * BAND_FINE).saturating_sub(1);
            let hi = ((b + 1) * BAND_FINE + 1).min(FINE_POINTS);
            let mut m = 0.0f64;
            for p in lo..hi {
                let v = (n[p] - o[p]).norm() + flat * n[p].norm().max(o[p].norm());
                // A NaN (a response that is not finite) refuses below.
                m = if v.is_nan() { f64::NAN } else { m.max(v) };
                if m.is_nan() {
                    break;
                }
            }
            *d = m;
        }
    }
}

/// One output of the convolution: the inputs it sums and, per input and
/// measured fade length, the bound per band per unit of that input's
/// change (`None`: that length not measured for this output).
#[derive(Debug, Clone)]
struct Row {
    inputs: Vec<usize>,
    /// `[step]` -> `[input][band]`, flattened.
    steps: Vec<Option<Vec<f32>>>,
}

/// The measured bound of a convolution's IRs at one sample rate.
#[derive(Debug, Clone)]
pub struct FadeBound {
    /// The fade lengths measured, input frames ([`FADE_STEPS_MS`]).
    steps: Vec<usize>,
    rows: Vec<Row>,
}

impl FadeBound {
    /// The bound of `irs` on `channels` channels at `sample_rate` (the
    /// convolution's rate), measured or from the cache; `None` when an IR
    /// is longer than [`MAX_TAPS`] or not finite. Not real-time safe (FFTs,
    /// on as many threads as the machine has).
    pub fn of(irs: &ConvIrs, channels: usize, sample_rate: u32) -> Option<Arc<FadeBound>> {
        let rows: Vec<Vec<(usize, &[f64])>> = match irs {
            ConvIrs::Channels(v) if v.len() == 1 => {
                (0..channels).map(|c| vec![(c, &v[0][..])]).collect()
            }
            ConvIrs::Channels(v) => v
                .iter()
                .enumerate()
                .map(|(c, ir)| vec![(c, &ir[..])])
                .collect(),
            ConvIrs::Matrix(m) => {
                let mut rows = vec![Vec::new(); m.channels()];
                for (out, input, ir) in m.entries() {
                    rows[out].push((input, ir));
                }
                rows
            }
        };
        let entries = || rows.iter().flatten();
        if entries().any(|(i, ir)| {
            *i >= channels || ir.len() > MAX_TAPS || ir.iter().any(|x| !x.is_finite())
        }) {
            return None;
        }
        let key = cache_key(&rows, sample_rate);
        if let Some(b) = cache_get(key) {
            return Some(b);
        }
        let steps: Vec<usize> = FADE_STEPS_MS
            .iter()
            .map(|ms| ((f64::from(sample_rate) * ms / 1000.0).round() as usize).max(1))
            .collect();
        let fine = fine_grid(sample_rate);
        let mut planner = RealFftPlanner::<f64>::new();
        let prep: Vec<RowPrep> = rows
            .iter()
            .map(|row| RowPrep::new(row, sample_rate, &fine, &mut planner))
            .collect();
        // The positions of each (output, fade length), in chunks.
        let plans: Vec<Option<Vec<i64>>> = prep
            .iter()
            .flat_map(|p| steps.iter().map(move |&len| p.positions(len)))
            .collect();
        let mut tasks = Vec::new();
        for (t, plan) in plans.iter().enumerate() {
            if let Some(pos) = plan {
                for c in 0..pos.len().div_ceil(CHUNK) {
                    tasks.push((t, c));
                }
            }
        }
        let width = |t: usize| prep[t / steps.len()].row.len() * BANDS;
        let results: Vec<Mutex<Vec<f32>>> = (0..plans.len())
            .map(|t| Mutex::new(vec![0.0; width(t)]))
            .collect();
        let next = AtomicUsize::new(0);
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(tasks.len())
            .max(1);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    let mut planner = RealFftPlanner::<f64>::new();
                    loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(t, c)) = tasks.get(k) else { break };
                        let (r, st) = (t / steps.len(), t % steps.len());
                        let pos = plans[t].as_ref().expect("planned");
                        let chunk = &pos[c * CHUNK..((c + 1) * CHUNK).min(pos.len())];
                        let q = prep[r].measure(steps[st], chunk, &mut planner);
                        let mut acc = results[t].lock().unwrap();
                        for (a, v) in acc.iter_mut().zip(q) {
                            // A NaN stays (`max` would drop it).
                            *a = if v.is_nan() || a.is_nan() {
                                f32::NAN
                            } else {
                                a.max(v)
                            };
                        }
                    }
                });
            }
        });
        let mut results = results.into_iter().map(|m| m.into_inner().unwrap());
        let mut out = FadeBound {
            steps: steps.clone(),
            rows: Vec::with_capacity(prep.len()),
        };
        for (r, p) in prep.iter().enumerate() {
            let mut row = Row {
                inputs: p.row.iter().map(|(i, _)| *i).collect(),
                steps: Vec::with_capacity(steps.len()),
            };
            for st in 0..steps.len() {
                let q = results.next().expect("one per task");
                // Measured, and every value finite (an IR whose spectra
                // overflow is not known): else not used.
                let measured =
                    plans[r * steps.len() + st].is_some() && q.iter().all(|v| v.is_finite());
                row.steps.push(
                    measured.then(|| q.into_iter().map(|v| v + POSITION_SLACK as f32).collect()),
                );
            }
            out.rows.push(row);
        }
        let out = Arc::new(out);
        cache_put(key, &out);
        Some(out)
    }

    /// The fade lengths measured, input frames.
    pub fn steps(&self) -> &[usize] {
        &self.steps
    }

    /// The bound at fade step `step` for an edit of size `edit`
    /// (`channels x BANDS`, [`edit_size`]): the most the output can pass
    /// both settings' by, as a fraction of full scale ([`FADE_SAFETY`]
    /// included). Infinite when the step is not measured for some output
    /// or a value is not finite. Real-time safe.
    pub fn transient(&self, step: usize, edit: &[f64]) -> f64 {
        let mut worst = 0.0f64;
        for row in &self.rows {
            let Some(q) = row.steps.get(step).and_then(Option::as_ref) else {
                return f64::INFINITY;
            };
            for b in 0..BANDS {
                let mut t = 0.0f64;
                for (e, &i) in row.inputs.iter().enumerate() {
                    let d = edit.get(i * BANDS + b).copied().unwrap_or(f64::NAN);
                    t += f64::from(q[e * BANDS + b]) * d;
                }
                if !t.is_finite() {
                    return f64::INFINITY;
                }
                worst = worst.max(t);
            }
        }
        worst * FADE_SAFETY
    }

    /// The shortest measured fade (input frames) whose bound for `edit`
    /// ([`Self::transient`]) stays within [`FADE_TRANSIENT_MAX`], with the
    /// bound; `None` when none does. Real-time safe (no allocation).
    pub fn fade_len(&self, edit: &[f64]) -> Option<(usize, f64)> {
        (0..self.steps.len()).find_map(|s| {
            let t = self.transient(s, edit);
            (t <= FADE_TRANSIENT_MAX).then_some((self.steps[s], t))
        })
    }
}

/// The raised-cosine fade weight `len` frames long at frame `m` of it.
fn weight(m: i64, len: usize) -> f64 {
    if m <= 0 {
        0.0
    } else if m as usize >= len {
        1.0
    } else {
        0.5 - 0.5 * (std::f64::consts::PI * m as f64 / len as f64).cos()
    }
}

/// One output's IRs, ready to measure.
struct RowPrep<'a> {
    row: &'a [(usize, &'a [f64])],
    taps: usize,
    nf: usize,
    /// Each input's full spectrum at the FFT bins.
    full: Vec<Vec<Complex64>>,
    /// The dominant input (most energy): its projection gives the common
    /// `c`.
    dom: usize,
    /// Prefix sums of the row's absolute taps (all inputs).
    abs: Vec<f64>,
    /// Each band's FFT bins, `[first, last]`, the bins just outside it
    /// included.
    bins: Vec<(usize, usize)>,
}

impl<'a> RowPrep<'a> {
    fn new(
        row: &'a [(usize, &'a [f64])],
        sample_rate: u32,
        fine: &[f64],
        planner: &mut RealFftPlanner<f64>,
    ) -> Self {
        let taps = row.iter().map(|(_, ir)| ir.len()).max().unwrap_or(0);
        let nf = (2 * taps).next_power_of_two().max(MIN_FFT);
        let fft = planner.plan_fft_forward(nf);
        let mut abs = vec![0.0; taps + 1];
        let mut dom = (0, -1.0f64);
        let mut full = Vec::with_capacity(row.len());
        for (e, (_, ir)) in row.iter().enumerate() {
            let energy: f64 = ir.iter().map(|h| h * h).sum();
            if energy > dom.1 {
                dom = (e, energy);
            }
            for (k, h) in ir.iter().enumerate() {
                abs[k + 1] += h.abs();
            }
            let mut buf = fft.make_input_vec();
            buf[..ir.len()].copy_from_slice(ir);
            let mut out = fft.make_output_vec();
            fft.process(&mut buf, &mut out).expect("fft sizes");
            full.push(out);
        }
        for k in 0..taps {
            abs[k + 1] += abs[k];
        }
        let bin_hz = f64::from(sample_rate) / nf as f64;
        let last = nf / 2;
        let bins = (0..BANDS)
            .map(|b| {
                let lo = fine[(b * BAND_FINE).saturating_sub(1)];
                let hi = fine[((b + 1) * BAND_FINE).min(FINE_POINTS - 1)];
                let first = ((lo / bin_hz).floor() as usize).min(last);
                let end = ((hi / bin_hz).ceil() as usize).min(last);
                (first, end.max(first))
            })
            .collect();
        RowPrep {
            row,
            taps,
            nf,
            full,
            dom: dom.0,
            abs,
            bins,
        }
    }

    /// The absolute sum of the row's taps in `[a, b]` (clamped).
    fn abs_sum(&self, a: i64, b: i64) -> f64 {
        let a = a.clamp(0, self.taps as i64) as usize;
        let b = (b + 1).clamp(0, self.taps as i64) as usize;
        if b > a {
            self.abs[b] - self.abs[a]
        } else {
            0.0
        }
    }

    /// The fade positions to measure at fade length `len`: from the fade
    /// front at the first tap to past the last, each step short enough
    /// that `G` strays from the line between two by at most
    /// [`POSITION_SLACK`] (or one frame). `None` past `MAX_POSITIONS`.
    fn positions(&self, len: usize) -> Option<Vec<i64>> {
        let end = (self.taps + len) as i64;
        let mut out = vec![0i64];
        let mut n = 0i64;
        let l = len as f64;
        // How far `G` strays from the line between `n` and `n + s`:
        // `s^2 / 8` times its largest second derivative there, the taps
        // under the ramp at some point between (`|g''| <= pi^2 / 2L^2`).
        let slack = |n: i64, s: i64| {
            let w = self.abs_sum(n - len as i64, n + s);
            let pi2 = std::f64::consts::PI * std::f64::consts::PI;
            (s * s) as f64 / 8.0 * pi2 / (2.0 * l * l) * w
        };
        while n < end {
            let mut s = 1i64;
            while n + 2 * s <= end && slack(n, 2 * s) <= POSITION_SLACK {
                s *= 2;
            }
            n = (n + s).min(end);
            out.push(n);
            if out.len() > MAX_POSITIONS {
                return None;
            }
        }
        Some(out)
    }

    /// The bound per input and band over `positions` at fade length `len`:
    /// the largest `|G_n - c H|` over the bins of each band.
    fn measure(
        &self,
        len: usize,
        positions: &[i64],
        planner: &mut RealFftPlanner<f64>,
    ) -> Vec<f32> {
        // An output no input reaches: nothing to pass.
        if self.row.is_empty() {
            return Vec::new();
        }
        let fft = planner.plan_fft_forward(self.nf);
        let bins = self.nf / 2 + 1;
        let mut buf = fft.make_input_vec();
        let mut scratch = fft.make_scratch_vec();
        let mut g: Vec<Vec<Complex64>> =
            (0..self.row.len()).map(|_| fft.make_output_vec()).collect();
        let mut dist = vec![vec![0.0f64; bins]; self.row.len()];
        for &n in positions {
            for (e, (_, ir)) in self.row.iter().enumerate() {
                buf.fill(0.0);
                for (k, (&h, b)) in ir.iter().zip(buf.iter_mut()).enumerate() {
                    *b = h * weight(n - k as i64, len);
                }
                fft.process_with_scratch(&mut buf, &mut g[e], &mut scratch)
                    .expect("fft sizes");
            }
            let (gd, hd) = (&g[self.dom], &self.full[self.dom]);
            for b in 0..bins {
                let h2 = hd[b].norm_sqr();
                let c = if h2 > 0.0 {
                    ((gd[b] * hd[b].conj()).re / h2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                for e in 0..self.row.len() {
                    let d = (g[e][b] - self.full[e][b] * c).norm();
                    // A NaN stays (`max` would drop it).
                    if d > dist[e][b] || d.is_nan() {
                        dist[e][b] = d;
                    }
                }
            }
        }
        let mut out = vec![0.0f32; self.row.len() * BANDS];
        for (e, d) in dist.iter().enumerate() {
            for (b, &(first, last)) in self.bins.iter().enumerate() {
                let m = d[first..=last]
                    .iter()
                    .fold(0.0f64, |m, &v| if v.is_nan() { f64::NAN } else { m.max(v) });
                out[e * BANDS + b] = if m.is_finite() { m as f32 } else { f32::NAN };
            }
        }
        out
    }
}

/// The cache key of a measured bound: the IRs' bits, layout and rate.
fn cache_key(rows: &[Vec<(usize, &[f64])>], sample_rate: u32) -> u64 {
    let mut h = 0x9E37_79B9_7F4A_7C15u64 ^ u64::from(sample_rate);
    let mut mix = |v: u64| {
        h ^= v;
        h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h ^= h >> 31;
    };
    for row in rows {
        mix(row.len() as u64);
        for &(i, ir) in row {
            mix(i as u64);
            mix(ir.len() as u64);
            for &x in ir {
                mix(x.to_bits());
            }
        }
    }
    h
}

/// The bounds measured last (a chain is rebuilt on every edit; its IRs
/// rarely change).
static CACHE: Mutex<Vec<(u64, Arc<FadeBound>)>> = Mutex::new(Vec::new());
const CACHE_LEN: usize = 4;

fn cache_get(key: u64) -> Option<Arc<FadeBound>> {
    let mut c = CACHE.lock().ok()?;
    let at = c.iter().position(|(k, _)| *k == key)?;
    let hit = c.remove(at);
    let b = hit.1.clone();
    c.push(hit);
    Some(b)
}

fn cache_put(key: u64, b: &Arc<FadeBound>) {
    if let Ok(mut c) = CACHE.lock() {
        c.retain(|(k, _)| *k != key);
        if c.len() >= CACHE_LEN {
            c.remove(0);
        }
        c.push((key, b.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(channels: usize, v: f64) -> Vec<f64> {
        vec![v; channels * BANDS]
    }

    /// Two equal taps of opposite sign 20 ms apart (a 1 kHz null): a fade
    /// shorter than their spacing passes the null by about a tap's height
    /// per unit of change; a long one by about `pi d / 2 L` of it.
    #[test]
    fn far_cancelling_taps_have_a_large_bound() {
        let mut ir = vec![0.0; 2_000];
        ir[0] = 1.9;
        ir[960] = -1.9;
        let b = FadeBound::of(&ConvIrs::Channels(vec![ir]), 2, 48_000).unwrap();
        let unit = flat(2, 1.0);
        let short = b.transient(0, &unit) / FADE_SAFETY;
        let long = b.transient(b.steps().len() - 1, &unit) / FADE_SAFETY;
        assert!((short - 1.9).abs() < 0.1, "10 ms: {short}");
        let last = *b.steps().last().unwrap() as f64;
        let expect = 1.9 * std::f64::consts::PI * 960.0 / (2.0 * last);
        assert!(
            long >= expect && long < 1.3 * expect + POSITION_SLACK,
            "{last} frames: {long}, about {expect}"
        );
        // A -12 dB step: only the longest fade keeps the bound; a mute
        // (the whole signal changed): none.
        let step = b.fade_len(&flat(2, 1.0 - 10f64.powf(-12.0 / 20.0)));
        assert_eq!(step.map(|s| s.0), Some(24_000));
        assert!(step.unwrap().1 <= FADE_TRANSIENT_MAX);
        assert_eq!(b.fade_len(&flat(2, 1.0)), None);
    }

    /// A delay: the fade only moves later, nothing to pass but the
    /// positions' slack.
    #[test]
    fn a_delay_has_only_the_slack() {
        let mut ir = vec![0.0; 4_096];
        ir[1_440] = 0.8;
        let b = FadeBound::of(&ConvIrs::Channels(vec![ir]), 2, 48_000).unwrap();
        let t = b.transient(0, &flat(2, 1.0));
        assert!(t <= POSITION_SLACK * FADE_SAFETY + 1e-6, "{t}");
        assert_eq!(b.fade_len(&flat(2, 1.0)).map(|f| f.0), Some(480));
    }

    /// Taps so large their spectra overflow (`1e155`: the projection is
    /// NaN): nothing measured, every edit refused (never a NaN dropped by
    /// a `max` into a tiny bound).
    #[test]
    fn an_overflowing_ir_refuses() {
        let b = FadeBound::of(&ConvIrs::Channels(vec![vec![1e155]]), 2, 48_000).unwrap();
        let minus_1_db = flat(2, 1.0 - 10f64.powf(-1.0 / 20.0));
        for s in 0..b.steps().len() {
            assert_eq!(b.transient(s, &minus_1_db), f64::INFINITY, "step {s}");
        }
        assert_eq!(b.fade_len(&minus_1_db), None);
    }

    /// A matrix output no input reaches: measured as nothing to pass.
    #[test]
    fn an_output_without_entries_passes_nothing() {
        let mut m = crate::matrix::FilterMatrix::new(2);
        let mut ir = vec![0.0; 1_000];
        ir[10] = 1.0;
        m.set(0, 0, ir);
        let b = FadeBound::of(&ConvIrs::Matrix(m), 2, 48_000).unwrap();
        assert!(b.transient(0, &flat(2, 1.0)) <= POSITION_SLACK * FADE_SAFETY + 1e-6);
    }

    /// An unchanged input whose fade was not measured, or a NaN in the
    /// edit: refused, never a NaN dropped into an accept.
    #[test]
    fn what_is_not_known_refuses() {
        let mut ir = vec![0.0; 4_096];
        ir[0] = 1.0;
        let b = FadeBound::of(&ConvIrs::Channels(vec![ir]), 2, 48_000).unwrap();
        let mut edit = flat(2, 0.0);
        edit[3] = f64::NAN;
        assert_eq!(b.transient(0, &edit), f64::INFINITY);
        assert_eq!(b.fade_len(&edit), None);
        let mut b = (*b).clone();
        b.rows[1].steps[0] = None;
        assert_eq!(b.transient(0, &flat(2, 0.0)), f64::INFINITY);
    }
}
