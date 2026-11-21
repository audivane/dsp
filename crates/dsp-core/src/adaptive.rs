//! Adaptive (variable-ratio) resampling core: a kernel table of [`PHASES`]
//! phases per input frame with 4-point cubic (Lagrange) interpolation between
//! adjacent phases, driven by an exact 64.64 fixed-point position.
//!
//! The core runs inside the ×2 sandwich: its input is band-limited to
//! [`PASSBAND`] (cycles per input frame) by a steep ×2 in front of it, and its
//! kernel stops from [`STOPBAND`], where the ×2's images start. So one short
//! kernel ([`CoreTable`], 25 taps) serves every step up to [`Step::MAX`] = 2:
//! sampling at spacing `s` folds content above `0.5 / s`, which is at or above
//! 0.25 for `s <= 2`.
//!
//! **Position.** The position of the next output is a `u128` in 64.64 fixed
//! point (input frames). Each output adds the step `Δ` to it, and within one
//! call the step follows a cubic in the output index through three integer
//! forward differences (`d1, d2, d3`, units of 2⁻⁶⁴). Every update is an
//! integer add, so two runtimes given the same segments at the same calls
//! compute the same positions bit for bit. Output `k` of a call sits at
//! [`position_after`]. The top [`LOG2_PHASES`] bits of the fraction pick the
//! phase, the next 53 bits are the interpolation weight `μ`.
//!
//! **Pull contract.** The caller asks for exactly `n` outputs:
//! [`AdaptiveCore::plan`] latches the call's [`Segment`] (how the step moves
//! over these `n` outputs) and says how many new input frames that takes
//! (`need`); [`AdaptiveCore::process`] must follow with the same `n` and `need`
//! frames, or `need + 1` when `need > 0` (rounding up to whole source frames
//! before a ×2 over-pulls by at most one). A `plan` without a `process` (a
//! period abandoned for a reopen) is discarded by the next `plan`; a zero call
//! (`n = 0`) needs nothing and changes nothing.
//!
//! **Speed changes.** A [`Segment::Piece`] is one cubic Hermite piece of the
//! step over the call's outputs, matching value and slope at both ends, so the
//! step is C¹ across calls when the caller passes each piece's end slope as the
//! next one's start slope.
//!
//! **Real time.** [`AdaptiveCore::new`] allocates everything (the shared table
//! is built and measured once per process, off the audio thread). `plan`,
//! `process` and `reset` never allocate, lock or block. `process` panics only
//! on caller contract breaks (no plan, wrong slice lengths).
//!
//! **Causality.** The window ends at the newest received frame,
//! `floor(position)`. The cubic's upper guard rows (μ = 1 + 1/P, 1 + 2/P)
//! would want one tap past it; the table leaves it out so the pull stays
//! causal, and the table's measurement includes the omission.

use crate::dot::Kernels;
use crate::window::{bessel_i0, kaiser_beta_for_attenuation};
use std::sync::{Arc, OnceLock};

/// log2 of the table phases per input frame.
pub const LOG2_PHASES: u32 = 8;
/// Table phases per input frame (P).
pub const PHASES: usize = 1 << LOG2_PHASES;
/// Passband end of the core kernel, cycles per input frame.
pub const PASSBAND: f64 = 0.25;
/// Stopband start of the core kernel, cycles per input frame.
pub const STOPBAND: f64 = 0.75;
/// Stopband the core must reach (the steep filters' 150 dB plus 20 dB).
pub const TARGET_DB: f64 = 170.0;
/// Kaiser's formula is short of its target at this length: the window's β
/// is designed for the target plus this margin, and the table measured.
pub const DESIGN_MARGIN_DB: f64 = 2.0;
/// The interpolation error must sit at least this far below the target.
pub const INTERPOLATION_MARGIN_DB: f64 = 20.0;
/// Largest passband deviation the table may measure (dB).
pub const MAX_PASSBAND_RIPPLE_DB: f64 = 1e-5;
/// Frames a call may deliver beyond `need` (only when `need > 0`).
pub const MAX_OVERPULL: usize = 1;
/// Received frames at or after the next output's newest frame, at most.
pub const MAX_RETAINED: usize = 2;
/// Most outputs one call may plan: twice the largest nominal period (8192
/// frames, the quantum limit), since the ÷2 after the core takes at most
/// `2n` core outputs for `n` nominal frames. The core's input is
/// provisioned for this many outputs at [`Step::MAX`] (with the piece
/// tolerance and the trajectory bound of [`Segment::Piece`]).
pub const MAX_CALL_OUTPUTS: usize = 1 << 14;
/// Most channels a core takes (the chain's limit).
pub const MAX_CHANNELS: usize = crate::chain::MAX_CHANNELS;

const ONE_F: f64 = 18_446_744_073_709_551_616.0; // 2^64
/// Relative tolerance on the step range for a piece's interior (the
/// integer differences round; a Hermite piece of a smooth curve may
/// overshoot its end value by rounding).
const RANGE_TOL: f64 = 1e-6;
/// A cubic within the step range on `0..=n` has `|v'| <= 9 (2 - 0.25)(1 +
/// RANGE_TOL) / n < 16 / n` everywhere (Markov's inequality), so steeper
/// slopes are refused before any arithmetic; that bounds every
/// intermediate of the piece solve.
const SLOPE_LIMIT: f64 = 16.0;

/// A step (input frames per output frame) in 64.64 fixed point, within
/// [`Step::MIN`, `Step::MAX`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Step(u128);

impl Step {
    pub const ONE: Step = Step(1 << 64);
    /// 0.25: below the user range (0.5) with room for a follower's
    /// correction.
    pub const MIN: Step = Step(1 << 62);
    /// 2: the largest step the kernel serves without aliasing.
    pub const MAX: Step = Step(2 << 64);

    /// `round(s · 2⁶⁴)`, or `None` outside the range (or not finite). For
    /// `s` in range this is `s` exactly: an `f64` of at least 2⁻² has no
    /// bits below 2⁻⁵⁴.
    pub fn from_f64(s: f64) -> Option<Step> {
        if !s.is_finite() {
            return None;
        }
        let b = (s * ONE_F).round();
        if b < Self::MIN.0 as f64 || b > Self::MAX.0 as f64 {
            return None;
        }
        Some(Step(b as u128))
    }

    /// From raw 64.64 bits, or `None` outside the range.
    pub fn from_bits(bits: u128) -> Option<Step> {
        (Self::MIN.0..=Self::MAX.0)
            .contains(&bits)
            .then_some(Step(bits))
    }

    pub fn bits(self) -> u128 {
        self.0
    }

    pub fn to_f64(self) -> f64 {
        self.0 as f64 / ONE_F
    }
}

/// How the step moves over one call's outputs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Segment {
    /// The step stays where it is.
    Hold,
    /// The step is this value from the call's first output on (an
    /// instant change: resets and tests; speed commands use pieces).
    Jump(Step),
    /// One cubic Hermite piece: from the current step with slope `slope0`
    /// to `end` with slope `slope1`, over the call's `n` outputs. Slopes
    /// are in step units per output (input frames per output²). The step
    /// after the call is `end`, exactly.
    ///
    /// The differences are integers (units of 2⁻⁶⁴): `d3` is rounded, its
    /// error folded into `d2` before that is rounded, and `d1` solved last
    /// for the end. The step at output `k` then deviates from the ideal
    /// cubic by at most `k(n-k)/4 + k(n-k)|2k-n|/24 + k/2` units, at most
    /// `n³/249 + n²/16 + n/2`.
    Piece { slope0: f64, end: Step, slope1: f64 },
}

/// Why a plan was refused. The latched segment and every state are left
/// as they were (no plan is pending).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlanError {
    /// More outputs than the core was built for.
    TooManyOutputs { n: usize, max: usize },
    /// A piece's slopes are not finite, or steeper than any piece within
    /// the step range can be (`16 / n`).
    BadSlope,
    /// A piece leaves [`Step::MIN`, `Step::MAX`] between its ends.
    StepOutOfRange { min: f64, max: f64 },
    /// The call would need more input than the core is provisioned for, or
    /// its positions would not fit. Not reachable with in-range segments
    /// and calls within [`MAX_CALL_OUTPUTS`]; a guard, not a limit.
    Overflow,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyOutputs { n, max } => {
                write!(f, "{n} outputs in one call, at most {max}")
            }
            Self::BadSlope => write!(f, "a piece slope is not finite or too steep"),
            Self::StepOutOfRange { min, max } => write!(
                f,
                "the piece's step runs over {min}..{max}, outside {}..{}",
                Step::MIN.to_f64(),
                Step::MAX.to_f64()
            ),
            Self::Overflow => write!(f, "the call's input or positions do not fit"),
        }
    }
}

impl std::error::Error for PlanError {}

/// A latched call: its output count, the input it needs, and the step's
/// trajectory over it (exactly what `process` will apply).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Planned {
    /// Outputs of the call.
    pub outputs: usize,
    /// New input frames it needs.
    pub need: usize,
    /// The step at the call's first output, 64.64.
    pub step: u128,
    /// Forward differences of the step, units of 2⁻⁶⁴.
    pub d: [i128; 3],
    /// The step after the call, 64.64 (the endpoint rule sets it).
    pub end: u128,
    /// The position after the call, relative like the accumulator's.
    next: u128,
}

/// Position of output `k` (counted from 0, the next output), given the
/// next output's position `pos`, the step `step` and its forward
/// differences `d`, all as in [`Planned`]: `pos + kΔ + C(k,2) d1 +
/// C(k,3) d2 + C(k,4) d3`, equal to `k` rounds of the per-output adds.
/// `None` when it does not fit: `pos` or `step` at or above 2¹²⁷, any term
/// overflowing `i128`, or a negative result. The core's own arguments are
/// far inside (relative positions below 2⁸⁰, `k <= MAX_CALL_OUTPUTS`).
pub fn position_after(pos: u128, step: u128, d: [i128; 3], k: usize) -> Option<u128> {
    let pos = i128::try_from(pos).ok()?;
    let step = i128::try_from(step).ok()?;
    let k = i128::try_from(k).ok()?;
    let c2 = k.checked_mul(k - 1)? / 2;
    let c3 = c2.checked_mul(k - 2)? / 3;
    let c4 = c3.checked_mul(k - 3)? / 4;
    let p = pos
        .checked_add(k.checked_mul(step)?)?
        .checked_add(c2.checked_mul(d[0])?)?
        .checked_add(c3.checked_mul(d[1])?)?
        .checked_add(c4.checked_mul(d[2])?)?;
    u128::try_from(p).ok()
}

/// Round-to-nearest integer division (`b > 0`), halves away from minus
/// infinity; `None` on overflow.
fn div_round(a: i128, b: i128) -> Option<i128> {
    Some(
        a.checked_mul(2)?
            .checked_add(b)?
            .div_euclid(b.checked_mul(2)?),
    )
}

/// `x` rounded to an integer, or `None` when it is not finite or does not
/// fit comfortably in `i128`.
fn round_i128(x: f64) -> Option<i128> {
    (x.is_finite() && x.abs() < 2f64.powi(120)).then(|| x.round() as i128)
}

/// The smallest and largest value of `v0 + (b k + c k² + d k³) / 2⁶⁴` on
/// `0 <= k <= n` (coefficients in units of 2⁻⁶⁴).
fn cubic_range(v0: f64, b: f64, c: f64, d: f64, n: f64) -> (f64, f64) {
    let g = |k: f64| v0 + (b * k + c * k * k + d * k * k * k) / ONE_F;
    let (mut lo, mut hi) = (g(0.0).min(g(n)), g(0.0).max(g(n)));
    // g'(k) = b + 2c k + 3d k²
    let mut roots = [f64::NAN; 2];
    if d != 0.0 {
        let disc = 4.0 * c * c - 12.0 * b * d;
        if disc >= 0.0 {
            let s = disc.sqrt();
            roots = [(-2.0 * c + s) / (6.0 * d), (-2.0 * c - s) / (6.0 * d)];
        }
    } else if c != 0.0 {
        roots[0] = -b / (2.0 * c);
    }
    for r in roots {
        if r > 0.0 && r < n {
            let v = g(r);
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    (lo, hi)
}

/// The forward differences of one cubic Hermite piece over `n >= 1`
/// outputs from `step0` (slope `slope0`) to `end` (slope `slope1`), with
/// `d1` solved last so the step after `n` outputs lands within `n / 2`
/// units of `end` (the endpoint rule). Any input returns a result; none
/// panics.
fn piece_differences(
    step0: u128,
    slope0: f64,
    end: u128,
    slope1: f64,
    n: usize,
) -> Result<[i128; 3], PlanError> {
    let nf = n as f64;
    let steep = SLOPE_LIMIT / nf;
    if !(slope0.abs() <= steep && slope1.abs() <= steep) {
        return Err(PlanError::BadSlope); // also NaN and infinities
    }
    let dv = end as i128 - step0 as i128;
    let dvf = dv as f64;
    // v(k) = v0 + b k + c k² + d k³, units of 2^-64.
    let b = slope0 * ONE_F;
    let m1 = slope1 * ONE_F;
    let c = (3.0 * dvf - (2.0 * b + m1) * nf) / (nf * nf);
    let d = (-2.0 * dvf + (b + m1) * nf) / (nf * nf * nf);
    let (lo, hi) = cubic_range(step0 as f64 / ONE_F, b, c, d, nf);
    let (min, max) = (Step::MIN.to_f64(), Step::MAX.to_f64());
    if !(lo >= min * (1.0 - RANGE_TOL) && hi <= max * (1.0 + RANGE_TOL)) {
        return Err(PlanError::StepOutOfRange { min: lo, max: hi });
    }
    // The ideal forward differences at k = 0.
    let (x3, x2) = (6.0 * d, 2.0 * c + 6.0 * d);
    let d3 = round_i128(x3).ok_or(PlanError::Overflow)?;
    // d3's rounding error e3 adds e3 k(k-n)(k+n-3)/6 by the end-solve of
    // d1; moving e3 (n/2 - 1) out of d2 cancels it at the middle and leaves
    // e3 k(k-n)(2k-n)/12, about 8 times less.
    let e3 = d3 as f64 - x3;
    let d2 = round_i128(x2 - e3 * (nf / 2.0 - 1.0)).ok_or(PlanError::Overflow)?;
    let solve = || -> Option<i128> {
        let k = i128::try_from(n).ok()?;
        let c2 = k.checked_mul(k - 1)? / 2;
        let c3 = c2.checked_mul(k - 2)? / 3;
        let rest = dv
            .checked_sub(c2.checked_mul(d2)?)?
            .checked_sub(c3.checked_mul(d3)?)?;
        div_round(rest, k)
    };
    let d1 = solve().ok_or(PlanError::Overflow)?;
    Ok([d1, d2, d3])
}

/// The exact position arithmetic and the pull accounting of the core,
/// without the samples (the core runs one; tests and schedulers can run
/// one alone).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accumulator {
    /// Position of the next output, 64.64, counted from the first retained
    /// frame (the first frame after the kernel's history).
    pos: u128,
    /// The step at the next output.
    step: u128,
    /// Received frames at or after the next output's newest frame.
    retained: usize,
    /// Frames before the first retained one, since the last reset
    /// (wrapping after 2⁶⁴ frames, some 95 000 years at 6.144 MHz).
    base: u64,
    pending: Option<Planned>,
}

impl Accumulator {
    pub fn new(step: Step) -> Self {
        Self {
            pos: 0,
            step: step.bits(),
            retained: 0,
            base: 0,
            pending: None,
        }
    }

    /// Back to the state of [`Self::new`].
    pub fn reset(&mut self, step: Step) {
        *self = Self::new(step);
    }

    /// The step at the next output.
    pub fn step(&self) -> Step {
        Step(self.step)
    }

    /// Absolute position of the next output, 64.64 input frames counted
    /// from the first input frame after the last reset (modulo 2⁶⁴
    /// frames, like [`Self::frames_in`]).
    pub fn position(&self) -> u128 {
        ((self.base as u128) << 64).wrapping_add(self.pos)
    }

    /// Received frames at or after the next output's newest frame.
    pub fn retained(&self) -> usize {
        self.retained
    }

    /// Input frames received since the last reset, modulo 2⁶⁴.
    pub fn frames_in(&self) -> u64 {
        self.base.wrapping_add(self.retained as u64)
    }

    /// The latched call, if a `plan` is pending.
    pub fn planned(&self) -> Option<Planned> {
        self.pending
    }

    /// Latch `seg` for a call of `n` outputs and return the new input
    /// frames it needs (see the module docs). Committed state is untouched.
    pub fn plan(&mut self, n: usize, seg: Segment) -> Result<usize, PlanError> {
        self.pending = None;
        if n > MAX_CALL_OUTPUTS {
            return Err(PlanError::TooManyOutputs {
                n,
                max: MAX_CALL_OUTPUTS,
            });
        }
        if n == 0 {
            self.pending = Some(Planned {
                outputs: 0,
                need: 0,
                step: self.step,
                d: [0; 3],
                end: self.step,
                next: self.pos,
            });
            return Ok(0);
        }
        let (step, d, end) = match seg {
            Segment::Hold => (self.step, [0; 3], self.step),
            Segment::Jump(s) => (s.bits(), [0; 3], s.bits()),
            Segment::Piece {
                slope0,
                end,
                slope1,
            } => (
                self.step,
                piece_differences(self.step, slope0, end.bits(), slope1, n)?,
                end.bits(),
            ),
        };
        let last = position_after(self.pos, step, d, n - 1).ok_or(PlanError::Overflow)?;
        let next = position_after(self.pos, step, d, n).ok_or(PlanError::Overflow)?;
        let need = usize::try_from((last >> 64) + 1)
            .map_err(|_| PlanError::Overflow)?
            .saturating_sub(self.retained);
        self.pending = Some(Planned {
            outputs: n,
            need,
            step,
            d,
            end,
            next,
        });
        Ok(need)
    }

    fn check_input(planned: &Planned, n_in: usize) {
        let over = if planned.need > 0 { MAX_OVERPULL } else { 0 };
        assert!(
            n_in >= planned.need && n_in <= planned.need + over,
            "{n_in} input frames for a plan that needs {} (over-pull at most {over})",
            planned.need
        );
    }

    /// Commit the latched call with `n_in` input frames delivered. Returns
    /// how many frames left the window (the core shifts its history by
    /// that much). Panics without a pending plan or with a wrong `n_in`.
    pub fn commit(&mut self, n_in: usize) -> usize {
        let planned = self.pending.take().expect("commit without a plan");
        Self::check_input(&planned, n_in);
        if planned.outputs == 0 {
            return 0;
        }
        let next = planned.next;
        let avail = self.retained + n_in;
        // `next >> 64` fits (plan checked `need` from the same positions).
        let shift = usize::try_from(next >> 64).map_or(avail, |l| l.min(avail));
        self.pos = next - ((shift as u128) << 64);
        self.retained = avail - shift;
        debug_assert!(self.retained <= MAX_RETAINED);
        self.base = self.base.wrapping_add(shift as u64);
        self.step = planned.end;
        shift
    }
}

/// What the table measured when it was built: the effective kernel (the
/// table and the cubic interpolation, sampled 8 times finer than the table)
/// against the continuous kernel, relative to DC.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TableReport {
    /// Largest passband (`f <= PASSBAND`) deviation of the effective
    /// kernel, dB.
    pub passband_ripple_db: f64,
    /// Largest level of the effective kernel at `f >= STOPBAND`, dB.
    pub stopband_db: f64,
    /// The same for the continuous kernel.
    pub exact_stopband_db: f64,
    /// Largest `|H_eff - H_exact|` at any frequency, dB re DC.
    pub interpolation_error_db: f64,
}

/// The core's kernel table: a Kaiser-windowed sinc (cutoff
/// `(PASSBAND + STOPBAND) / 2`, Kaiser's length for [`TARGET_DB`], β for
/// [`TARGET_DB`] + [`DESIGN_MARGIN_DB`]) sampled at `PHASES` phases per
/// input frame, stored as `PHASES + 3` reversed rows of `taps`
/// coefficients: row `r` holds the phase `μ = (r - 1) / PHASES`, with
/// `row[taps - 1 - j] = h(j + μ - delay)`. Phase `φ` interpolates rows
/// `φ ..= φ + 3`.
pub struct CoreTable {
    taps: usize,
    beta: f64,
    /// Half the kernel's support, input frames.
    half: f64,
    /// Cutoff, cycles per input frame.
    fc: f64,
    i0_beta: f64,
    rows: Vec<f64>,
    report: TableReport,
}

impl CoreTable {
    /// The table every core shares: built and measured once per process.
    pub fn shared() -> Result<Arc<CoreTable>, String> {
        static TABLE: OnceLock<Result<Arc<CoreTable>, String>> = OnceLock::new();
        TABLE.get_or_init(|| Self::build().map(Arc::new)).clone()
    }

    /// Design, tabulate and measure the table; refused if the measurement
    /// misses the targets (stopband [`TARGET_DB`], interpolation error
    /// [`INTERPOLATION_MARGIN_DB`] below that, passband deviation
    /// [`MAX_PASSBAND_RIPPLE_DB`]).
    pub fn build() -> Result<CoreTable, String> {
        let width = STOPBAND - PASSBAND;
        // Kaiser's length for the target: the support in input frames.
        let support = ((TARGET_DB - 7.95) / (14.36 * width)).ceil() as usize + 1;
        // The cubic reads the rows on both sides of each phase, so one
        // more tap than the support covers every row.
        let taps = support + 1;
        let beta = kaiser_beta_for_attenuation(TARGET_DB + DESIGN_MARGIN_DB);
        let mut table = CoreTable {
            taps,
            beta,
            half: support as f64 / 2.0,
            fc: 0.5 * (PASSBAND + STOPBAND),
            i0_beta: bessel_i0(beta),
            rows: Vec::new(),
            report: TableReport {
                passband_ripple_db: f64::NAN,
                stopband_db: f64::NAN,
                exact_stopband_db: f64::NAN,
                interpolation_error_db: f64::NAN,
            },
        };
        table.rows = tabulate(taps, beta, table.fc)?;
        table.report = table.measure();
        crate::design::safety::check_interpolation_table(
            &table.report,
            TARGET_DB,
            INTERPOLATION_MARGIN_DB,
            MAX_PASSBAND_RIPPLE_DB,
        )
        .map_err(|e| e.to_string())?;
        Ok(table)
    }

    /// Taps per output (per channel, per row).
    pub fn taps(&self) -> usize {
        self.taps
    }

    /// The kernel's delay in input frames: an output at position `τ`
    /// estimates the input at `τ - delay`.
    pub fn delay(&self) -> f64 {
        (self.taps - 1) as f64 / 2.0
    }

    pub fn beta(&self) -> f64 {
        self.beta
    }

    pub fn report(&self) -> TableReport {
        self.report
    }

    /// The continuous kernel at `t` input frames from its centre.
    pub fn kernel(&self, t: f64) -> f64 {
        if t.abs() >= self.half {
            return 0.0;
        }
        let u = t / self.half;
        let w = bessel_i0(self.beta * (1.0 - u * u).sqrt()) / self.i0_beta;
        let x = 2.0 * self.fc * t;
        let s = if x == 0.0 {
            1.0
        } else if x.fract() == 0.0 {
            // The zero crossings exactly (sin(πx) rounds otherwise), so
            // the integer phase is an exact delay.
            0.0
        } else {
            (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
        };
        2.0 * self.fc * s * w
    }

    /// The continuous kernel's (zero-phase) transfer function at `f`
    /// cycles per input frame: what every phase approximates.
    pub fn response(&self, f: f64) -> f64 {
        const STEPS: usize = 4096;
        let m = (self.half * STEPS as f64) as i64;
        let w = 2.0 * std::f64::consts::PI * f;
        (-m..=m)
            .map(|i| {
                let t = i as f64 / STEPS as f64;
                self.kernel(t) * (w * t).cos()
            })
            .sum::<f64>()
            / STEPS as f64
    }

    /// The four rows phase `ph` interpolates.
    #[inline]
    fn rows_at(&self, ph: usize) -> &[f64] {
        &self.rows[ph * self.taps..(ph + 4) * self.taps]
    }

    fn measure(&self) -> TableReport {
        use realfft::RealFftPlanner;
        const Q: usize = 8;
        let t = self.taps;
        let pq = PHASES * Q;
        let len = t * pq;
        let delay = self.delay();
        let mut eff = vec![0.0; len];
        let mut exact = vec![0.0; len];
        for k in 0..pq {
            // Cell centres: no sample lands on a table point.
            let f = (k as f64 + 0.5) / pq as f64;
            let w = weights(((k % Q) as f64 + 0.5) / Q as f64);
            let rows = self.rows_at(k / Q);
            for j in 0..t {
                let i = t - 1 - j;
                eff[j * pq + k] = w[0] * rows[i]
                    + w[1] * rows[t + i]
                    + w[2] * rows[2 * t + i]
                    + w[3] * rows[3 * t + i];
                exact[j * pq + k] = self.kernel(j as f64 + f - delay);
            }
        }
        let n = (4 * len).next_power_of_two();
        let fft = RealFftPlanner::<f64>::new().plan_fft_forward(n);
        let spectrum = |x: &[f64]| -> Vec<f64> {
            let mut input = fft.make_input_vec();
            input[..x.len()].copy_from_slice(x);
            let mut out = fft.make_output_vec();
            fft.process(&mut input, &mut out)
                .expect("buffer sizes come from the plan");
            out.iter().map(|c| c.norm()).collect()
        };
        let diff: Vec<f64> = eff.iter().zip(&exact).map(|(a, b)| a - b).collect();
        let (se, sx, sd) = (spectrum(&eff), spectrum(&exact), spectrum(&diff));
        let freq = |b: usize| b as f64 * pq as f64 / n as f64;
        let db = |x: f64| 20.0 * x.max(1e-300).log10();
        let mut report = TableReport {
            passband_ripple_db: 0.0,
            stopband_db: f64::NEG_INFINITY,
            exact_stopband_db: f64::NEG_INFINITY,
            interpolation_error_db: f64::NEG_INFINITY,
        };
        for b in 0..se.len() {
            let f = freq(b);
            if f <= PASSBAND {
                report.passband_ripple_db = report.passband_ripple_db.max(db(se[b] / se[0]).abs());
            }
            if f >= STOPBAND {
                report.stopband_db = report.stopband_db.max(db(se[b] / se[0]));
                report.exact_stopband_db = report.exact_stopband_db.max(db(sx[b] / sx[0]));
            }
            report.interpolation_error_db = report.interpolation_error_db.max(db(sd[b] / sx[0]));
        }
        report
    }
}

/// The table rows for `taps` taps per row: the `standard` path's Kaiser
/// windowed sinc (β `beta`, cutoff `fc` cycles per input frame) at the integer
/// grid ratio [`PHASES`], linear
/// phase.
///
/// The path scales its kernel so the branches sum to 1 on average; the table is
/// scaled instead so the centre tap is exactly 1, and the other taps of the
/// centre branch (the sinc's zeros) are exactly 0. So phase 0 is an exact unit
/// impulse, and every row samples the same continuous kernel
/// ([`CoreTable::kernel`]); the path's own scale would leave phase 0 off the
/// other phases by the windowed sinc's DC deviation.
///
/// Row `r`, tap `taps - 1 - j` is grid tap `j · PHASES + r - 1` (the upper
/// guard rows' tap at `j = -1` is left out, see the module docs).
fn tabulate(taps: usize, beta: f64, fc: f64) -> Result<Vec<f64>, String> {
    use crate::resample::{UpsamplerDesign, try_design_kernel};
    // The cutoff as a fraction of the input Nyquist.
    let design = UpsamplerDesign::linear(PHASES, (taps - 1) * PHASES + 1, beta, 2.0 * fc);
    let mut h = try_design_kernel(&design).map_err(|e| format!("adaptive core table: {e}"))?;
    let c = (h.len() - 1) / 2;
    let centre = h[c];
    if !(centre.is_finite() && centre > 0.0) {
        return Err(format!("adaptive core table: centre tap {centre}"));
    }
    for v in &mut h {
        *v /= centre;
    }
    h[c] = 1.0;
    for k in (PHASES..=c).step_by(PHASES) {
        h[c - k] = 0.0;
        h[c + k] = 0.0;
    }
    let mut rows = vec![0.0; (PHASES + 3) * taps];
    for (r, row) in rows.chunks_exact_mut(taps).enumerate() {
        for j in 0..taps {
            if let Some(&v) = (j * PHASES + r).checked_sub(1).and_then(|n| h.get(n)) {
                row[taps - 1 - j] = v;
            }
        }
    }
    Ok(rows)
}

/// The 4-point Lagrange weights at `μ` in `[0, 1)` for the points
/// `-1, 0, 1, 2`.
#[inline]
fn weights(mu: f64) -> [f64; 4] {
    let (a, b, c) = (mu + 1.0, mu - 1.0, mu - 2.0);
    [
        -mu * b * c / 6.0,
        a * b * c / 2.0,
        -a * mu * c / 2.0,
        a * mu * b / 6.0,
    ]
}

/// Why a core was not built.
#[derive(Clone, Debug, PartialEq)]
pub enum CoreError {
    /// The shared table failed its measurement.
    Table(String),
    /// `channels` must be `1..=MAX_CHANNELS`.
    Channels(usize),
    /// `max_out` must be `1..=MAX_CALL_OUTPUTS`.
    MaxOut(usize),
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Table(e) => f.write_str(e),
            Self::Channels(c) => write!(f, "{c} channels, must be 1 to {MAX_CHANNELS}"),
            Self::MaxOut(n) => {
                write!(f, "{n} outputs per call, must be 1 to {MAX_CALL_OUTPUTS}")
            }
        }
    }
}

impl std::error::Error for CoreError {}

/// The adaptive resampling core (see the module docs).
pub struct AdaptiveCore {
    table: Arc<CoreTable>,
    taps: usize,
    channels: usize,
    max_out: usize,
    max_in: usize,
    /// Per channel: `taps - 1` history frames, then the retained and new
    /// frames (room for `MAX_RETAINED + max_in`).
    hist: Vec<Vec<f64>>,
    /// Window index of the frame `delay` before the newest: at phase 0 the
    /// output is that frame, copied (so the sign of zero survives).
    centre: usize,
    acc: Accumulator,
    kernels: Kernels,
}

impl AdaptiveCore {
    /// A core for `channels` channels (`1..=MAX_CHANNELS`) and calls of
    /// `1..=max_out` outputs (`max_out` in `1..=MAX_CALL_OUTPUTS`), starting
    /// at `step` with zero history. Uses the shared table.
    pub fn new(channels: usize, max_out: usize, step: Step) -> Result<Self, CoreError> {
        let table = CoreTable::shared().map_err(CoreError::Table)?;
        Self::with_table(table, channels, max_out, step)
    }

    /// [`Self::new`] with a given table.
    pub fn with_table(
        table: Arc<CoreTable>,
        channels: usize,
        max_out: usize,
        step: Step,
    ) -> Result<Self, CoreError> {
        if !(1..=MAX_CHANNELS).contains(&channels) {
            return Err(CoreError::Channels(channels));
        }
        if !(1..=MAX_CALL_OUTPUTS).contains(&max_out) {
            return Err(CoreError::MaxOut(max_out));
        }
        let taps = table.taps();
        // The next output sits less than 3 frames past the first retained
        // one, and each later one at most 2 (1 + RANGE_TOL) frames, plus the
        // trajectory bound (below 1e-9 per output), further: `need <= 2n +
        // 3` for n <= MAX_CALL_OUTPUTS, plus the over-pull; one frame of
        // slack on top.
        let max_in = 2 * max_out + 5;
        let hist_len = taps - 1 + MAX_RETAINED + max_in;
        Ok(Self {
            centre: taps - 1 - (taps - 1) / 2,
            table,
            taps,
            channels,
            max_out,
            max_in,
            hist: (0..channels).map(|_| vec![0.0; hist_len]).collect(),
            acc: Accumulator::new(step),
            kernels: Kernels::detect(),
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Most outputs per call.
    pub fn max_out(&self) -> usize {
        self.max_out
    }

    /// Most input frames one call can take (`need` plus the over-pull).
    pub fn max_in(&self) -> usize {
        self.max_in
    }

    pub fn table(&self) -> &Arc<CoreTable> {
        &self.table
    }

    /// The kernel's delay in input frames.
    pub fn delay(&self) -> f64 {
        self.table.delay()
    }

    /// The position arithmetic and pull accounting.
    pub fn accumulator(&self) -> &Accumulator {
        &self.acc
    }

    /// The step at the next output.
    pub fn step(&self) -> Step {
        self.acc.step()
    }

    /// Received frames at or after the next output's newest frame.
    pub fn retained(&self) -> usize {
        self.acc.retained()
    }

    /// The latched call, if a `plan` is pending.
    pub fn planned(&self) -> Option<Planned> {
        self.acc.planned()
    }

    /// Clear the history and start again at `step`, position 0, nothing
    /// retained, nothing pending.
    pub fn reset(&mut self, step: Step) {
        for h in &mut self.hist {
            h.fill(0.0);
        }
        self.acc.reset(step);
    }

    /// Latch `seg` for a call of `n` outputs and return the new input
    /// frames it needs. On an error nothing is pending and nothing changed.
    pub fn plan(&mut self, n: usize, seg: Segment) -> Result<usize, PlanError> {
        if n > self.max_out {
            self.acc.pending = None;
            return Err(PlanError::TooManyOutputs {
                n,
                max: self.max_out,
            });
        }
        let need = self.acc.plan(n, seg)?;
        if need + MAX_OVERPULL > self.max_in {
            // A guard: in-range segments never get here (see `with_table`).
            self.acc.pending = None;
            return Err(PlanError::Overflow);
        }
        Ok(need)
    }

    /// Run the latched call: `input[c]` holds the `need` frames the plan
    /// asked for (or one more when `need > 0`), `output[c]` receives the
    /// plan's `n` outputs. Panics without a pending plan or with wrong
    /// lengths (caller bugs).
    pub fn process(&mut self, input: &[&[f64]], output: &mut [&mut [f64]]) {
        let planned = self.acc.planned().expect("process without a plan");
        assert_eq!(input.len(), self.channels, "input channel count mismatch");
        assert_eq!(output.len(), self.channels, "output channel count mismatch");
        let n_in = input.first().map_or(0, |x| x.len());
        let n = planned.outputs;
        Accumulator::check_input(&planned, n_in);
        for c in 0..self.channels {
            assert_eq!(input[c].len(), n_in, "inconsistent input slice length");
            assert_eq!(output[c].len(), n, "output length must be the planned n");
        }
        if n == 0 {
            self.acc.commit(n_in);
            return;
        }
        let t = self.taps;
        let off = t - 1 + self.acc.retained;
        for (h, x) in self.hist.iter_mut().zip(input) {
            h[off..off + n_in].copy_from_slice(x);
        }
        let avail = self.acc.retained + n_in;

        let mut pos = self.acc.pos;
        let mut step = planned.step as i128;
        let [mut d1, mut d2, d3] = planned.d;
        let k = self.kernels;
        let table = &*self.table;
        for j in 0..n {
            let lead = (pos >> 64) as usize;
            debug_assert!(lead < avail, "output {j}: newest frame {lead} not received");
            let frac = pos as u64;
            let ph = (frac >> (64 - LOG2_PHASES)) as usize;
            let mu = ((frac << LOG2_PHASES) >> 11) as f64 * (1.0 / (1u64 << 53) as f64);
            let w = weights(mu);
            let rows = table.rows_at(ph);
            if frac >> 11 == 0 {
                // Phase 0, μ = 0: the weights are (-0, 1, 0, 0) and that
                // row is a unit impulse, so the output is the centre frame.
                // Copying it equals the sum except for the sign of zero.
                for (h, out) in self.hist.iter().zip(output.iter_mut()) {
                    out[j] = h[lead + self.centre];
                }
            } else if self.channels == 2 {
                let (a, b) = (&self.hist[0], &self.hist[1]);
                let (ya, yb) = (k.dot4x2)(rows, t, &w, &a[lead..lead + t], &b[lead..lead + t]);
                output[0][j] = ya;
                output[1][j] = yb;
            } else {
                for (h, out) in self.hist.iter().zip(output.iter_mut()) {
                    out[j] = (k.dot4)(rows, t, &w, &h[lead..lead + t]);
                }
            }
            debug_assert!(step > 0);
            pos += step as u128;
            step += d1;
            d1 += d2;
            d2 += d3;
        }
        // The endpoint rule: the step lands within n / 2 units of the end,
        // and is set to it.
        debug_assert!((step - planned.end as i128).unsigned_abs() <= (n as u128).div_ceil(2));
        let shift = self.acc.commit(n_in);
        debug_assert_eq!(pos - ((shift as u128) << 64), self.acc.pos);
        let keep = t - 1 + avail - shift;
        for h in &mut self.hist {
            h.copy_within(shift..shift + keep, 0);
        }
    }
}

#[cfg(test)]
mod tests;
