//! The harmonics stage as a static curve, for the live view's split of the
//! added content by order ([`LiveCurve`]).
//!
//! On music a "harmonic order" is not measurable as such: order `k` of the
//! curve makes `k`-th harmonics of every note and the intermodulation
//! between them. What is exact is the curve's own decomposition:
//!
//! - **Orders** (a parallel stage of static Chebyshev layers: no sag or
//!   bias, no tilt, no flux or tape layer): the stage is
//!   `y = L x + Σ_k a_k D_k(x)` with `D_k` the Chebyshev term minus its
//!   value and slope at 0, so each order's products on the actual music are
//!   `a_k D_k(x)`, evaluated from the tapped input.
//! - **Even / odd** (a series chain of such layers): the chain is still a
//!   curve `f`, so its even part `(f(x) + f(−x)) / 2` and odd part
//!   `(f(x) − f(−x)) / 2` are well defined, but cross terms between layers
//!   mix the orders. An estimate (see the DC blockers below).
//! - Otherwise (flux, tape, sag/bias, tilt: layers with memory) neither
//!   split can be evaluated without re-running the stage; [`live_curve`]
//!   says why, and the view shows the total only.
//!
//! **DC blockers** (5 Hz, on the even terms) have memory: their state
//! holds the DC of the music before the window, and after a loud bass
//! passage it decays through the window. Each even part runs through the
//! same filter, started in its steady state for the window repeated. For
//! a parallel stage [`LiveCurve::eval_with`] then takes the stage's own
//! state (read at a block's end on the audio thread): a single even order
//! carries exactly what the stage's blocker carries; several share that
//! one blocker, so the state goes to their total and each line is an
//! estimate while the transient lasts ([`LiveFit`]). A series chain has no
//! such seed (the `f(−x)` half never ran), so right after a sudden level
//! change in deep bass its even/odd split can be off.
//!
//! Chebyshev terms are not harmonic numbers below full scale (`T4` is
//! mostly a 2nd harmonic at low level), so two even terms can cancel each
//! other; [`LiveFit::agrees`] says when a line would read above the even
//! total.
//!
//! The curve acts on each channel: a split of several channels' mean
//! output evaluates it per channel and averages the parts.

use super::{Harmonics, LEN, T_AT_0, T_SLOPE_0};
use audivane_protocol::{HarmonicsConfig, HarmonicsLayer};

/// Working buffers [`LiveCurve::eval_with`] reuses across calls.
#[derive(Clone, Debug, Default)]
pub struct LiveScratch {
    e: Vec<f64>,
    rest: Vec<f64>,
}

impl LiveScratch {
    /// After an orders split: the even orders' total, with the stage's
    /// own DC blocker state (exact even when the lines are estimates).
    pub fn even_total(&self) -> &[f64] {
        &self.e
    }
}

/// How [`LiveCurve::eval`] splits the stage's products.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveSplit {
    /// One part per order ([`LiveCurve::orders`]).
    Orders,
    /// Two parts: even, then odd.
    EvenOdd,
}

/// One static layer: `linear · x_c + Σ coef_k D_k(x_c) + over`.
#[derive(Clone, Debug)]
struct CurveStage {
    linear: f64,
    coef: [f64; LEN],
    top: usize,
    /// The even terms' DC blocker (`None`: off): `(b0, a1)` of
    /// `H(z) = b0 (1 − z⁻¹) / (1 + a1 z⁻¹)`.
    dc: Option<(f64, f64)>,
}

impl CurveStage {
    /// `e` through the stage's DC blocker, started in its steady state for
    /// `e` repeated (as a blocker that has run a while on music like the
    /// window sits): the state that one pass maps to itself.
    /// Returns the filter's state just before sample `at` (0 without a
    /// blocker).
    fn dc_block(&self, e: &mut [f64], at: usize) -> f64 {
        let Some((b0, a1)) = self.dc else {
            return 0.0;
        };
        if e.is_empty() {
            return 0.0;
        }
        // TDF-II: y = b0 e + z; z' = −b0 e − a1 y. One pass is affine in the
        // start state: z_end = (−a1)^n z0 + B, so the fixed point is
        // B / (1 − (−a1)^n).
        let pass = |z0: f64, e: &[f64]| {
            let mut z = z0;
            for &v in e {
                let y = b0 * v + z;
                z = -b0 * v - a1 * y;
            }
            z
        };
        let b = pass(0.0, e);
        let a = (-a1).powi(e.len().min(i32::MAX as usize) as i32);
        let mut z = if a < 1.0 { b / (1.0 - a) } else { 0.0 };
        let mut z_at = z;
        for (i, v) in e.iter_mut().enumerate() {
            if i == at {
                z_at = z;
            }
            let y = b0 * *v + z;
            z = -b0 * *v - a1 * y;
            *v = y;
        }
        if at >= e.len() {
            z_at = z;
        }
        z_at
    }

    /// Add to `e` (the blocker's output) the response to its state at
    /// sample `at` moving by `delta`: `delta · (−a1)^(i − at)` at every
    /// sample `i` (the filter with no input; before `at`, run backward).
    fn dc_shift(&self, e: &mut [f64], at: usize, delta: f64) {
        let Some((_, a1)) = self.dc else {
            return;
        };
        let r = -a1;
        if delta == 0.0 || r <= 0.0 {
            return;
        }
        let mut v = delta;
        for y in e.iter_mut().skip(at) {
            *y += v;
            v *= r;
        }
        let mut v = delta;
        let head = at.min(e.len());
        for y in e[..head].iter_mut().rev() {
            v /= r;
            *y += v;
        }
    }
}

impl CurveStage {
    /// The even and odd sums of the terms and the rest (`linear · x_c` plus
    /// what passes beyond full scale) for one sample.
    #[inline]
    fn parts(&self, raw: f64) -> (f64, f64, f64) {
        let x = if raw.is_finite() { raw } else { 0.0 };
        let xc = x.clamp(-1.0, 1.0);
        let rest = self.linear * xc + (x - xc);
        let two_x = 2.0 * xc;
        let (mut t_prev, mut t) = (1.0, xc);
        let mut acc = [0.0; 2];
        for k in 2..=self.top {
            let t_next = two_x * t - t_prev;
            t_prev = t;
            t = t_next;
            acc[k & 1] += self.coef[k] * (t - T_SLOPE_0[k] * xc - T_AT_0[k]);
        }
        (acc[0], acc[1], rest)
    }
}

/// The stage as a memoryless curve, built off the audio thread from the
/// config chain A runs ([`live_curve`]).
#[derive(Clone, Debug)]
pub struct LiveCurve {
    split: LiveSplit,
    stages: Vec<CurveStage>,
    /// With [`LiveSplit::Orders`]: the orders with a term.
    orders: Vec<usize>,
    linear_gain: f64,
}

impl LiveCurve {
    pub fn split(&self) -> LiveSplit {
        self.split
    }

    /// The orders [`Self::eval`] returns one part each for (empty with
    /// [`LiveSplit::EvenOdd`]).
    pub fn orders(&self) -> &[usize] {
        &self.orders
    }

    /// The stage's [`Harmonics::linear_gain`] (to match the curve with the
    /// running stage).
    pub fn linear_gain(&self) -> f64 {
        self.linear_gain
    }

    /// How many parts [`Self::eval`] writes.
    pub fn parts(&self) -> usize {
        match self.split {
            LiveSplit::Orders => self.orders.len(),
            LiveSplit::EvenOdd => 2,
        }
    }

    /// Each part of the stage's output for the input `x` into `out` (resized
    /// to [`Self::parts`] buffers of `x.len()`): one per order, or the even
    /// and the odd part. The parts still hold their DC (the stage's
    /// blockers remove it) and, for odd orders and the odd part, whatever of
    /// them correlates with `x` (the analysis removes both).
    pub fn eval(&self, x: &[f64], out: &mut Vec<Vec<f64>>) {
        let _ = self.eval_with(x, out, &mut LiveScratch::default(), None);
    }

    /// [`Self::eval`] with reused working buffers and, for a parallel
    /// stage, the real DC blocker's state `seed = (i, z)` just before
    /// sample `i` of `x` (the stage's own state, read at a block's end), so
    /// the even orders carry the music before the window as the stage's
    /// blocker does instead of the window looped: exactly for one even
    /// order, on their total for several. A
    /// series chain ignores the seed. The [`LiveFit`] says how far the
    /// parts hold.
    pub fn eval_with(
        &self,
        x: &[f64],
        out: &mut Vec<Vec<f64>>,
        scratch: &mut LiveScratch,
        seed: Option<(usize, f64)>,
    ) -> LiveFit {
        let n = x.len();
        out.resize_with(self.parts(), Vec::new);
        for p in out.iter_mut() {
            p.clear();
            p.resize(n, 0.0);
        }
        match self.split {
            LiveSplit::Orders => {
                let Some(st) = self.stages.first() else {
                    return LiveFit {
                        exact: true,
                        agrees: true,
                    };
                };
                let top = self.orders.last().copied().unwrap_or(1);
                for (i, &raw) in x.iter().enumerate() {
                    let v = if raw.is_finite() { raw } else { 0.0 };
                    let xc = v.clamp(-1.0, 1.0);
                    let two_x = 2.0 * xc;
                    let (mut t_prev, mut t) = (1.0, xc);
                    let mut j = 0;
                    for k in 2..=top {
                        let t_next = two_x * t - t_prev;
                        t_prev = t;
                        t = t_next;
                        if self.orders.get(j) == Some(&k) {
                            out[j][i] = st.coef[k] * (t - T_SLOPE_0[k] * xc - T_AT_0[k]);
                            j += 1;
                        }
                    }
                }
                self.even_orders(st, out, scratch, seed)
            }
            LiveSplit::EvenOdd => {
                let (even, odd) = out.split_at_mut(1);
                let (pos, neg) = (&mut even[0], &mut odd[0]);
                pos.copy_from_slice(x);
                for (d, &v) in neg.iter_mut().zip(x) {
                    *d = -v;
                }
                let LiveScratch { e, rest, .. } = scratch;
                e.clear();
                e.resize(n, 0.0);
                rest.clear();
                rest.resize(n, 0.0);
                for v in [&mut *pos, &mut *neg] {
                    for st in &self.stages {
                        for (i, &s) in v.iter().enumerate() {
                            let (ev, od, r) = st.parts(s);
                            e[i] = ev;
                            rest[i] = r + od;
                        }
                        st.dc_block(e, 0);
                        for i in 0..n {
                            v[i] = rest[i] + e[i];
                        }
                    }
                }
                for (p, q) in pos.iter_mut().zip(neg.iter_mut()) {
                    let (a, b) = (*p, *q);
                    *p = 0.5 * (a + b);
                    *q = 0.5 * (a - b);
                }
                LiveFit {
                    exact: self.stages.iter().all(|st| st.dc.is_none()),
                    agrees: true,
                }
            }
        }
    }

    /// The even orders (raw terms in `out`) through their blockers, and
    /// their total into `scratch` ([`LiveScratch::even_total`]).
    ///
    /// One even order: its blocker is the stage's, so the stage's own
    /// state seeds it exactly. Several: the stage has one blocker on their
    /// sum, so the transient its state carries into the window (after a
    /// level change in deep bass) belongs to no single order (each
    /// order's offsets are opposite and cancel in the sum; splitting the
    /// state among them can read each far above the total). Each order
    /// then runs from its own steady state (its products without the
    /// transient) and only the total takes the stage's state: exact while
    /// that transient is below −30 dB of the total, an estimate otherwise.
    fn even_orders(
        &self,
        st: &CurveStage,
        out: &mut [Vec<f64>],
        scratch: &mut LiveScratch,
        seed: Option<(usize, f64)>,
    ) -> LiveFit {
        let n = out.first().map_or(0, Vec::len);
        let at = seed.map_or(0, |(i, _)| i.min(n));
        let evens = || (0..self.orders.len()).filter(|&j| self.orders[j].is_multiple_of(2));
        let count = evens().count();
        let mut exact = true;
        let mut z_model = 0.0;
        if st.dc.is_some() {
            for j in evens() {
                z_model += st.dc_block(&mut out[j], at);
            }
            if count == 1 {
                match seed {
                    Some((_, z)) => {
                        st.dc_shift(&mut out[evens().next().unwrap_or(0)], at, z - z_model)
                    }
                    None => exact = false,
                }
            }
        }
        let e = &mut scratch.e;
        e.clear();
        e.resize(n, 0.0);
        for j in evens() {
            for (t, v) in e.iter_mut().zip(&out[j]) {
                *t += v;
            }
        }
        if count >= 2
            && let Some((_, a1)) = st.dc
        {
            match seed {
                Some((_, z)) => {
                    let before = variance(e);
                    let delta = z - z_model;
                    st.dc_shift(e, at, delta);
                    // The transient's own energy over the window.
                    let r = -a1;
                    let (mut v, mut energy) = (delta, 0.0);
                    for _ in at..n {
                        energy += v * v;
                        v *= r;
                    }
                    let mut v = delta;
                    for _ in 0..at {
                        v /= r;
                        energy += v * v;
                    }
                    if energy > 1e-3 * before.max(variance(e)) {
                        exact = false;
                    }
                }
                None => exact = false,
            }
        }
        let limit = variance(e) * 10f64.powf(AGREE_DB / 10.0);
        let agrees = count < 2 || evens().all(|j| variance(&out[j]) <= limit);
        LiveFit { exact, agrees }
    }
}

/// Sum of squares about the mean.
fn variance(v: &[f64]) -> f64 {
    let m = v.iter().sum::<f64>() / v.len().max(1) as f64;
    v.iter().map(|a| (a - m) * (a - m)).sum()
}

/// How far (dB) an even order's line may read above the even total for
/// the lines to count as agreeing with it ([`LiveFit::agrees`]).
pub const AGREE_DB: f64 = 3.0;

/// How far [`LiveCurve::eval_with`]'s parts can be taken at face value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveFit {
    /// Every part is what the stage makes (as far as it is a static
    /// curve); `false`: the even orders are estimates this window (the
    /// DC blocker's transient after a level change, no seed, or a series
    /// chain whose stages block DC).
    pub exact: bool,
    /// No even order reads more than [`AGREE_DB`] above the even total.
    /// `false`: the orders' terms cancel each other (a curve whose
    /// Chebyshev terms are mostly the same harmonic at this level), so
    /// per-order lines would mislead; show the even total instead
    /// ([`LiveScratch::even_total`]) and the odd orders' sum.
    pub agrees: bool,
}

/// Why the stage built for `config` has no [`LiveCurve`], from its layers
/// alone (before building, so a tape layer is never calibrated here).
fn memory_reason(config: &HarmonicsConfig) -> Option<&'static str> {
    config
        .layers
        .iter()
        .filter(|l| l.enabled() && l.amount() > 0.0)
        .find_map(|l| match l {
            HarmonicsLayer::Flux(_) => Some(
                "a transformer layer has memory (its shelves): its products depend on what came before",
            ),
            HarmonicsLayer::Hysteresis(_) => {
                Some("a tape layer has memory (hysteresis): its products depend on what came before")
            }
            HarmonicsLayer::Chebyshev(_) => None,
        })
}

/// The stage `config` builds at `output_rate` as a [`LiveCurve`], or why it
/// has none (a layer with memory). Off the audio thread: builds the stage
/// (static Chebyshev layers only, which build fast).
pub fn live_curve(config: &HarmonicsConfig, output_rate: f64) -> Result<LiveCurve, String> {
    if let Some(why) = memory_reason(config) {
        return Err(why.into());
    }
    let stage = Harmonics::try_new(config, output_rate, 1).map_err(|e| e.to_string())?;
    stage.live_curve()
}

impl Harmonics {
    /// This stage as a [`LiveCurve`] ([`live_curve`]).
    pub fn live_curve(&self) -> Result<LiveCurve, String> {
        let one = |h: &Harmonics| -> Result<CurveStage, String> {
            if let Some(g) = h.groups.first() {
                return Err(match g {
                    crate::harmonics_layer::LayerGroup::Dynamic(_) => {
                        "sag and bias move the curve with the music's envelope"
                    }
                    crate::harmonics_layer::LayerGroup::Flux(_) => {
                        "a transformer layer has memory (its shelves)"
                    }
                    crate::harmonics_layer::LayerGroup::Hysteresis(_) => {
                        "a tape layer has memory (hysteresis)"
                    }
                }
                .into());
            }
            if !h.bank.tilt.is_empty() {
                return Err("tilted orders pass through filters, which have memory".into());
            }
            Ok(CurveStage {
                linear: h.bank.linear,
                coef: h.bank.coef,
                top: h.bank.top,
                dc: h.bank.dc_block.then_some((h.bank.hpf_b0, h.bank.hpf_a1)),
            })
        };
        if self.is_series() {
            let stages = self.series.iter().map(one).collect::<Result<Vec<_>, _>>()?;
            Ok(LiveCurve {
                split: LiveSplit::EvenOdd,
                stages,
                orders: Vec::new(),
                linear_gain: self.linear_gain(),
            })
        } else {
            let st = one(self)?;
            let orders = (2..=st.top).filter(|&k| st.coef[k] != 0.0).collect();
            Ok(LiveCurve {
                split: LiveSplit::Orders,
                stages: vec![st],
                orders,
                linear_gain: self.linear_gain(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Stage;

    fn tone(n: usize, rate: f64, hz: &[f64], amp: f64) -> Vec<f64> {
        (0..n)
            .map(|i| {
                hz.iter()
                    .map(|f| amp * (2.0 * std::f64::consts::PI * f * i as f64 / rate).sin())
                    .sum()
            })
            .collect()
    }

    /// The parts plus the linear part are the stage's output over an
    /// analysis window cut from a running stream: exactly without the DC
    /// blocker, and with it to a residual far under the products (the
    /// blocker's state before the window), once the window's mean is out.
    #[test]
    fn orders_sum_to_the_stage_output() {
        let rate = 96_000.0;
        for (dc_block, tol_db) in [(false, -250.0), (true, -35.0)] {
            let mut cfg = HarmonicsConfig::from_preset("se_triode_high_mu").unwrap();
            cfg.dc_block = dc_block;
            let mut stage = Harmonics::try_new(&cfg, rate, 1).unwrap();
            let curve = stage.live_curve().unwrap();
            assert_eq!(curve.split(), LiveSplit::Orders);
            assert!(curve.orders().contains(&2));
            // Music-like: a bass note and two more, beating.
            let x = tone(96_000, rate, &[55.0, 440.0, 660.0], 0.3);
            let mut y = x.clone();
            stage.process(&mut [&mut y[..]]);
            let (w0, w1) = (96_000 - 8192, 96_000);
            let mut parts = Vec::new();
            curve.eval(&x[w0..w1], &mut parts);
            let g = curve.linear_gain();
            let resid: Vec<f64> = (w0..w1)
                .map(|i| y[i] - g * x[i] - parts.iter().map(|p| p[i - w0]).sum::<f64>())
                .collect();
            let mean = resid.iter().sum::<f64>() / resid.len() as f64;
            let r2: f64 = resid.iter().map(|r| (r - mean) * (r - mean)).sum();
            let added: Vec<f64> = (w0..w1).map(|i| y[i] - g * x[i]).collect();
            let am = added.iter().sum::<f64>() / added.len() as f64;
            let a2: f64 = added.iter().map(|a| (a - am) * (a - am)).sum();
            let rel_db = 10.0 * (r2.max(1e-300) / a2).log10();
            assert!(
                rel_db < tol_db,
                "dc_block {dc_block}: residual {rel_db:.1} dB under the products"
            );
            eprintln!("dc_block {dc_block}: residual {rel_db:.1} dB relative to the added content");
        }
    }

    /// A stage with the given orders' levels (dB, H2 first) and every
    /// other order off, at drive 0 and amount 1, its DC blocker on.
    fn orders_only(levels: &[(usize, f64)]) -> HarmonicsConfig {
        let mut cfg = HarmonicsConfig::from_preset("se_triode_high_mu").unwrap();
        let audivane_protocol::HarmonicsLayer::Chebyshev(l) = &mut cfg.layers[0] else {
            panic!("a Chebyshev preset");
        };
        l.levels_db = [None; 8];
        for &(k, db) in levels {
            l.levels_db[k - 2] = Some(db);
        }
        l.signs = [audivane_protocol::HarmonicSign::Positive; 8];
        l.invert_even = false;
        l.drive_db = 0.0;
        l.amount = 1.0;
        assert!(cfg.dc_block);
        cfg
    }

    /// A tone of amplitude `amp(i)` at 55 Hz through the stage with
    /// `levels` (blocks of 1000) and through one stage per order (the
    /// truth for each line; the stage
    /// is their sum). The window is the 8192 samples from `from`.
    struct Split {
        /// Each order's line, dB.
        lines: Vec<f64>,
        /// Each order alone through its own stage, dB.
        alone: Vec<f64>,
        /// What the stage adds (all even here), dB.
        measured: f64,
        /// The split's even total, dB.
        even_total: f64,
        fit: LiveFit,
    }

    fn split_vs_truth(levels: &[(usize, f64)], amp: impl Fn(usize) -> f64, from: usize) -> Split {
        let rate = 96_000.0;
        let n = 8192;
        let total = from + n;
        let x: Vec<f64> = (0..total)
            .map(|i| amp(i) * (2.0 * std::f64::consts::PI * 55.0 * i as f64 / rate).sin())
            .collect();
        let run = |levels: &[(usize, f64)]| {
            let mut st = Harmonics::try_new(&orders_only(levels), rate, 1).unwrap();
            let mut y = x.clone();
            let mut ends = Vec::new();
            for (b, chunk) in y.chunks_mut(1000).enumerate() {
                st.process(&mut [chunk]);
                ends.push(((b + 1) * 1000, st.live_dc(0).unwrap()));
            }
            let g = st.linear_gain();
            let added: Vec<f64> = (from..total).map(|i| y[i] - g * x[i]).collect();
            (st, added, ends)
        };
        let (stage, added, ends) = run(levels);
        let g = stage.linear_gain();
        let curve = stage.live_curve().unwrap();
        let (end, z) = *ends.iter().find(|(p, _)| *p >= from).unwrap();
        let level = |v: &[f64]| 10.0 * (variance(v) / v.len() as f64).log10();
        let alone = levels
            .iter()
            .map(|&o| {
                let (st, a, _) = run(&[o]);
                level(&a) + 20.0 * (g / st.linear_gain()).log10()
            })
            .collect();
        let mut parts = Vec::new();
        let mut scratch = LiveScratch::default();
        let fit = curve.eval_with(&x[from..], &mut parts, &mut scratch, Some((end - from, z)));
        Split {
            lines: parts.iter().map(|p| level(p)).collect(),
            alone,
            measured: level(&added),
            even_total: level(scratch.even_total()),
            fit,
        }
    }

    /// Two orders, H2 −30 and H4 −42 dB (same signs): a 55 Hz tone
    /// dropping from 0.8 to 0.2 as the window starts. Each order alone
    /// would carry a large blocker transient (they cancel in the stage's
    /// one blocker), so right after the drop the lines are estimates, said
    /// so, and below the even total; the total itself is exact. Half a
    /// second on, the two terms cancel each other at this level (T4 is
    /// mostly a 2nd harmonic down here, and 2·a2 = 8·a4): the lines no
    /// longer agree with the total, and the split says so.
    #[test]
    fn two_even_orders_after_a_level_drop() {
        let levels = [(2, -30.0), (4, -42.0)];
        let drop = |i: usize| if i < 96_000 { 0.8 } else { 0.2 };
        let s = split_vs_truth(&levels, drop, 96_000);
        assert!(!s.fit.exact, "an estimate right after the drop");
        assert!(s.fit.agrees);
        assert!(
            (s.even_total - s.measured).abs() < 0.1,
            "{s:?}",
            s = (s.even_total, s.measured)
        );
        for (k, (l, a)) in s.lines.iter().zip(&s.alone).enumerate() {
            assert!(*l <= s.even_total + AGREE_DB, "H{}: {l:.2}", 2 * k + 2);
            eprintln!(
                "after the drop: H{} {l:.2} dB (alone {a:.2}), even total {:.2}",
                2 * k + 2,
                s.even_total
            );
        }
        let s = split_vs_truth(&levels, drop, 96_000 + 48_000);
        assert!(
            !s.fit.agrees,
            "the terms cancel: {:?} vs {:.2}",
            s.lines, s.even_total
        );
        assert!((s.even_total - s.measured).abs() < 0.1);
    }

    /// H2 and H4 whose DC nearly cancels on a steady tone (H4 −30.97 dB
    /// against H2 −30 at 0.98): no division by that small sum; each line
    /// reads its order, exactly, and agrees with the total.
    #[test]
    fn even_orders_whose_dc_cancels_read_true() {
        let levels = [(2, -30.0), (4, -30.97)];
        let s = split_vs_truth(&levels, |_| 0.98, 96_000);
        assert!(s.fit.exact && s.fit.agrees, "{:?}", s.fit);
        for (k, (l, a)) in s.lines.iter().zip(&s.alone).enumerate() {
            assert!((l - a).abs() < 0.1, "H{}: {l:.2} vs {a:.2} dB", 2 * k + 2);
        }
        assert!((s.even_total - s.measured).abs() < 0.1);
    }

    /// H2 alone: a 55 Hz tone dropping from 0.8 to 0.2 right at the
    /// window's start. The stage's blocker still holds the loud
    /// passage's DC and decays through the window; seeded with its state
    /// (read at a block's end, as the tap does), the H2 part reads what
    /// the stage adds to within 0.1 dB. Unseeded (the window looped) it
    /// reads far low.
    #[test]
    fn the_seeded_blocker_follows_a_level_drop_in_deep_bass() {
        let rate = 96_000.0;
        let mut cfg = HarmonicsConfig::from_preset("se_triode_high_mu").unwrap();
        let audivane_protocol::HarmonicsLayer::Chebyshev(l) = &mut cfg.layers[0] else {
            panic!("a Chebyshev preset");
        };
        l.levels_db = [Some(-30.0), None, None, None, None, None, None, None];
        l.drive_db = 0.0;
        l.amount = 1.0;
        assert!(cfg.dc_block);
        let mut stage = Harmonics::try_new(&cfg, rate, 1).unwrap();
        let curve = stage.live_curve().unwrap();
        let drop_at = 96_000;
        let n = 8192;
        let total = drop_at + n;
        let x: Vec<f64> = (0..total)
            .map(|i| {
                let a = if i < drop_at { 0.8 } else { 0.2 };
                a * (2.0 * std::f64::consts::PI * 55.0 * i as f64 / rate).sin()
            })
            .collect();
        let mut y = x.clone();
        // Blocks of 1000 (block ends off the window's start), the
        // blocker's state read at each end.
        let mut ends = Vec::new();
        for (b, chunk) in y.chunks_mut(1000).enumerate() {
            stage.process(&mut [chunk]);
            ends.push(((b + 1) * 1000, stage.live_dc(0).unwrap()));
        }
        let (end, z) = *ends.iter().find(|(p, _)| *p >= drop_at).unwrap();
        let w = &x[drop_at..];
        let g = curve.linear_gain();
        let added: Vec<f64> = (drop_at..total).map(|i| y[i] - g * x[i]).collect();
        let level = |v: &[f64]| {
            let m = v.iter().sum::<f64>() / v.len() as f64;
            10.0 * (v.iter().map(|a| (a - m) * (a - m)).sum::<f64>() / v.len() as f64).log10()
        };
        let mut scratch = LiveScratch::default();
        let mut parts = Vec::new();
        curve.eval_with(w, &mut parts, &mut scratch, Some((end - drop_at, z)));
        let seeded = level(&parts[0]) - level(&added);
        assert!(seeded.abs() < 0.1, "seeded H2 {seeded:+.3} dB off");
        curve.eval_with(w, &mut parts, &mut scratch, None);
        let looped = level(&parts[0]) - level(&added);
        assert!(
            looped < -3.0,
            "unseeded only {looped:+.2} dB off: the case is too easy"
        );
        eprintln!("H2 after the drop: seeded {seeded:+.3} dB, looped {looped:+.2} dB");
    }

    #[test]
    fn series_splits_even_and_odd_and_memory_layers_have_none() {
        let mut cfg = HarmonicsConfig::from_preset("se_triode_high_mu").unwrap();
        let layer = cfg.layers[0].clone();
        cfg.layers.push(layer);
        cfg.routing = audivane_protocol::HarmonicsRouting::Series;
        let curve = live_curve(&cfg, 96_000.0).unwrap();
        assert_eq!(curve.split(), LiveSplit::EvenOdd);
        let x = tone(4800, 96_000.0, &[1000.0], 0.5);
        let mut parts = Vec::new();
        curve.eval(&x, &mut parts);
        // The odd part of a sine has no even harmonics: its value half a
        // period later is its negative.
        let half = 48;
        for i in 1000..1100 {
            assert!((parts[1][i] + parts[1][i + half]).abs() < 1e-9);
            assert!((parts[0][i] - parts[0][i + half]).abs() < 1e-9);
        }
        let tape = HarmonicsConfig::from_preset("tape_master").unwrap();
        assert!(live_curve(&tape, 96_000.0).is_err());
    }
}
