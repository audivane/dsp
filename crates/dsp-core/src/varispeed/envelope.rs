//! The speed envelope: how a speed command becomes one cubic piece of the
//! core's step per call.
//!
//! Every curve is in **log speed** `l = ln s` against the core's output
//! index `k` (2G outputs). The splatter of a speed change follows the
//! curvature `l''`, so every curve is held to one curvature limit `C`: the
//! peak curvature of the reference ramp, a raised cosine over ln 4 (0.5 → 2)
//! in [`REFERENCE_RAMP_S`] (measured ≤ −126 dB re a 10 kHz tone).
//!
//! - **From rest** (the step's slope is zero): a raised cosine to the
//!   target over `max(ramp, T_min, 20 ms)`, with `T_min = 200 ms ·
//!   sqrt(|Δl| / ln 4)`, the shortest cosine within `C`, and
//!   [`MIN_RAMP_S`] the floor.
//! - **Mid-ramp, heading towards the new target:** a cubic Hermite from the
//!   current value and slope to the target with zero end slope, over the
//!   shortest duration of at least `ramp` (and the floor) within `C`, when
//!   that Hermite is
//!   monotone (no overshoot: `m0 · K ≤ 3 Δl`).
//! - **Otherwise** (the slope points away from the target, or the target is
//!   too close to stop at within `C`, or the ramp asked is so long that a
//!   Hermite would overshoot): a brake, the quadratic that takes the slope
//!   to zero at curvature `C`, then the cosine from where it stops. A brake
//!   never leaves the range the previous curves span (a cosine at or below
//!   `C` always has room to stop before its own end), so commands within
//!   `[0.5, 2]` keep the step there.
//!
//! The planner emits each call's [`Segment::Piece`] from the curve's value
//! and slope at the call's end (converted to step units), and the previous
//! piece's end slope as the start slope, so the step is C¹ across calls. On
//! the call where the curve ends, the piece spans the outputs up to the
//! curve's end and ends on the target's bits with zero slope (the endpoint
//! rule); the rest of that call, and every call after it, holds. A piece
//! also ends where a brake hands over to its cosine, and the stage cuts
//! long calls into pieces of at most [`crate::varispeed::PIECE_MAX`]
//! outputs, so a call may run several pieces.

use crate::adaptive::{Segment, Step};
use std::f64::consts::{LN_2, PI};

/// The reference ramp: a raised cosine over ln 4 (0.5 → 2) in this many
/// seconds sets the curvature limit.
pub const REFERENCE_RAMP_S: f64 = 0.2;
/// Log-speed span of the reference ramp (ln 4).
pub const REFERENCE_SPAN: f64 = 2.0 * LN_2;
/// No ramp is shorter than this, seconds. Within the curvature limit a
/// small change could ramp in well under a millisecond (`T_min` for 1 ->
/// 1.001 is 4.3 ms), which concentrates its splatter at the target's edge.
/// A time, not a period, so it does not depend on the rate or quantum.
pub const MIN_RAMP_S: f64 = 0.02;
/// Relative margin on a brake's stop in [`Envelope::peak_speed`]: the stop
/// is an interior value of the curve, which the integer pieces may round
/// past by far less (about 1e-14 at most); a brake that would stop this
/// close under a cap counts as reaching it.
pub const PEAK_MARGIN: f64 = 1e-9;

/// One curve of log speed over `0..=len` outputs.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Curve {
    /// Raised cosine from `l0` to `l1`, zero slope at both ends.
    Cosine { l0: f64, l1: f64, len: f64 },
    /// Cubic Hermite from `l0` with slope `m0` (per output) to `l1` with
    /// zero slope; monotone by construction.
    Hermite { l0: f64, m0: f64, l1: f64, len: f64 },
    /// `l0 + m0 k - m0 k² / (2 len)`: the slope falls linearly to zero.
    Brake { l0: f64, m0: f64, len: f64 },
}

impl Curve {
    fn len(&self) -> f64 {
        match *self {
            Self::Cosine { len, .. } | Self::Hermite { len, .. } | Self::Brake { len, .. } => len,
        }
    }

    /// Log speed and its slope (per output) at `k`, clamped to the curve.
    fn eval(&self, k: f64) -> (f64, f64) {
        match *self {
            Self::Cosine { l0, l1, len } => {
                let u = (k / len).clamp(0.0, 1.0);
                let d = l1 - l0;
                (
                    l0 + d * 0.5 * (1.0 - (PI * u).cos()),
                    d * 0.5 * PI / len * (PI * u).sin(),
                )
            }
            Self::Hermite { l0, m0, l1, len } => {
                let u = (k / len).clamp(0.0, 1.0);
                let (u2, u3) = (u * u, u * u * u);
                let l = (2.0 * u3 - 3.0 * u2 + 1.0) * l0
                    + (u3 - 2.0 * u2 + u) * len * m0
                    + (-2.0 * u3 + 3.0 * u2) * l1;
                let dl = ((6.0 * u2 - 6.0 * u) * l0
                    + (3.0 * u2 - 4.0 * u + 1.0) * len * m0
                    + (-6.0 * u2 + 6.0 * u) * l1)
                    / len;
                (l, dl)
            }
            Self::Brake { l0, m0, len } => {
                let k = k.clamp(0.0, len);
                (l0 + m0 * k - m0 * k * k / (2.0 * len), m0 * (1.0 - k / len))
            }
        }
    }
}

/// The envelope planner's state: the curve in progress (if any), how far
/// into it the committed calls are, and the step's slope at the next
/// output. It is `Copy`, so a call can be planned on a copy and committed by
/// keeping the copy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Envelope {
    first: Option<Curve>,
    /// The cosine after a brake.
    second: Option<Curve>,
    /// Outputs into `first` (then into `second`).
    elapsed: f64,
    target: Step,
    /// Slope of the step at the next output, step units per output: the
    /// last piece's end slope.
    slope: f64,
    /// Curvature limit, log speed per output².
    curvature: f64,
    /// Outputs per second (the core runs at 2G).
    rate: f64,
    /// The shortest ramp, outputs ([`MIN_RAMP_S`]).
    min_len: f64,
}

impl Envelope {
    /// Holding at `speed`, for a core running `outputs_per_second` (> 0).
    pub fn new(outputs_per_second: f64, speed: Step) -> Self {
        let k_ref = REFERENCE_RAMP_S * outputs_per_second;
        Self {
            first: None,
            second: None,
            elapsed: 0.0,
            target: speed,
            slope: 0.0,
            curvature: 0.5 * REFERENCE_SPAN * (PI / k_ref).powi(2),
            rate: outputs_per_second,
            min_len: (MIN_RAMP_S * outputs_per_second).max(1.0),
        }
    }

    /// Hold at `speed` from the next output (a reset).
    pub fn hold_at(&mut self, speed: Step) {
        *self = Self::new(self.rate, speed);
    }

    /// The step the envelope is heading to (or holding at).
    pub fn target(&self) -> Step {
        self.target
    }

    /// True while a curve is in progress.
    pub fn ramping(&self) -> bool {
        self.first.is_some()
    }

    /// Slope of the step at the next output (step units per output).
    pub fn slope(&self) -> f64 {
        self.slope
    }

    /// The curvature limit, log speed per output².
    pub fn curvature_limit(&self) -> f64 {
        self.curvature
    }

    /// Outputs a ramp of `ms` milliseconds lasts.
    pub fn outputs_for_ms(&self, ms: f64) -> f64 {
        ms * 1e-3 * self.rate
    }

    /// `T_min` for a log-speed change `dl`, in outputs: the shortest raised
    /// cosine within the curvature limit.
    pub fn cosine_len(&self, dl: f64) -> f64 {
        PI * (dl.abs() / (2.0 * self.curvature)).sqrt()
    }

    /// Outputs left until the curve in progress ends (0 when holding).
    pub fn remaining(&self) -> f64 {
        match self.first {
            None => 0.0,
            Some(c) => (c.len() + self.second.map_or(0.0, |s| s.len()) - self.elapsed).max(0.0),
        }
    }

    /// The shortest Hermite duration of at least `ramp` within the
    /// curvature limit, from slope `m0` towards a change `dl` of the same
    /// sign, or `None` when no such Hermite is monotone.
    fn hermite_len(&self, dl: f64, m0: f64, ramp: f64) -> Option<f64> {
        let (d, m, c) = (dl.abs(), m0.abs(), self.curvature);
        // Curvature at the ends: |6d - 4mK| / K² and |6d - 2mK| / K².
        let k1 = (-m + (m * m + 6.0 * c * d).sqrt()) / c;
        let mut k = ramp.max(k1).max(1.0);
        if 4.0 * m * k > 6.0 * d && c * k * k - 4.0 * m * k + 6.0 * d < 0.0 {
            k = (2.0 * m + (4.0 * m * m - 6.0 * c * d).max(0.0).sqrt()) / c;
        }
        (m * k <= 3.0 * d && k.is_finite()).then_some(k)
    }

    /// Start a new curve from `current` (the core's step at the next
    /// output, with this envelope's slope) to `target`, lasting at least
    /// `ramp` outputs (0 means as short as the curvature limit allows).
    pub fn retarget(&mut self, current: Step, target: Step, ramp: f64) {
        let ramp = if ramp.is_finite() { ramp } else { 0.0 }.max(self.min_len);
        let s0 = current.to_f64();
        let (l0, l1) = (s0.ln(), target.to_f64().ln());
        let dl = l1 - l0;
        let m0 = self.slope / s0;
        self.target = target;
        self.elapsed = 0.0;
        self.second = None;
        if self.slope == 0.0 {
            self.first = (current != target).then_some(Curve::Cosine {
                l0,
                l1,
                len: ramp.max(self.cosine_len(dl)).max(1.0),
            });
            return;
        }
        if m0 * dl > 0.0
            && let Some(len) = self.hermite_len(dl, m0, ramp)
        {
            self.first = Some(Curve::Hermite { l0, m0, l1, len });
            return;
        }
        let kb = (m0.abs() / self.curvature).max(1.0);
        let stop = l0 + m0 * kb / 2.0;
        self.first = Some(Curve::Brake { l0, m0, len: kb });
        self.second = Some(Curve::Cosine {
            l0: stop,
            l1,
            len: (ramp - kb)
                .max(self.cosine_len(l1 - stop))
                .max(self.min_len),
        });
    }

    /// Log speed and slope at `k` outputs into the curve in progress.
    fn eval(&self, k: f64) -> (f64, f64) {
        let first = self.first.expect("eval while holding");
        match self.second {
            Some(second) if k > first.len() => second.eval(k - first.len()),
            _ => first.eval(k),
        }
    }

    /// The piece for a call of `n` outputs: the segment, how many of the
    /// call's outputs it spans (the rest of the call is planned again),
    /// the envelope after the piece, and the range of the curve over the
    /// piece (the step must stay inside it). `self` is not changed (commit
    /// by keeping the returned copy).
    ///
    /// A piece spans at most `n` outputs and stops at the earliest
    /// boundary ahead: the end of a brake (where the curvature changes;
    /// one cubic across it can overshoot the brake) and the curve's end
    /// (rounded up to whole outputs), where it lands on the target's bits
    /// with zero slope (stretched over a longer call, a piece that starts
    /// with the curve's slope and must end flat overshoots the target).
    pub fn segment(&self, n: usize) -> (Segment, usize, Envelope, (f64, f64)) {
        let mut after = *self;
        let Some(first) = self.first else {
            let t = self.target.to_f64();
            return (Segment::Hold, n, after, (t, t));
        };
        if n == 0 {
            let t = self.eval(self.elapsed).0.exp();
            return (Segment::Hold, 0, after, (t, t));
        }
        let left = self.remaining();
        let mut span = n;
        let junction = (self.second.is_some() && self.elapsed < first.len()).then(|| first.len());
        if let Some(j) = junction {
            span = span.min(((j - self.elapsed).ceil() as usize).max(1));
        }
        let a = self.elapsed;
        let la = self.eval(a).0;
        let mut lo = la;
        let mut hi = la;
        if span as f64 >= left {
            // The endpoint rule: land on the target's bits, zero slope.
            let span = (left.ceil() as usize).clamp(1, span);
            let lt = self.target.to_f64().ln();
            lo = lo.min(lt);
            hi = hi.max(lt);
            if let Some(j) = junction {
                let lj = self.eval(j).0;
                lo = lo.min(lj);
                hi = hi.max(lj);
            }
            after.first = None;
            after.second = None;
            after.elapsed = 0.0;
            after.slope = 0.0;
            return (
                Segment::Piece {
                    slope0: self.slope,
                    end: self.target,
                    slope1: 0.0,
                },
                span,
                after,
                (lo.exp(), hi.exp()),
            );
        }
        let b = a + span as f64;
        let (l, dl) = self.eval(b);
        lo = lo.min(l);
        hi = hi.max(l);
        if let Some(j) = junction.filter(|&j| j <= b) {
            // Every curve is monotone on its own span, so the piece's range
            // is spanned by its ends and the junction it reaches.
            let lj = self.eval(j).0;
            lo = lo.min(lj);
            hi = hi.max(lj);
        }
        let mut s = l.exp().clamp(Step::MIN.to_f64(), Step::MAX.to_f64());
        // On the curve towards the target the end never passes it (the
        // curve is monotone; `exp` may round a value just short of it to
        // the far side).
        let last = match self.second {
            Some(second) if b > first.len() => Some(second),
            None => Some(first),
            Some(_) => None,
        };
        if let Some(Curve::Cosine { l0, l1, .. } | Curve::Hermite { l0, l1, .. }) = last {
            let t = self.target.to_f64();
            s = if l1 >= l0 { s.min(t) } else { s.max(t) };
        }
        let end = Step::from_f64(s).unwrap_or(self.target);
        after.elapsed = b;
        after.slope = s * dl;
        (
            Segment::Piece {
                slope0: self.slope,
                end,
                slope1: after.slope,
            },
            span,
            after,
            (lo.exp(), hi.exp()),
        )
    }

    /// The largest speed the step reaches over the next `k` outputs from
    /// `current` (the curves are monotone on their own spans, so the ends
    /// of each window and the brake's end bound it).
    pub fn peak_within(&self, current: Step, k: f64) -> f64 {
        let mut l = current.to_f64().ln();
        let Some(first) = self.first else {
            return l.exp();
        };
        let (a, b) = (
            self.elapsed,
            (self.elapsed + k).min(self.elapsed + self.remaining()),
        );
        let len1 = first.len();
        if a < len1 {
            l = l.max(first.eval(a).0).max(first.eval(b.min(len1)).0);
        }
        if let Some(second) = self.second
            && b > len1
        {
            let a2 = (a - len1).max(0.0);
            l = l.max(second.eval(a2).0).max(second.eval(b - len1).0);
        }
        if self.elapsed + k >= self.elapsed + self.remaining() {
            l = l.max(self.target.to_f64().ln());
        }
        l.exp()
    }

    /// The largest speed the step can reach from `current` until the curve
    /// in progress ends (`current` itself when holding): `current`, the
    /// target, and the stop of a brake still ahead (raised by
    /// [`PEAK_MARGIN`]). The cosines and Hermites are monotone between
    /// their ends, so nothing else can be higher; the two ends are exact
    /// bits, so a ramp to a cap is within it.
    pub fn peak_speed(&self, current: Step) -> f64 {
        let mut p = current.to_f64().max(self.target.to_f64());
        if let Some(first @ Curve::Brake { len, .. }) = self.first
            && self.elapsed < len
        {
            p = p.max(first.eval(len).0.exp() * (1.0 + PEAK_MARGIN));
        }
        p
    }

    /// Abandon the curve in progress and hold at `speed` (a fallback when
    /// the core refuses a piece; never reached for in-range commands).
    pub(crate) fn snap(&mut self, speed: Step) {
        self.first = None;
        self.second = None;
        self.elapsed = 0.0;
        self.slope = 0.0;
        self.target = speed;
    }
}
