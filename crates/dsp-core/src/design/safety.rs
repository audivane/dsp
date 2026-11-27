//! The post-build safety gate: [`super::prepare`] runs it on every design
//! before a runtime is allocated, and refuses a design that fails it. Gains
//! are measured against unity before headroom (headroom is applied after the
//! gate).
//!
//! **Kernel kinds**, per polyphase branch (the filter one output phase
//! applies at the input rate, so its response is the gain that phase can
//! emit; on a rational grid every branch is reachable): every tap finite,
//! the kernel not all zeros; DC within ±0.01 dB (±0.1 dB once a phase
//! transform has moved it); no boost over +1 dB, the largest branch
//! magnitude sampled on an FFT of 8 points per tap and at least 2^16 plus
//! the analytic bound on peaks between bins (`(Δω/2) Σ|j - c||h_j|`, or the
//! tighter `(Δω²/8) Σ(j - c)²|h_j|`), the FFT doubled up to
//! [`budget::MAX_GATE_FFT`] when only the bound is over; the bounded-input
//! peak (the largest branch L1 norm, what a full-scale input can produce)
//! reported ([`Safety::l1_db`]), not refused; the delay the path's
//! convention (the centre for linear phase, the largest tap otherwise).
//!
//! **Runtime kinds** (cascade, interpolator, block FFT): the same checks on
//! the runtime's response to an impulse at input frame 0, per output
//! phase, run offline on a private one-channel instance.
//!
//! **The `standard` path** (the Custom builder) builds every design it
//! always has: its DC, boost and delay results are measured and reported,
//! and only a non-finite or all-zero kernel is refused (its boost is
//! measured on the first FFT, not refined by doubling). Its designs span
//! continuous parameters, so they cannot be exempted one by one like the
//! presets (a Tukey design at cutoff 1.0 rings +1.4 dB near Nyquist); the
//! new combinations of the other paths get the whole gate.
//!
//! **Legacy exemptions** ([`EXEMPTIONS`]): the presets the engine has
//! always built that fail a check (the block FFT's boost at every integer
//! ratio, the shortest half-band's DC at rational ratios), measured over
//! every preset at every source rate and every ratio a rate ladder can ask
//! for. An exemption, keyed by the catalogue preset (the design must be
//! exactly the catalogue's), the kind of ratio and the check, allows the
//! measured worst plus a small margin. A design edited away from the
//! catalogue, or a construction that measures worse (another C library's
//! `sin` and `cos` move the block FFT's window and twiddles by an ulp), is
//! gated in full. The construction hash ([`construction_hash`]) is
//! reported and keys the fingerprints
//! (`tests/fixtures/filter-safety-exemptions.tsv`), not the exemptions.
//!
//! Measurements are cached per construction (kernels) or per design and
//! context (runtime kinds); the verdict is applied on every call.

use super::{ArithmeticPath, DesignError, FilterDesign, Kernel, KernelDesign, Phase, budget};
use crate::presets::{FilterPreset, UpsamplerRuntime};
use realfft::RealFftPlanner;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// +1 dB.
const MAX_BOOST: f64 = 1.122_018_454_301_963_3;

/// A check of the gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Check {
    Finite,
    Nonzero,
    Dc,
    Boost,
    Delay,
}

impl Check {
    pub fn name(self) -> &'static str {
        match self {
            Self::Finite => "finite",
            Self::Nonzero => "nonzero",
            Self::Dc => "dc",
            Self::Boost => "boost",
            Self::Delay => "delay",
        }
    }
}

/// A legacy exemption: preset `preset` (its catalogue design exactly), at
/// integer ratios (`integer`) or at the others, may fail `check` by up to
/// `max_db` (the DC deviation, or the boost bound): the worst measured over
/// every source rate and ladder output rate, plus a margin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Exemption {
    pub preset: FilterPreset,
    pub integer: bool,
    pub check: Check,
    pub max_db: f64,
}

/// The legacy exemptions (the measured values: the fixture
/// `tests/fixtures/filter-safety-exemptions.tsv` for the tier B matrix,
/// and the ladder-only rational pairs).
pub const EXEMPTIONS: &[Exemption] = &[
    // The overlap-save block FFT's window ripple: +4.879 dB (measured to
    // within 2e-7 dB over every ratio and block position).
    Exemption {
        preset: FilterPreset::BlockFft,
        integer: true,
        check: Check::Boost,
        max_db: 4.9,
    },
    Exemption {
        preset: FilterPreset::BlockFftLarge,
        integer: true,
        check: Check::Boost,
        max_db: 4.9,
    },
    // The short-hop geometry: +6.048 dB.
    Exemption {
        preset: FilterPreset::BlockFftFast,
        integer: true,
        check: Check::Boost,
        max_db: 6.07,
    },
    // The 31-tap half-band's branches on a rational grid.
    Exemption {
        preset: FilterPreset::HalfbandTiny,
        integer: false,
        check: Check::Dc,
        max_db: HALFBAND_TINY_DC_DB,
    },
];

/// The largest DC deviation [`FilterPreset::HalfbandTiny`] may have at a
/// rational ratio.
const HALFBAND_TINY_DC_DB: f64 = 0.05;

/// The catalogue preset `d` is, if any.
fn preset_of(d: &FilterDesign) -> Option<FilterPreset> {
    FilterPreset::ALL
        .iter()
        .copied()
        .find(|&p| super::catalogue::design(p) == *d)
}

/// Whether `value_db` failing `check` is a legacy exemption of `d` at
/// `ratio`.
fn exempt(
    d: &FilterDesign,
    ratio: audivane_protocol::RateRatio,
    check: Check,
    value_db: f64,
) -> bool {
    // The block FFT's window ripple is the method's own, at every
    // geometry: a geometry no preset has keeps its preset's bound (the
    // property test measures the new ones).
    if let FilterDesign::BlockFft(b) = d
        && check == Check::Boost
        && ratio.is_integer()
    {
        let p = match b.overlap {
            super::Overlap::QuarterAverage => FilterPreset::BlockFft,
            super::Overlap::Half => FilterPreset::BlockFftFast,
        };
        return EXEMPTIONS
            .iter()
            .any(|e| e.preset == p && e.check == Check::Boost && value_db <= e.max_db);
    }
    let Some(p) = preset_of(d) else {
        return false;
    };
    EXEMPTIONS.iter().any(|e| {
        e.preset == p && e.integer == ratio.is_integer() && e.check == check && value_db <= e.max_db
    })
}

/// What the gate measured.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Safety {
    /// Branches (kernel kinds) or output phases (runtime kinds).
    pub branches: usize,
    /// Largest |DC gain| deviation of a branch from 0 dB.
    pub dc_dev_db: f64,
    /// The DC tolerance applied.
    pub dc_tol_db: f64,
    /// Largest sampled branch magnitude, dB.
    pub boost_sampled_db: f64,
    /// Its guaranteed bound, dB (held to +1 dB).
    pub boost_bound_db: f64,
    /// Largest FFT used.
    pub nfft: usize,
    /// Bounded-input peak: the largest branch L1 norm, dB.
    pub l1_db: f64,
    /// The construction's hash ([`construction_hash`]).
    pub hash: u64,
}

impl Safety {
    /// Nothing measured: a unity ratio has no upsampler.
    pub fn unity() -> Self {
        Self {
            branches: 1,
            dc_dev_db: 0.0,
            dc_tol_db: 0.01,
            boost_sampled_db: 0.0,
            boost_bound_db: 0.0,
            nfft: 0,
            l1_db: 0.0,
            hash: 0,
        }
    }
}

/// FNV-1a over the words of a runtime constructor's record, as the
/// fingerprint tests hash them: per record its tag, its length and its
/// words.
pub fn construction_hash(records: &[(&str, &[u64])]) -> u64 {
    let tag = |t: &str| crate::bypass::hash_u64(t.bytes().map(u64::from));
    crate::bypass::hash_u64(records.iter().flat_map(|&(t, words)| {
        [tag(t), words.len() as u64]
            .into_iter()
            .chain(words.iter().copied())
    }))
}

/// The hash of the runtime `k` is constructed into at `ratio` with chain
/// blocks of `max_block` (the words `Upsampler` and `RationalResampler`
/// record).
pub fn kernel_hash(k: &Kernel, ratio: audivane_protocol::RateRatio, max_block: usize) -> u64 {
    let head: [u64; 4] = if ratio.is_integer() {
        [
            u64::from(ratio.up),
            k.delay as u64,
            budget::engine_block(max_block) as u64,
            k.taps.len() as u64,
        ]
    } else {
        [
            u64::from(ratio.up),
            u64::from(ratio.down),
            k.delay_exact.to_bits(),
            k.taps.len() as u64,
        ]
    };
    let tag = if ratio.is_integer() {
        "upsampler"
    } else {
        "rational"
    };
    let tagh = crate::bypass::hash_u64(tag.bytes().map(u64::from));
    crate::bypass::hash_u64(
        [tagh, (head.len() + k.taps.len()) as u64]
            .into_iter()
            .chain(head)
            .chain(k.taps.iter().map(|v| v.to_bits())),
    )
}

// ------------------------------------------------------------- measurement

/// What the gate measures of one construction (cached).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Measured {
    branches: usize,
    finite: bool,
    nonzero: bool,
    dc_dev_db: f64,
    boost_sampled_db: f64,
    boost_bound_db: f64,
    nfft: usize,
    l1_db: f64,
    hash: u64,
}

/// The gate's running state.
struct Gate {
    planner: RealFftPlanner<f64>,
    /// Whether the FFT is doubled while only the analytic bound is over
    /// +1 dB (where the boost is enforced; a measured-only boost keeps the
    /// first FFT, as [`budget::gate_workspace`] charges it).
    doubling: bool,
    finite: bool,
    nonzero: bool,
    dc_dev_db: f64,
    boost_sampled_db: f64,
    boost_bound_db: f64,
    nfft: usize,
    l1_db: f64,
}

fn db(v: f64) -> f64 {
    20.0 * v.abs().log10()
}

impl Gate {
    fn new(doubling: bool) -> Self {
        Self {
            planner: RealFftPlanner::new(),
            doubling,
            finite: true,
            nonzero: false,
            dc_dev_db: 0.0,
            boost_sampled_db: f64::NEG_INFINITY,
            boost_bound_db: f64::NEG_INFINITY,
            nfft: 0,
            l1_db: f64::NEG_INFINITY,
        }
    }

    /// Largest |H(w)| of `h` sampled on `nfft` points.
    fn sampled_max(&mut self, h: &[f64], nfft: usize) -> f64 {
        let fft = self.planner.plan_fft_forward(nfft);
        let mut x = fft.make_input_vec();
        x[..h.len()].copy_from_slice(h);
        let mut spec = fft.make_output_vec();
        // Lengths match the plan by construction.
        if fft.process(&mut x, &mut spec).is_err() {
            return f64::INFINITY;
        }
        spec.iter().map(|c| c.norm()).fold(0.0, f64::max)
    }

    /// Fold one branch (or output phase) into the gate.
    fn branch(&mut self, h: &[f64]) {
        let dc: f64 = h.iter().sum();
        self.dc_dev_db = self.dc_dev_db.max(db(dc).abs());
        let l1: f64 = h.iter().map(|v| v.abs()).sum();
        self.l1_db = self.l1_db.max(db(l1));
        // Zeros at both ends do not change |H|.
        let Some(first) = h.iter().position(|&v| v != 0.0) else {
            return;
        };
        let last = h.iter().rposition(|&v| v != 0.0).unwrap_or(first);
        let h = &h[first..=last];
        // Moments around the weighted median (any centre is valid; this
        // one makes the first moment smallest).
        let half = l1 / 2.0;
        let mut acc = 0.0;
        let c = h
            .iter()
            .position(|v| {
                acc += v.abs();
                acc >= half
            })
            .unwrap_or(0) as f64;
        let (m1, m2) = h.iter().enumerate().fold((0.0, 0.0), |(m1, m2), (j, v)| {
            let d = (j as f64 - c).abs();
            (m1 + d * v.abs(), m2 + d * d * v.abs())
        });
        let mut nfft = budget::gate_fft(h.len());
        loop {
            let max = self.sampled_max(h, nfft);
            // Between two bins Δω = 2π/nfft apart, |H| exceeds the larger
            // sampled value by at most (Δω/2) Σ|j - c||h_j| (first
            // derivative) and by at most (Δω²/8) Σ(j - c)²|h_j| (the error
            // of the complex linear interpolation). Both are guaranteed.
            let dw = 2.0 * std::f64::consts::PI / nfft as f64;
            let bound = max + (dw / 2.0 * m1).min(dw * dw / 8.0 * m2);
            if bound <= MAX_BOOST
                || max > MAX_BOOST
                || nfft >= budget::MAX_GATE_FFT
                || !self.doubling
            {
                self.boost_sampled_db = self.boost_sampled_db.max(db(max));
                self.boost_bound_db = self.boost_bound_db.max(db(bound));
                self.nfft = self.nfft.max(nfft);
                return;
            }
            nfft *= 2;
        }
    }

    /// The gate over the `branches` polyphase branches of `h`.
    fn branches(&mut self, h: &[f64], branches: usize) {
        self.finite &= h.iter().all(|v| v.is_finite());
        self.nonzero |= h.iter().any(|&v| v != 0.0);
        if !self.finite {
            return;
        }
        let branches = branches.max(1);
        let mut branch = Vec::with_capacity(h.len() / branches + 1);
        for b in 0..branches {
            branch.clear();
            branch.extend(h.iter().skip(b).step_by(branches));
            self.branch(&branch);
        }
    }

    /// The measurements of a construction of `branches` branches.
    fn measured(&self, branches: usize, hash: u64) -> Measured {
        Measured {
            branches,
            finite: self.finite,
            nonzero: self.nonzero,
            dc_dev_db: self.dc_dev_db,
            boost_sampled_db: self.boost_sampled_db,
            boost_bound_db: self.boost_bound_db,
            nfft: self.nfft,
            l1_db: self.l1_db,
            hash,
        }
    }
}

/// How a verdict is reached.
#[derive(Clone, Copy)]
struct Policy<'a> {
    /// The DC tolerance, dB.
    dc_tol_db: f64,
    /// Whether the delay is the path's convention.
    delay_ok: bool,
    /// The `standard` path: DC, boost and delay measured only.
    custom: bool,
    /// The design and ratio, for the legacy exemptions.
    design: Option<&'a FilterDesign>,
    ratio: audivane_protocol::RateRatio,
}

impl Measured {
    /// The verdict: the measurements, or the first failed check that
    /// `policy` neither measures only nor exempts.
    fn verdict(&self, policy: Policy) -> Result<Safety, DesignError> {
        let fail = |check: Check, value_db: f64, what: String| -> Result<(), DesignError> {
            let measured_only =
                policy.custom && matches!(check, Check::Dc | Check::Boost | Check::Delay);
            let exempted = policy
                .design
                .is_some_and(|d| exempt(d, policy.ratio, check, value_db));
            if measured_only || exempted {
                Ok(())
            } else {
                Err(DesignError::Unsafe(format!(
                    "the design fails the safety check ({}): {what}",
                    check.name()
                )))
            }
        };
        let dc_tol_db = policy.dc_tol_db;
        if !self.finite {
            fail(
                Check::Finite,
                f64::INFINITY,
                "it has non-finite taps".into(),
            )?;
        }
        if !self.nonzero {
            fail(Check::Nonzero, f64::INFINITY, "it is all zeros".into())?;
        }
        if self.finite && self.dc_dev_db > dc_tol_db {
            fail(
                Check::Dc,
                self.dc_dev_db,
                format!(
                    "a polyphase branch's DC gain is {:.3} dB off unity, more than {dc_tol_db} dB",
                    self.dc_dev_db
                ),
            )?;
        }
        if self.finite && self.boost_bound_db > db(MAX_BOOST) {
            fail(
                Check::Boost,
                self.boost_bound_db,
                format!(
                    "a polyphase branch can boost by up to {:.2} dB, more than 1 dB",
                    self.boost_bound_db
                ),
            )?;
        }
        if !policy.delay_ok {
            fail(
                Check::Delay,
                f64::INFINITY,
                "the delay it reports is not the kernel's".into(),
            )?;
        }
        Ok(Safety {
            branches: self.branches,
            dc_dev_db: self.dc_dev_db,
            dc_tol_db,
            boost_sampled_db: self.boost_sampled_db,
            boost_bound_db: self.boost_bound_db,
            nfft: self.nfft,
            l1_db: self.l1_db,
            hash: self.hash,
        })
    }
}

/// The measurements so far, by construction. A construction that cannot
/// be built is not cached (its error is the build's).
type Measurements = Mutex<HashMap<u64, Measured>>;

/// Most measurements kept (one is a few dozen bytes).
const CACHE_CAPACITY: usize = 4096;

fn cache() -> &'static Measurements {
    static CACHE: OnceLock<Measurements> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached(
    key: u64,
    f: impl FnOnce() -> Result<Measured, DesignError>,
) -> Result<Measured, DesignError> {
    if let Some(m) = cache().lock().ok().and_then(|c| c.get(&key).copied()) {
        return Ok(m);
    }
    let m = f()?;
    if let Ok(mut c) = cache().lock() {
        if c.len() >= CACHE_CAPACITY {
            c.clear();
        }
        c.insert(key, m);
    }
    Ok(m)
}

/// Index of the first largest |value|.
fn peak_index(h: &[f64]) -> usize {
    let mut best = 0;
    for (i, v) in h.iter().enumerate() {
        if v.abs() > h[best].abs() {
            best = i;
        }
    }
    best
}

/// Whether `k`'s delay is its design's convention: the centre for linear
/// phase (`floor(N / 2)` on the preset paths; `floor((N - 1) / 2)`, and
/// exactly `(N - 1) / 2` on a rational grid, on the `standard` path), the
/// first largest tap otherwise.
fn delay_ok(d: &KernelDesign, k: &Kernel, integer: bool) -> bool {
    let n = k.taps.len();
    let standard = d.numerics.path == ArithmeticPath::Standard;
    match d.phase {
        Phase::Linear if standard => {
            k.delay == n.saturating_sub(1) / 2
                && (integer || k.delay_exact == (n as f64 - 1.0) / 2.0)
        }
        Phase::Linear => k.delay == n / 2 && k.delay_exact == k.delay as f64,
        _ => k.delay == peak_index(&k.taps) && k.delay_exact == k.delay as f64,
    }
}

/// The gate on kernel `k` of design `d` (`kd` its kernel design) in `cx`
/// (see the module docs).
pub fn check_kernel(
    d: &FilterDesign,
    kd: &KernelDesign,
    k: &Kernel,
    cx: &super::BuildContext,
) -> Result<Safety, DesignError> {
    let hash = kernel_hash(k, cx.ratio, cx.max_block);
    let branches = cx.ratio.up.max(1) as usize;
    // The Custom designs are measured only; the new combinations of the
    // standard path get the whole gate.
    let custom = kd.numerics.path == ArithmeticPath::Standard && !super::novel(kd);
    let key = crate::bypass::hash_u64([hash, u64::from(custom)]);
    let m = cached(key, || {
        let mut g = Gate::new(!custom);
        g.branches(&k.taps, branches);
        Ok(g.measured(branches, hash))
    })?;
    m.verdict(Policy {
        dc_tol_db: dc_tol_db(kd.phase),
        delay_ok: delay_ok(kd, k, cx.ratio.is_integer()),
        custom,
        design: Some(d),
        ratio: cx.ratio,
    })
}

/// The gate on the varispeed core's interpolation table: its effective
/// kernel (the table with its cubic interpolation, `r`) must measure a
/// stopband at or below `-target_db`, an interpolation error at or below
/// `-(target_db + interpolation_margin_db)` and a passband deviation at
/// most `max_ripple_db`. A measurement that is not a number fails.
pub fn check_interpolation_table(
    r: &crate::adaptive::TableReport,
    target_db: f64,
    interpolation_margin_db: f64,
    max_ripple_db: f64,
) -> Result<(), DesignError> {
    let ok = r.stopband_db <= -target_db
        && r.interpolation_error_db <= -(target_db + interpolation_margin_db)
        && r.passband_ripple_db <= max_ripple_db;
    if ok {
        return Ok(());
    }
    Err(DesignError::Unsafe(format!(
        "adaptive core table misses its targets: stopband {:.1} dB (target -{target_db}), \
         interpolation error {:.1} dB (target -{}), passband {:.2e} dB (at most {max_ripple_db:e})",
        r.stopband_db,
        r.interpolation_error_db,
        target_db + interpolation_margin_db,
        r.passband_ripple_db
    )))
}

/// The gate on the runtime kind `d` in `cx`: the response of a private
/// one-channel instance to an impulse at input frame 0, per output phase.
/// `construct` builds the instance.
pub(crate) fn check_runtime(
    d: &FilterDesign,
    cx: &super::BuildContext,
    construct: impl FnOnce() -> Result<Box<dyn UpsamplerRuntime>, DesignError>,
) -> Result<Safety, DesignError> {
    let key = crate::bypass::hash_u64(
        format!("{d:?} {} {} {}", cx.source_rate, cx.ratio, cx.max_block)
            .bytes()
            .map(u64::from),
    );
    let branches = cx.ratio.up.max(1) as usize;
    let m = cached(key, || {
        let (rt, records) = crate::test_support::capture(construct);
        let mut rt = rt?;
        let words: Vec<(&str, &[u64])> = records
            .iter()
            .map(|r| (r.tag, r.words.as_slice()))
            .collect();
        let hash = construction_hash(&words);
        let h = impulse_response(rt.as_mut(), cx.max_block);
        let mut g = Gate::new(true);
        g.branches(&h, branches);
        Ok(g.measured(branches, hash))
    })?;
    m.verdict(Policy {
        dc_tol_db: 0.01,
        delay_ok: true,
        custom: false,
        design: Some(d),
        ratio: cx.ratio,
    })
}

/// Channel 0's response of `rt` (from its start) to a unit impulse at
/// input frame 0, over its impulse span, at most `max_block` input frames
/// per call.
fn impulse_response(rt: &mut dyn UpsamplerRuntime, max_block: usize) -> Vec<f64> {
    let r = rt.ratio();
    let (l, m) = (r.up as usize, r.down as usize);
    let span = rt.impulse_span_out_frames();
    let frames = (span * m).div_ceil(l) + 1;
    let block = max_block.max(1);
    let mut out = Vec::with_capacity(span + 2 * block * l);
    let mut input = vec![0.0; block];
    let mut buf = Vec::new();
    let mut pos = 0;
    while pos < frames {
        let n = block.min(frames - pos);
        input[..n].fill(0.0);
        if pos == 0 {
            input[0] = 1.0;
        }
        buf.clear();
        buf.resize(rt.output_frames(n), 0.0);
        rt.process(&[&input[..n]], &mut [buf.as_mut_slice()]);
        out.extend_from_slice(&buf);
        pos += n;
    }
    out.truncate(span);
    out
}

/// The DC tolerance of a novel design's branches, dB: 0.01 at linear
/// phase, 0.1 transformed.
pub(crate) fn dc_tol_db(phase: Phase) -> f64 {
    if phase == Phase::Linear { 0.01 } else { 0.1 }
}

/// The largest DC deviation from unity of the `branches` polyphase
/// branches of `h`, dB (what the gate's DC check measures).
pub(crate) fn branch_dc_dev_db(h: &[f64], branches: usize) -> f64 {
    let branches = branches.max(1);
    (0..branches)
        .map(|b| db(h.iter().skip(b).step_by(branches).sum::<f64>()).abs())
        .fold(0.0, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    use audivane_protocol::RateRatio;

    /// Every exemption the fixture records (tier B's matrix) is covered by
    /// a row of the table, within its bound, and every row covers one.
    #[test]
    fn exemptions_cover_the_fixtures() {
        let fixture = include_str!("../../tests/fixtures/filter-safety-exemptions.tsv");
        let mut worst = vec![f64::NEG_INFINITY; EXEMPTIONS.len()];
        for l in fixture
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
        {
            // taps_hash, check, value, first case (name rate up/down).
            let c: Vec<&str> = l.split('\t').collect();
            let value: f64 = c[2].parse().unwrap();
            let case: Vec<&str> = c[3].split(' ').collect();
            let preset = FilterPreset::from_name(case[0]).unwrap();
            let (up, down) = case[2].split_once('/').unwrap();
            let ratio = RateRatio::new(up.parse().unwrap(), down.parse().unwrap());
            let row = EXEMPTIONS.iter().position(|e| {
                e.preset == preset
                    && e.integer == ratio.is_integer()
                    && e.check.name() == c[1]
                    && value <= e.max_db
            });
            let row = row.unwrap_or_else(|| panic!("{l}: no exemption covers it"));
            worst[row] = worst[row].max(value);
        }
        for (e, worst) in EXEMPTIONS.iter().zip(worst) {
            // Every row covers a recorded case, and its margin over the
            // worst recorded one is small.
            assert!(worst.is_finite(), "{e:?} covers no recorded case");
            assert!(e.max_db - worst < 0.025, "{e:?}: worst recorded {worst}");
        }
    }

    fn kernel_gate(taps: Vec<f64>, branches: usize) -> Measured {
        let mut g = Gate::new(true);
        g.branches(&taps, branches);
        g.measured(branches, 0)
    }

    fn strict(ratio: RateRatio) -> Policy<'static> {
        Policy {
            dc_tol_db: 0.01,
            delay_ok: true,
            custom: false,
            design: None,
            ratio,
        }
    }

    #[test]
    fn refusals() {
        let two = RateRatio::integer(2);
        // A clean two-branch kernel: an impulse per branch.
        let g = kernel_gate(vec![0.0, 1.0, 1.0, 0.0], 2);
        assert!(g.verdict(strict(two)).is_ok());
        // DC off by 10%.
        let g = kernel_gate(vec![0.0, 1.1, 1.0, 0.0], 2);
        let e = g.verdict(strict(two)).unwrap_err();
        assert!(e.to_string().contains("(dc)"), "{e}");
        // A boost: unity DC, but +6 dB at Nyquist.
        let g = kernel_gate(vec![1.0, -1.0, 1.0], 1);
        let e = g.verdict(strict(two)).unwrap_err();
        assert!(e.to_string().contains("(boost)"), "{e}");
        // Non-finite, all zeros.
        let g = kernel_gate(vec![f64::NAN, 1.0], 1);
        assert!(g.verdict(strict(two)).is_err());
        let g = kernel_gate(vec![0.0; 4], 2);
        assert!(g.verdict(strict(two)).is_err());
        // The standard path's legacy designs: measured, refused only when
        // broken.
        let custom = Policy {
            custom: true,
            delay_ok: false,
            ..strict(two)
        };
        let g = kernel_gate(vec![1.0, -1.0, 1.1], 1);
        assert!(g.verdict(custom).is_ok());
        // The same measurement (as the cache holds it) under the strict
        // policy is refused: the verdict is not cached.
        assert!(g.verdict(strict(two)).is_err());
        let g = kernel_gate(vec![f64::NAN, 1.0], 1);
        assert!(g.verdict(custom).is_err());
    }

    /// An exemption covers its preset (the catalogue design exactly), its
    /// kind of ratio and its check, up to its bound.
    #[test]
    fn exemptions_are_keyed_by_preset_ratio_and_bound() {
        let tiny = super::super::catalogue::design(FilterPreset::HalfbandTiny);
        let rational = RateRatio::new(160, 147);
        // DC 0.03 dB off: two branches, one at 1, one 0.35% high.
        let g = kernel_gate(vec![1.0, 1.0035], 2);
        assert!(g.dc_dev_db > 0.02 && g.dc_dev_db < HALFBAND_TINY_DC_DB);
        let policy = |design, ratio| Policy {
            design: Some(design),
            ..strict(ratio)
        };
        assert!(g.verdict(policy(&tiny, rational)).is_ok());
        // Not at an integer ratio, not for another design, not past the
        // bound.
        assert!(g.verdict(policy(&tiny, RateRatio::integer(2))).is_err());
        let other = super::super::catalogue::design(FilterPreset::HalfbandSmall);
        assert!(g.verdict(policy(&other, rational)).is_err());
        let worse = kernel_gate(vec![1.0, 1.01], 2);
        assert!(worse.verdict(policy(&tiny, rational)).is_err());
        // A boost is not a DC exemption.
        let boost = kernel_gate(vec![1.0, -1.0, 1.0], 1);
        assert!(boost.verdict(policy(&tiny, rational)).is_err());
        // The block FFT's boost, within its bound only.
        let fft = super::super::catalogue::design(FilterPreset::BlockFft);
        let m = Measured {
            boost_bound_db: 4.88,
            ..kernel_gate(vec![0.0, 1.0, 1.0, 0.0], 2)
        };
        assert!(m.verdict(policy(&fft, RateRatio::integer(2))).is_ok());
        let m = Measured {
            boost_bound_db: 5.5,
            ..m
        };
        assert!(m.verdict(policy(&fft, RateRatio::integer(2))).is_err());
    }
}
