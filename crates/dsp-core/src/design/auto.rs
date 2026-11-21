//! Offline, measured automatic FIR design. No functions here run on the audio thread.
//!
//! The `auto` length rule of the `standard` path: the shortest measured
//! length meeting an accuracy budget on the band edges.
use crate::design::paths::standard::ManualConfig;
use crate::design::paths::standard::{UpsamplerDesign, design_ratio, try_design_kernel_for};
use crate::resample::band_design_for;
use audivane_protocol::{FilterErrorBudget, FilterPhase, RateRatio, WindowKind};
use realfft::RealFftPlanner;
use rustfft::num_complex::Complex64;
use std::collections::HashMap;
use std::f64::consts::PI;
use std::sync::{Mutex, OnceLock};

pub const AUTO_MAX_TAPS: usize = 4_194_304;
/// Bounded search in whole input-sample half spans (odd, centred kernels).
pub const AUTO_MAX_HALF_SPANS: usize = 2048;

/// Largest relative amplitude error in the protected bands. Frequencies are
/// cycles per intermediate-rate sample; gain is the interpolation numerator.
/// Measured on a 32x FFT grid plus exact band edges.
pub fn protected_error(kernel: &[f64], gain: f64, pass: f64, stop: f64) -> f64 {
    protected_error_with(&mut RealFftPlanner::new(), kernel, gain, pass, stop)
}

/// [`protected_error`] with a caller-owned planner. A search measures many
/// kernels of similar length and reuses the plans (twiddles, up to ~100 MB at
/// the longest lengths); they are dropped with the planner when the search
/// ends instead of living on in the calling thread.
pub fn protected_error_with(
    planner: &mut RealFftPlanner<f64>,
    kernel: &[f64],
    gain: f64,
    pass: f64,
    stop: f64,
) -> f64 {
    let n = (kernel.len().max(2) * 32).next_power_of_two();
    protected_error_n(planner, kernel, gain, pass, stop, n)
}

/// [`protected_error_with`] on an `n`-point FFT grid.
fn protected_error_n(
    planner: &mut RealFftPlanner<f64>,
    kernel: &[f64],
    gain: f64,
    pass: f64,
    stop: f64,
    n: usize,
) -> f64 {
    scan_n(planner, kernel, gain, pass, stop, n, &mut |_, _| {})
}

/// [`protected_error_n`], each grid point's error in the protected bands
/// also given to `sink` as `(frequency, error)` (the band-edge points
/// are not).
fn scan_n(
    planner: &mut RealFftPlanner<f64>,
    kernel: &[f64],
    gain: f64,
    pass: f64,
    stop: f64,
    n: usize,
    sink: &mut impl FnMut(f64, f64),
) -> f64 {
    let fft = planner.plan_fft_forward(n);
    let mut time = fft.make_input_vec();
    time[..kernel.len()].copy_from_slice(kernel);
    let mut spec = fft.make_output_vec();
    fft.process(&mut time, &mut spec)
        .expect("offline response FFT");
    let mut error = 0.0_f64;
    for (k, c) in spec.iter().enumerate() {
        let f = k as f64 / n as f64;
        let a = c.norm() / gain;
        if f <= pass {
            error = error.max((a - 1.0).abs());
            sink(f, (a - 1.0).abs());
        }
        if f >= stop {
            error = error.max(a);
            sink(f, a);
        }
    }
    for (f, target) in [(pass, 1.0), (stop, 0.0)] {
        let step = Complex64::from_polar(1.0, -2.0 * PI * f);
        let value = kernel
            .iter()
            .rev()
            .fold(Complex64::new(0.0, 0.0), |a, &h| a * step + h);
        error = error.max((value.norm() / gain - target).abs());
    }
    error
}

/// Largest FFT the real-grid check of a rational design uses (2^23
/// points, 64 MB of input). Kernels longer than 2^23 / 32 taps are
/// measured in several FFTs of this size ([`protected_error_bounded`]).
pub const AUTO_GRID_MAX_FFT: usize = 1 << 23;

/// [`protected_error_with`] (32 points per tap) with FFTs of at most
/// [`AUTO_GRID_MAX_FFT`] points. A longer kernel is measured on `r = 32 /
/// m` interleaved grids of `n = (len m)` (rounded up to a power of two)
/// points: grid `s` is the `n`-point grid shifted by `s / (r n)`, so
/// together they are exactly the `r n` points [`protected_error_with`]
/// uses and the result is the same measurement, not an estimate with a
/// margin. Every point is a true value of the response, so once the error
/// exceeds `bound` the remaining grids are skipped (the error can only
/// grow): a failing kernel usually costs one FFT, a passing one all `r`.
/// The second return is whether the measurement is complete: a skipped one
/// is a lower bound of the error only (always above `bound`).
pub fn protected_error_bounded(
    planner: &mut RealFftPlanner<f64>,
    kernel: &[f64],
    gain: f64,
    pass: f64,
    stop: f64,
    bound: f64,
) -> (f64, bool) {
    let len = kernel.len().max(2);
    if (len * 32).next_power_of_two() <= AUTO_GRID_MAX_FFT {
        return (
            protected_error_with(planner, kernel, gain, pass, stop),
            true,
        );
    }
    let mut m = 16;
    while m > 1 && (len * m).next_power_of_two() > AUTO_GRID_MAX_FFT {
        m /= 2;
    }
    let n = (len * m).next_power_of_two();
    let r = 32 / m;
    let mut error = protected_error_n(planner, kernel, gain, pass, stop, n);
    for shift in 1..r {
        if error > bound {
            return (error, false);
        }
        error = error.max(protected_error_shifted(
            planner, kernel, gain, pass, stop, n, shift, r,
        ));
    }
    (error, true)
}

/// A candidate of the real-grid check measured against the pass `bound`
/// and the `best` (exact) error so far: the error, and whether it is
/// exact. A partial measurement is a lower bound, which decides "worse"
/// when it is already at least `best`; below `best` the measurement is
/// completed, so the best is always exact.
fn measure_ranked(
    planner: &mut RealFftPlanner<f64>,
    kernel: &[f64],
    gain: f64,
    pass: f64,
    stop: f64,
    bound: f64,
    best: f64,
) -> (f64, bool) {
    let (err, complete) = protected_error_bounded(planner, kernel, gain, pass, stop, bound);
    if complete || err >= best {
        return (err, complete);
    }
    protected_error_bounded(planner, kernel, gain, pass, stop, f64::INFINITY)
}

/// [`protected_error_n`] on the `n`-point grid shifted by `shift / (r n)`
/// (the band-edge points are left to the unshifted grid). The response at
/// `k / n + d` is the DFT of `h[t] e^(-2 pi i d t)`: with `a = h cos` and
/// `b = h sin` (both real), it is `A[k] - i B[k]`.
#[allow(clippy::too_many_arguments)]
fn protected_error_shifted(
    planner: &mut RealFftPlanner<f64>,
    kernel: &[f64],
    gain: f64,
    pass: f64,
    stop: f64,
    n: usize,
    shift: usize,
    r: usize,
) -> f64 {
    scan_shifted(
        planner,
        kernel,
        gain,
        pass,
        stop,
        n,
        shift,
        r,
        &mut |_, _| {},
    )
}

/// [`protected_error_shifted`] with each point given to `sink` (as
/// [`scan_n`]).
#[allow(clippy::too_many_arguments)]
fn scan_shifted(
    planner: &mut RealFftPlanner<f64>,
    kernel: &[f64],
    gain: f64,
    pass: f64,
    stop: f64,
    n: usize,
    shift: usize,
    r: usize,
    sink: &mut impl FnMut(f64, f64),
) -> f64 {
    let fft = planner.plan_fft_forward(n);
    let w = 2.0 * PI * shift as f64 / (r * n) as f64;
    let mut time = fft.make_input_vec();
    let mut a = fft.make_output_vec();
    let mut b = fft.make_output_vec();
    for (part, out) in [(0, &mut a), (1, &mut b)] {
        time.fill(0.0);
        for (t, (x, &h)) in time.iter_mut().zip(kernel).enumerate() {
            let (sin, cos) = (w * t as f64).sin_cos();
            *x = h * if part == 0 { cos } else { sin };
        }
        fft.process(&mut time, out).expect("offline response FFT");
    }
    let offset = shift as f64 / r as f64;
    let mut error = 0.0_f64;
    for (k, (ak, bk)) in a.iter().zip(&b).enumerate() {
        let f = (k as f64 + offset) / n as f64;
        if f > 0.5 {
            break;
        }
        // A - iB.
        let value = Complex64::new(ak.re + bk.im, ak.im - bk.re);
        let amp = value.norm() / gain;
        if f <= pass {
            error = error.max((amp - 1.0).abs());
            sink(f, (amp - 1.0).abs());
        }
        if f >= stop {
            error = error.max(amp);
            sink(f, amp);
        }
    }
    error
}

/// [`protected_error_bounded`] that also gives every grid point of the
/// protected bands to `sink` as `(frequency, error)`, and returns the grid
/// spacing (`1 / (32 points per tap, rounded up to a power of two)`)
/// beside the error and its completeness. The grids and their order are
/// [`protected_error_bounded`]'s, so the error is the same.
pub(crate) fn protected_error_scan(
    planner: &mut RealFftPlanner<f64>,
    kernel: &[f64],
    gain: f64,
    pass: f64,
    stop: f64,
    bound: f64,
    sink: &mut impl FnMut(f64, f64),
) -> (f64, bool, f64) {
    let len = kernel.len().max(2);
    let full = (len * 32).next_power_of_two();
    if full <= AUTO_GRID_MAX_FFT {
        let e = scan_n(planner, kernel, gain, pass, stop, full, sink);
        return (e, true, 1.0 / full as f64);
    }
    let mut m = 16;
    while m > 1 && (len * m).next_power_of_two() > AUTO_GRID_MAX_FFT {
        m /= 2;
    }
    let n = (len * m).next_power_of_two();
    let r = 32 / m;
    let step = 1.0 / (r * n) as f64;
    let mut error = scan_n(planner, kernel, gain, pass, stop, n, sink);
    for shift in 1..r {
        if error > bound {
            return (error, false, step);
        }
        error = error.max(scan_shifted(
            planner, kernel, gain, pass, stop, n, shift, r, sink,
        ));
    }
    (error, true, step)
}

/// Beta margin over the error budget: the window is designed for `A +
/// margin` dB so that its sidelobe floor sits clearly below the budget and
/// the length search, not the window, sets the final error.
pub const AUTO_BETA_MARGIN_DB: f64 = 6.0;

/// Measurement margin: a candidate passes when its measured protected error
/// is at least this far below the budget.
pub const AUTO_MEASURE_MARGIN_DB: f64 = 0.05;

/// Most linear-prototype measurements one search may make.
pub const AUTO_MAX_LINEAR_STEPS: usize = 48;

/// Most phase-converted candidates tried above the shortest linear length.
pub const AUTO_MAX_PHASE_STEPS: usize = 8;

/// Phase conversions that may fail (as errors, not as a missed budget)
/// before the search gives up.
pub const AUTO_MAX_PHASE_FAILURES: usize = 3;

/// The search starts at this fraction of the window-formula length estimate
/// ([`band_design_for`] at `A + margin`), per window. Measured shortest
/// lengths sit at 0.68-0.82 of the estimate for Kaiser and 0.95-1.0 for
/// Gaussian; starting just below keeps the bracket a few percent wide.
pub fn auto_start_fraction(window: WindowKind) -> f64 {
    match window {
        WindowKind::Gaussian => 0.94,
        _ => 0.76,
    }
}

/// Geometric bracketing step around the start (a few percent of the length).
pub const AUTO_BRACKET_STEP: f64 = 1.04;

/// The budget in dB of a resolved automatic-taps config. `match_output`
/// must be resolved with [`ManualConfig::with_output`] first: it is an
/// error here, never a guess.
pub fn automatic_target(cfg: &ManualConfig) -> Result<f64, String> {
    let a = match cfg.auto_taps {
        Some(FilterErrorBudget::Db(a)) => a,
        Some(FilterErrorBudget::MatchOutput) => {
            return Err(
                "\"match_output\" must be resolved against the output settings before design"
                    .into(),
            );
        }
        None => return Err("no error budget".into()),
    };
    if !a.is_finite() || !(55.0..=200.0).contains(&a) {
        return Err(format!(
            "error budget must be between 55 and 200 dB (got {a})"
        ));
    }
    if cfg.window == WindowKind::Tukey || cfg.preserve_samples {
        return Err("automatic taps needs Kaiser or Gaussian without preserve_samples".into());
    }
    let mut band = cfg.band;
    band.attenuation_db = a + AUTO_BETA_MARGIN_DB;
    // Validate edges, including finite values, without allocating a kernel.
    band_design_for(cfg.window, &band, 1).map_err(|e| e.to_string())?;
    Ok(a)
}

/// What a ratio means for the search: the kernel's passband gain, its
/// grid, and the protected band edges in cycles per intermediate-rate
/// sample. All ratio handling of the search and its cache goes through
/// here.
///
/// At an integer ratio `L` both the gain and the grid scale are `L`. At a
/// rational `up/down` they differ: the gain is `up` (each of the `up`
/// polyphase branches sums to 1), while the band edges are fractions of
/// the lower of the two Nyquists, `max(up, down)` grid samples per sample
/// at the lower rate ([`design_ratio`]). 44.1 -> 48 kHz (160/147) and
/// 48 -> 44.1 kHz (147/160) share the scale 160 but not the gain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DesignGrid {
    /// Passband gain (`up`).
    pub gain: usize,
    /// Grid samples per lower-rate sample (`max(up, down)`): kernels are
    /// `2 k scale + 1` taps for `k` lower-rate half spans.
    pub scale: usize,
    pub pass: f64,
    pub stop: f64,
}

impl DesignGrid {
    /// The grid of the integer ratio `ratio`.
    pub fn new(cfg: &ManualConfig, ratio: usize) -> Self {
        Self::for_ratio(cfg, RateRatio::integer(ratio.max(1) as u32))
    }

    /// The grid of any ratio.
    pub fn for_ratio(cfg: &ManualConfig, ratio: RateRatio) -> Self {
        let scale = design_ratio(ratio).max(1);
        let edge = 2.0 * scale as f64;
        Self {
            gain: (ratio.up as usize).max(1),
            scale,
            pass: cfg.band.passband_end / edge,
            stop: cfg.band.stopband_start / edge,
        }
    }

    /// Kernel length for `k` lower-rate half spans.
    pub fn taps(&self, k: usize) -> usize {
        2 * k * self.scale + 1
    }

    /// Largest half-span count `k` the search may use.
    pub fn max_half_spans(&self) -> usize {
        AUTO_MAX_HALF_SPANS.min((AUTO_MAX_TAPS - 1) / (2 * self.scale))
    }
}

/// Largest grid scale a rational search measures on its own grid (x16,
/// the engine's largest integer ratio). Above it the half-span count comes
/// from the x16 search, which the integer validation runs anyway, and the
/// kernel is built with it on the real grid. Near the band both are the
/// same windowed sinc, and a finer grid only moves the images further out
/// (less aliasing), so the x16 measurement is the conservative one, while a
/// search on a 160x grid would measure FFTs ten times longer per step.
pub const AUTO_EXACT_MAX_SCALE: usize = 16;

/// Cheap feasibility check: the window-formula length estimate at `ratio`,
/// in taps. Errors when the estimate is past the search limits, so the
/// config can be rejected before any kernel is designed.
pub fn automatic_estimate(cfg: &ManualConfig, ratio: usize) -> Result<usize, String> {
    if !(1..=64).contains(&ratio) {
        return Err("design ratio must be in 1..=64".into());
    }
    automatic_estimate_for(cfg, RateRatio::integer(ratio as u32))
}

/// [`automatic_estimate`] at any ratio, in taps on the ratio's grid
/// ([`DesignGrid::for_ratio`]). Integer ratios must be in `1..=64`, a
/// rational one's terms at most 4096. A rational refusal names the grid
/// scale, not the ratio: the ratios with one scale share the cached result,
/// and the caller adds the ratio.
pub fn automatic_estimate_for(cfg: &ManualConfig, ratio: RateRatio) -> Result<usize, String> {
    let a = automatic_target(cfg)?;
    let ok = if ratio.is_integer() {
        (1..=64).contains(&ratio.up)
    } else {
        ratio.up.max(ratio.down) <= 4096
    };
    if !ok {
        let what = if ratio.is_integer() {
            format!("ratio {ratio}")
        } else {
            format!("the {}x grid", ratio.up.max(ratio.down))
        };
        return Err(format!(
            "{what} is outside the automatic design's range (x1..x64, or rational terms up \
             to 4096)"
        ));
    }
    let grid = DesignGrid::for_ratio(cfg, ratio);
    let at = if ratio.is_integer() {
        format!("at {ratio}")
    } else {
        format!("on the {}x grid", grid.scale)
    };
    let mut band = cfg.band;
    band.attenuation_db = a + AUTO_BETA_MARGIN_DB;
    let estimate = band_design_for(cfg.window, &band, grid.scale)
        .map_err(|e| e.to_string())?
        .taps;
    let max = grid.taps(grid.max_half_spans());
    if !ratio.is_unity() && estimate > max {
        return Err(format!(
            "{a:.2} dB over this band needs about {estimate} taps {at}, more than the \
             {max}-tap automatic limit; widen the transition or lower the target"
        ));
    }
    Ok(estimate)
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct CacheKey {
    window: WindowKind,
    phase: FilterPhase,
    phase_mix: u64,
    /// The configured band edges (f64 bits) and the requested ratio (both
    /// terms: the grid scale alone does not fix the gain, see
    /// [`DesignGrid`]).
    pass: u64,
    stop: u64,
    up: u32,
    down: u32,
    target: u64,
}

/// Entries are small (design parameters, not kernels). One validation adds
/// about 20 integer ratios and a few dozen small-scale rational ones (larger
/// scales are derived from x16 and not stored), so the cache holds several
/// configs' worth and evicts the least recently used entry, never everything
/// at once.
const CACHE_CAPACITY: usize = 1024;

/// A search's result and the peak workspace it was charged
/// ([`search_workspace`]).
type Searched = (Result<UpsamplerDesign, String>, u64);

thread_local! {
    /// The largest workspace charged on this thread by the search running
    /// on it ([`crate::design::budget::check_candidate`] and
    /// [`crate::design::budget::check_measurement`] note it).
    static SEARCH_PEAK: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Note `bytes` as charged by the running search.
pub(crate) fn note_search_workspace(bytes: u64) {
    SEARCH_PEAK.with(|p| p.set(p.get().max(bytes)));
}

/// Run `f` (a search) and the workspace it charged, the search peak of an
/// enclosing search kept.
fn charged<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let outer = SEARCH_PEAK.with(|p| p.replace(0));
    let r = f();
    let peak = SEARCH_PEAK.with(|p| p.replace(outer));
    note_search_workspace(peak);
    (r, peak)
}

#[derive(Default)]
struct Cache {
    entries: HashMap<CacheKey, (u64, Searched)>,
    clock: u64,
}

impl Cache {
    fn get(&mut self, key: &CacheKey) -> Option<Searched> {
        self.clock += 1;
        let clock = self.clock;
        self.entries.get_mut(key).map(|(used, value)| {
            *used = clock;
            value.clone()
        })
    }

    fn insert(&mut self, key: CacheKey, value: Searched) {
        self.clock += 1;
        if self.entries.len() >= CACHE_CAPACITY && !self.entries.contains_key(&key) {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, (used, _))| *used)
                .map(|(k, _)| *k);
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(key, (self.clock, value));
    }
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

/// Shortest member of the odd centred family N = 2 k L + 1 meeting the
/// measured budget, including phase conversion. Results (and failures) are
/// cached per process by everything the design depends on: the window,
/// phase (and mix), the band edges as fractions of the intermediate rate,
/// the kernel grid and the budget. The sample rate does not enter, so
/// configs that resolve to the same ratio (A/B sides with the same filter,
/// fallback rebuilds) share one search; each distinct ratio is its own
/// entry. Fields the design ignores (`phase_mix` outside intermediate phase,
/// `taper`) are normalised in the cached value.
pub fn automatic_design(cfg: &ManualConfig, ratio: usize) -> Result<UpsamplerDesign, String> {
    if !(1..=64).contains(&ratio) {
        return Err("design ratio must be in 1..=64".into());
    }
    automatic_design_for(cfg, RateRatio::integer(ratio as u32))
}

/// [`automatic_design`] at any ratio. Searches (integer ratios, rational
/// ones up to [`AUTO_EXACT_MAX_SCALE`]) are cached by both terms of the
/// ratio; larger rational scales are derived from the x16 search.
/// At a rational ratio the design is on the ratio's grid
/// ([`UpsamplerDesign::try_for_ratio`]: `ratio` is the grid scale, `taps`
/// the grid length; [`try_design_kernel_for`] gives the kernel).
pub fn automatic_design_for(
    cfg: &ManualConfig,
    ratio: RateRatio,
) -> Result<UpsamplerDesign, String> {
    searched_for(cfg, ratio).0
}

/// The largest construction workspace the search of [`automatic_design_for`]
/// at `ratio` is charged (each candidate's design and measurement, with the
/// plan the search still holds), cached with its result: what a build that
/// searches cold needs beside its final kernel. 0 for a design without an
/// automatic length.
pub fn search_workspace(cfg: &ManualConfig, ratio: RateRatio) -> u64 {
    if cfg.auto_taps.is_none() {
        return 0;
    }
    searched_for(cfg, ratio).1
}

/// [`automatic_design_for`] and the search's workspace.
fn searched_for(cfg: &ManualConfig, ratio: RateRatio) -> Searched {
    #[cfg(test)]
    count_search();
    let a = match automatic_target(cfg) {
        Ok(a) => a,
        Err(e) => return (Err(e), 0),
    };
    let cfg = &normalised(cfg);
    if derived(ratio) && cfg.phase == FilterPhase::Linear {
        // Linear phase: the x16 half-span count is the real grid's shortest
        // (the x16 grid only aliases more). Cheap, so not stored; its
        // workspace is the x16 search's.
        let proxy = searched_for(cfg, RateRatio::integer(AUTO_EXACT_MAX_SCALE as u32)).1;
        return (derived_design(cfg, ratio, a).map(|(d, _)| d), proxy);
    }
    let key = CacheKey {
        window: cfg.window,
        phase: cfg.phase,
        phase_mix: cfg.phase_mix.to_bits(),
        pass: cfg.band.passband_end.to_bits(),
        stop: cfg.band.stopband_start.to_bits(),
        up: ratio.up,
        down: ratio.down,
        target: a.to_bits(),
    };
    if let Some(hit) = cache().lock().ok().and_then(|mut c| c.get(&key)) {
        return hit;
    }
    let result = if derived(ratio) {
        // Ratios with the same grid scale (160/147 and 147/160, or
        // 640/147 and 147/640) have the same design and, before the gain
        // `up`, the same kernel and error: one real-grid check serves all
        // of them, kept under a per-scale entry (`down` 0) besides each
        // ratio's own.
        let scale_key = CacheKey {
            up: design_ratio(ratio) as u32,
            down: 0,
            ..key
        };
        match cache().lock().ok().and_then(|mut c| c.get(&scale_key)) {
            Some(hit) => hit,
            None => {
                let (result, peak) = charged(|| verified_on_grid(cfg, ratio, a));
                // It starts from the x16 search's length.
                let proxy = searched_for(cfg, RateRatio::integer(AUTO_EXACT_MAX_SCALE as u32)).1;
                let result = (result, peak.max(proxy));
                if let Ok(mut c) = cache().lock() {
                    c.insert(scale_key, result.clone());
                }
                result
            }
        }
    } else {
        charged(|| automatic_design_uncached_for(cfg, ratio))
    };
    if let Ok(mut c) = cache().lock() {
        c.insert(key, result.clone());
    }
    result
}

/// Whether `ratio` takes its half spans from the x16 search
/// ([`AUTO_EXACT_MAX_SCALE`]).
fn derived(ratio: RateRatio) -> bool {
    !ratio.is_integer() && design_ratio(ratio) > AUTO_EXACT_MAX_SCALE
}

/// The design at `ratio` with the x16 search's half-span count `k` (also
/// returned), before any check on the real grid. `cfg` is normalised and
/// `a` its budget.
fn derived_design(
    cfg: &ManualConfig,
    ratio: RateRatio,
    a: f64,
) -> Result<(UpsamplerDesign, usize), String> {
    automatic_estimate_for(cfg, ratio)?;
    let proxy = automatic_design_for(cfg, RateRatio::integer(AUTO_EXACT_MAX_SCALE as u32))?;
    let k = (proxy.taps - 1) / (2 * AUTO_EXACT_MAX_SCALE);
    let grid = DesignGrid::for_ratio(cfg, ratio);
    if k > grid.max_half_spans() {
        return Err(over_limit(a, grid, k));
    }
    let mut manual = cfg.clone();
    manual.auto_taps = None;
    manual.cutoff_mode = audivane_protocol::CutoffMode::Band;
    manual.band.attenuation_db = a + AUTO_BETA_MARGIN_DB;
    let d = UpsamplerDesign::try_for_ratio(&manual, ratio).map_err(|e| e.to_string())?;
    Ok((
        UpsamplerDesign {
            taps: grid.taps(k),
            ..d
        },
        k,
    ))
}

/// The refusal of a rational design over the length limit. It names the
/// grid scale, not the ratio: ratios with one scale share the cached
/// result (160/147 and 147/160), and the caller adds the ratio.
fn over_limit(a: f64, grid: DesignGrid, k: usize) -> String {
    format!(
        "{a:.2} dB over this band needs {} taps on the {}x grid, more than the {}-tap \
         automatic limit; widen the transition or lower the target",
        grid.taps(k),
        grid.scale,
        grid.taps(grid.max_half_spans())
    )
}

/// Most steps the real-grid check of a minimum or intermediate phase
/// rational design takes above the x16 half-span count.
pub const AUTO_MAX_GRID_STEPS: usize = 12;

/// The check gives up early after this many steps in a row that measure
/// worse than the best so far: past the phase conversion's accuracy
/// floor a longer kernel only measures worse.
pub const AUTO_GRID_WORSE_STEPS: usize = 3;

/// Geometric step of that check (at least one half span).
pub const AUTO_GRID_STEP: f64 = 1.02;

/// A minimum or intermediate phase design above [`AUTO_EXACT_MAX_SCALE`]:
/// the phase conversion of a kernel `max(up, down) / 16` times longer than
/// the x16 one is not as accurate, so the x16 half-span count can miss the
/// budget on the real grid. Start at that count, measure the converted
/// kernel on the real grid ([`protected_error_bounded`]) and step `k` up by
/// [`AUTO_GRID_STEP`] until it passes, at most [`AUTO_MAX_GRID_STEPS`]
/// times, else an error naming the grid scale (the result is shared by the
/// ratios with that scale). Not the shortest passing `k`, only one that
/// passes.
fn verified_on_grid(
    cfg: &ManualConfig,
    ratio: RateRatio,
    a: f64,
) -> Result<UpsamplerDesign, String> {
    let (d, k0) = derived_design(cfg, ratio, a)?;
    let grid = DesignGrid::for_ratio(cfg, ratio);
    let kmax = grid.max_half_spans();
    let mut k = k0;
    let (mut last, mut last_exact) = (f64::NAN, true);
    let (mut best, mut worse) = (f64::INFINITY, 0usize);
    for _ in 0..=AUTO_MAX_GRID_STEPS {
        if k > kmax {
            return Err(over_limit(a, grid, k));
        }
        let candidate = UpsamplerDesign {
            taps: grid.taps(k),
            ..d
        };
        // The last candidate's plans went with it.
        crate::design::budget::check_candidate(candidate.taps, true, 0)?;
        let h = try_design_kernel_for(&candidate, ratio).map_err(|e| {
            format!(
                "phase conversion failed at {} taps on the {}x grid: {e}",
                grid.taps(k),
                grid.scale
            )
        })?;
        if h.len() > AUTO_MAX_TAPS {
            return Err(over_limit(a, grid, k));
        }
        crate::design::budget::check_measurement(h.len(), true)?;
        let bound = 10.0_f64.powf(-(a + AUTO_MEASURE_MARGIN_DB) / 20.0);
        // A candidate's plans go with it (each candidate is longer).
        let mut planner = RealFftPlanner::<f64>::new();
        let (err, exact) = measure_ranked(
            &mut planner,
            &h,
            grid.gain as f64,
            grid.pass,
            grid.stop,
            bound,
            best,
        );
        if exact && err <= bound {
            return Ok(candidate);
        }
        (last, last_exact) = (-20.0 * err.log10(), exact);
        if exact && err < best {
            (best, worse) = (err, 0);
        } else {
            worse += 1;
            if worse >= AUTO_GRID_WORSE_STEPS {
                break;
            }
        }
        k = ((k as f64 * AUTO_GRID_STEP).ceil() as usize).max(k + 1);
    }
    // A partial last measurement bounds the attenuation from above.
    let last = if last_exact {
        format!("last {last:.2} dB")
    } else {
        format!("last at most {last:.2} dB")
    };
    Err(format!(
        "no {} phase design met {a:.2} dB on the {}x grid above {} taps (best {:.2} dB, \
         {last}); widen the transition or lower the target",
        if cfg.phase == FilterPhase::Minimum {
            "minimum"
        } else {
            "intermediate"
        },
        grid.scale,
        grid.taps(k0),
        -20.0 * best.log10()
    ))
}

/// The shortest grid length an automatic design at `ratio` can have,
/// cheaply: above [`AUTO_EXACT_MAX_SCALE`] the x16-derived length (the
/// real-grid check only lengthens it), else the design itself. Lets a
/// caller refuse a design over its size or cost limits before the
/// real-grid check runs.
pub fn automatic_taps_lower_bound(cfg: &ManualConfig, ratio: RateRatio) -> Result<usize, String> {
    let a = automatic_target(cfg)?;
    let cfg = &normalised(cfg);
    if derived(ratio) {
        derived_design(cfg, ratio, a).map(|(d, _)| d.taps)
    } else {
        automatic_design_for(cfg, ratio).map(|d| d.taps)
    }
}

/// The config with the fields an automatic design ignores set to fixed
/// values, so the design does not depend on who asked first.
fn normalised(cfg: &ManualConfig) -> ManualConfig {
    let mut cfg = cfg.clone();
    if cfg.phase != FilterPhase::Intermediate {
        cfg.phase_mix = 1.0;
    }
    cfg.taper = ManualConfig::default().taper;
    cfg
}

/// [`automatic_design`] without the cache (for timing and tests).
///
/// Search: start at [`auto_start_fraction`] of the window-formula estimate,
/// step geometrically by [`AUTO_BRACKET_STEP`] until one half-span count
/// fails and another passes, then bisect to adjacent counts. Only the cheap
/// linear prototype is measured during this. For minimum or intermediate
/// phase, the converted kernel is then measured from the shortest linear
/// pass upwards ([`AUTO_MAX_PHASE_STEPS`] at most, giving up after
/// [`AUTO_MAX_PHASE_FAILURES`] failed conversions) and, when the first
/// converted pass is the linear one, one step below is checked too. Error
/// is not strictly monotonic in length (sidelobe peaks move), so this is
/// the shortest passing length of the bracket, not of every shorter length.
/// Beta is fixed by `A +` [`AUTO_BETA_MARGIN_DB`].
pub fn automatic_design_uncached(
    cfg: &ManualConfig,
    ratio: usize,
) -> Result<UpsamplerDesign, String> {
    if !(1..=64).contains(&ratio) {
        return Err("design ratio must be in 1..=64".into());
    }
    automatic_design_uncached_for(cfg, RateRatio::integer(ratio as u32))
}

/// A planner that keeps the plans of one FFT size: the search's
/// measurements at that size reuse them, a new size drops them
/// ([`crate::design::budget::measurement_workspace`] charges one size).
#[derive(Default)]
struct OneSize {
    size: usize,
    planner: Option<RealFftPlanner<f64>>,
}

impl OneSize {
    /// What the plans it holds take (while the next candidate is designed).
    fn retained(&self) -> u64 {
        if self.planner.is_some() {
            crate::design::budget::plan_workspace(self.size)
        } else {
            0
        }
    }

    /// The planner for [`protected_error_with`] on a `len`-tap kernel.
    fn for_len(&mut self, len: usize) -> &mut RealFftPlanner<f64> {
        let size = (len.max(2) * 32).next_power_of_two();
        if self.size != size {
            self.size = size;
            self.planner = None;
        }
        self.planner.get_or_insert_with(RealFftPlanner::new)
    }
}

/// [`automatic_design_uncached`] at any ratio, measured on the ratio's own
/// grid ([`DesignGrid::for_ratio`]).
pub fn automatic_design_uncached_for(
    cfg: &ManualConfig,
    ratio: RateRatio,
) -> Result<UpsamplerDesign, String> {
    #[cfg(test)]
    count_search();
    let a = automatic_target(cfg)?;
    let estimate = automatic_estimate_for(cfg, ratio)?;
    let mut manual = cfg.clone();
    manual.auto_taps = None;
    manual.cutoff_mode = audivane_protocol::CutoffMode::Band;
    manual.band.attenuation_db = a + AUTO_BETA_MARGIN_DB;
    let mut d = UpsamplerDesign::try_for_ratio(&manual, ratio).map_err(|e| e.to_string())?;
    if ratio.is_unity() {
        return Ok(d);
    }
    let grid = DesignGrid::for_ratio(cfg, ratio);
    let (pass, stop, gain) = (grid.pass, grid.stop, grid.gain as f64);
    let bound = 10.0_f64.powf(-(a + AUTO_MEASURE_MARGIN_DB) / 20.0);
    let kmax = grid.max_half_spans();
    let taps = |k: usize| grid.taps(k);
    // Plans live as long as this search, not as long as the thread.
    let mut planner = OneSize::default();
    let fail = || {
        format!(
            "no design met {a:.2} dB within {kmax} input half spans ({} taps at {ratio}); \
             widen the transition or lower the target",
            taps(kmax)
        )
    };

    // Linear-prototype bracket and bisection.
    let mut steps = 0usize;
    let mut linear_passes = |k: usize| -> Result<bool, String> {
        steps += 1;
        if steps > AUTO_MAX_LINEAR_STEPS {
            return Err(format!(
                "automatic taps search exceeded {AUTO_MAX_LINEAR_STEPS} measurements"
            ));
        }
        crate::design::budget::check_candidate(taps(k), false, planner.retained())?;
        crate::design::budget::check_measurement(taps(k), false)?;
        let h = try_design_kernel_for(
            &UpsamplerDesign {
                taps: taps(k),
                phase: FilterPhase::Linear,
                ..d
            },
            ratio,
        )
        .map_err(|e| e.to_string())?;
        Ok(protected_error_with(planner.for_len(h.len()), &h, gain, pass, stop) <= bound)
    };
    let start =
        ((auto_start_fraction(cfg.window) * estimate as f64) / (2.0 * grid.scale as f64)) as usize;
    let mut k = start.clamp(1, kmax);
    // `lo` fails (0 stands for "nothing shorter"), `hi` passes.
    let (mut lo, mut hi);
    if linear_passes(k)? {
        hi = k;
        lo = 0;
        while hi > 1 {
            let next = ((hi as f64 / AUTO_BRACKET_STEP) as usize).clamp(1, hi - 1);
            if linear_passes(next)? {
                hi = next;
            } else {
                lo = next;
                break;
            }
        }
    } else {
        lo = k;
        loop {
            if lo >= kmax {
                return Err(fail());
            }
            k = ((lo as f64 * AUTO_BRACKET_STEP).ceil() as usize).clamp(lo + 1, kmax);
            if linear_passes(k)? {
                hi = k;
                break;
            }
            lo = k;
        }
    }
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if linear_passes(mid)? {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    if d.phase == FilterPhase::Linear {
        d.taps = taps(hi);
        return Ok(d);
    }

    // Phase conversion, measured on the final kernel.
    let mut failures = 0usize;
    let mut converted_passes = |k: usize| -> Result<bool, String> {
        crate::design::budget::check_candidate(taps(k), true, planner.retained())?;
        match try_design_kernel_for(&UpsamplerDesign { taps: taps(k), ..d }, ratio) {
            Ok(h) if h.len() > AUTO_MAX_TAPS => Ok(false),
            Ok(h) => {
                // The converted kernel (an intermediate one grows).
                crate::design::budget::check_measurement(h.len(), false)?;
                Ok(protected_error_with(planner.for_len(h.len()), &h, gain, pass, stop) <= bound)
            }
            Err(e) => {
                failures += 1;
                if failures >= AUTO_MAX_PHASE_FAILURES {
                    Err(format!(
                        "phase conversion failed {failures} times near {} taps: {e}",
                        taps(k)
                    ))
                } else {
                    Ok(false)
                }
            }
        }
    };
    let first = hi;
    let mut found = None;
    for k in first..(first + AUTO_MAX_PHASE_STEPS).min(kmax + 1) {
        if converted_passes(k)? {
            found = Some(k);
            break;
        }
    }
    let Some(mut k) = found else {
        return Err(format!(
            "no {} phase design met {a:.2} dB within {AUTO_MAX_PHASE_STEPS} steps above the \
             {}-tap linear length",
            if d.phase == FilterPhase::Minimum {
                "minimum"
            } else {
                "intermediate"
            },
            taps(first)
        ));
    };
    if k == first {
        // The converted response is not bit-identical to the prototype's:
        // make sure the next shorter one does not pass as well.
        while k > 1 && first - k < AUTO_MAX_PHASE_STEPS && converted_passes(k - 1)? {
            k -= 1;
        }
    }
    d.taps = taps(k);
    Ok(d)
}

#[cfg(test)]
thread_local! {
    /// Calls into the search (cached or not) on this thread, for tests
    /// that a caller does not search.
    static SEARCHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn count_search() {
    SEARCHES.with(|n| n.set(n.get() + 1));
}

/// Searches started on this thread so far (tests only).
#[cfg(test)]
pub(crate) fn searches() -> usize {
    SEARCHES.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resample::try_design_kernel;
    use audivane_protocol::{DitherKind, OutputConfig, RateTarget};

    #[test]
    fn cache_evicts_the_least_recently_used_entry_only() {
        let key = |ratio| CacheKey {
            window: WindowKind::Kaiser,
            phase: FilterPhase::Linear,
            phase_mix: 0,
            pass: 0,
            stop: 0,
            up: ratio as u32,
            down: 1,
            target: 0,
        };
        let mut cache = Cache::default();
        for ratio in 0..CACHE_CAPACITY {
            cache.insert(key(ratio), (Err(ratio.to_string()), 0));
        }
        // Touch the oldest entry: the next oldest goes instead.
        assert!(cache.get(&key(0)).is_some());
        cache.insert(key(CACHE_CAPACITY), (Err("new".into()), 0));
        assert_eq!(cache.entries.len(), CACHE_CAPACITY);
        assert!(cache.get(&key(0)).is_some());
        assert!(cache.get(&key(1)).is_none());
        for ratio in 2..=CACHE_CAPACITY {
            assert!(cache.get(&key(ratio)).is_some(), "{ratio}");
        }
    }

    #[test]
    fn output_budgets_include_dither_variance() {
        for bits in [16, 24, 32] {
            let bare = audivane_protocol::filter_error_budget_db(&OutputConfig {
                bits,
                dither: DitherKind::None,
                headroom_db: 0.0,
            });
            let tpdf = audivane_protocol::filter_error_budget_db(&OutputConfig {
                bits,
                dither: DitherKind::Tpdf,
                headroom_db: 0.0,
            });
            assert!((bare - tpdf - 4.771212547).abs() < 1e-8);
        }
        let cfg = ManualConfig {
            auto_taps: Some(FilterErrorBudget::MatchOutput),
            ..Default::default()
        };
        let out = OutputConfig {
            bits: 16,
            dither: DitherKind::Tpdf,
            headroom_db: 0.0,
        };
        assert_eq!(
            cfg.with_output(&out).auto_taps,
            Some(FilterErrorBudget::Db(
                audivane_protocol::filter_error_budget_db(&out)
            ))
        );
    }

    fn config(ratio: u32, window: WindowKind, phase: FilterPhase, a: f64) -> ManualConfig {
        ManualConfig {
            target: RateTarget::Ratio(ratio),
            window,
            phase,
            phase_mix: 0.5,
            auto_taps: Some(FilterErrorBudget::Db(a)),
            ..Default::default()
        }
    }

    #[test]
    fn automatic_taps_meets_budget_through_the_engine_and_previous_step_fails() {
        for a in [98.0, 146.24, 194.4] {
            for ratio in [2, 4, 8, 16, 32] {
                for window in [WindowKind::Kaiser, WindowKind::Gaussian] {
                    for phase in [
                        FilterPhase::Linear,
                        FilterPhase::Minimum,
                        FilterPhase::Intermediate,
                    ] {
                        let cfg = config(ratio, window, phase, a);
                        let d = automatic_design(&cfg, ratio as usize).unwrap();
                        let up = cfg.clone().into_upsampler_config().unwrap();
                        let measured = crate::resample::measure_upsampler(&up, 44_100).unwrap();
                        let pass = cfg.band.passband_end / (2.0 * ratio as f64);
                        let stop = cfg.band.stopband_start / (2.0 * ratio as f64);
                        let err = measured
                            .protected_band_error(cfg.band.passband_end, cfg.band.stopband_start);
                        let prev = UpsamplerDesign {
                            taps: d.taps - 2 * ratio as usize,
                            ..d
                        };
                        let prior = try_design_kernel(&prev)
                            .map(|h| protected_error(&h, ratio as f64, pass, stop))
                            .unwrap_or(f64::INFINITY);
                        eprintln!(
                            "{a} dB x{ratio} {window:?} {phase:?}: {} taps, error {:.4} dB, previous {:.4} dB",
                            d.taps,
                            -20.0 * err.log10(),
                            -20.0 * prior.log10()
                        );
                        assert!(err <= 10.0_f64.powf(-a / 20.0));
                        assert!(prior > 10.0_f64.powf(-(a + 0.05) / 20.0));
                    }
                }
            }
        }
    }

    #[test]
    fn output_precision_targets() {
        for a in [146.24, 194.4] {
            for window in [WindowKind::Kaiser, WindowKind::Gaussian] {
                for phase in [
                    FilterPhase::Linear,
                    FilterPhase::Minimum,
                    FilterPhase::Intermediate,
                ] {
                    let cfg = config(8, window, phase, a);
                    let d = automatic_design(&cfg, 8).unwrap();
                    let h = try_design_kernel(&d).unwrap();
                    let err = protected_error(
                        &h,
                        8.0,
                        cfg.band.passband_end / 16.0,
                        cfg.band.stopband_start / 16.0,
                    );
                    eprintln!(
                        "{a} {window:?} {phase:?}: {} taps, {:.4} dB",
                        d.taps,
                        -20.0 * err.log10()
                    );
                    assert!(err <= 10.0_f64.powf(-a / 20.0));
                }
            }
        }
    }

    /// Largest relative error over the protected bands by direct DTFT,
    /// independent of [`protected_error`] and its FFT grid: 8 points per tap
    /// across both bands, and 512 points per tap within 4/N of each edge.
    fn dense_dtft_error(h: &[f64], gain: f64, pass: f64, stop: f64) -> f64 {
        let n = h.len() as f64;
        let response = |f: f64| {
            let (mut re, mut im) = (0.0_f64, 0.0_f64);
            let w = -2.0 * PI * f;
            let (step_s, step_c) = w.sin_cos();
            let (mut s, mut c) = (0.0_f64, 1.0_f64);
            for (i, &v) in h.iter().enumerate() {
                if i % 64 == 0 {
                    // Re-anchor the rotating phasor to bound rounding drift.
                    (s, c) = (w * i as f64).sin_cos();
                }
                re += v * c;
                im += v * s;
                (s, c) = (s * step_c + c * step_s, c * step_c - s * step_s);
            }
            re.hypot(im) / gain
        };
        let mut freqs = Vec::new();
        let coarse = 1.0 / (8.0 * n);
        let fine = 1.0 / (512.0 * n);
        let mut f = 0.0;
        while f <= pass {
            freqs.push((f, 1.0));
            f += coarse;
        }
        let mut f = stop;
        while f <= 0.5 {
            freqs.push((f, 0.0));
            f += coarse;
        }
        for i in 0..=2048 {
            let d = i as f64 * fine;
            if pass - d >= 0.0 {
                freqs.push((pass - d, 1.0));
            }
            if stop + d <= 0.5 {
                freqs.push((stop + d, 0.0));
            }
        }
        freqs
            .into_iter()
            .map(|(f, target)| (response(f) - target).abs())
            .fold(0.0, f64::max)
    }

    #[test]
    fn independent_dense_dtft_meets_the_budget() {
        for (ratio, window, phase, a) in [
            (2, WindowKind::Kaiser, FilterPhase::Linear, 98.0),
            (32, WindowKind::Kaiser, FilterPhase::Linear, 98.0),
            (8, WindowKind::Gaussian, FilterPhase::Minimum, 146.24),
            (8, WindowKind::Kaiser, FilterPhase::Intermediate, 141.47),
            (4, WindowKind::Kaiser, FilterPhase::Minimum, 194.4),
        ] {
            let cfg = config(ratio, window, phase, a);
            let d = automatic_design(&cfg, ratio as usize).unwrap();
            let h = try_design_kernel(&d).unwrap();
            let pass = cfg.band.passband_end / (2.0 * ratio as f64);
            let stop = cfg.band.stopband_start / (2.0 * ratio as f64);
            let err = dense_dtft_error(&h, ratio as f64, pass, stop);
            eprintln!(
                "dense DTFT x{ratio} {window:?} {phase:?} {a} dB: {} taps, {:.3} dB",
                h.len(),
                -20.0 * err.log10()
            );
            assert!(err <= 10.0_f64.powf(-a / 20.0), "{}", -20.0 * err.log10());
        }
    }

    #[test]
    fn bracketed_search_matches_an_exhaustive_walk() {
        for (ratio, window, a, pe, se) in [
            (2, WindowKind::Kaiser, 98.0, 0.9, 1.0),
            (4, WindowKind::Gaussian, 120.0, 0.95, 1.0),
            (2, WindowKind::Kaiser, 146.24, 0.5, 1.5),
            (3, WindowKind::Gaussian, 70.0, 0.8, 1.2),
        ] {
            let mut cfg = config(ratio, window, FilterPhase::Linear, a);
            cfg.band.passband_end = pe;
            cfg.band.stopband_start = se;
            let d = automatic_design_uncached(&cfg, ratio as usize).unwrap();
            let pass = pe / (2.0 * ratio as f64);
            let stop = se / (2.0 * ratio as f64);
            let bound = 10.0_f64.powf(-(a + AUTO_MEASURE_MARGIN_DB) / 20.0);
            let first = (1..)
                .map(|k| 2 * k * ratio as usize + 1)
                .find(|&taps| {
                    let h = try_design_kernel(&UpsamplerDesign { taps, ..d }).unwrap();
                    protected_error(&h, ratio as f64, pass, stop) <= bound
                })
                .unwrap();
            assert_eq!(d.taps, first, "x{ratio} {window:?} {a} dB [{pe}, {se}]");
        }
    }

    #[test]
    fn designs_are_cached_and_infeasible_ones_rejected_from_the_estimate() {
        let mut cfg = config(4, WindowKind::Kaiser, FilterPhase::Minimum, 123.0);
        cfg.band.passband_end = 0.97;
        let first = automatic_design(&cfg, 4).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(automatic_design(&cfg, 4).unwrap(), first);
        assert!(started.elapsed() < std::time::Duration::from_millis(5));

        cfg.band.passband_end = 0.999;
        let started = std::time::Instant::now();
        let err = automatic_estimate(&cfg, 32).unwrap_err();
        assert!(err.contains("automatic limit"), "{err}");
        assert_eq!(automatic_design(&cfg, 32).unwrap_err(), err);
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }

    #[test]
    fn cached_designs_normalise_ignored_fields() {
        let mut a = config(3, WindowKind::Kaiser, FilterPhase::Minimum, 111.0);
        a.phase_mix = 0.3;
        a.taper = 0.6;
        let mut b = a.clone();
        b.phase_mix = 0.8;
        b.taper = 0.1;
        let da = automatic_design(&a, 3).unwrap();
        assert_eq!(automatic_design(&b, 3).unwrap(), da);
        assert_eq!(da.phase_mix, 1.0);
        assert_eq!(da.taper, ManualConfig::default().taper);
        // Intermediate phase keeps its mix, and it is part of the key.
        a.phase = FilterPhase::Intermediate;
        b.phase = FilterPhase::Intermediate;
        assert_eq!(automatic_design(&a, 3).unwrap().phase_mix, 0.3);
        assert_eq!(automatic_design(&b, 3).unwrap().phase_mix, 0.8);
    }

    #[test]
    fn design_grid_holds_the_ratio_handling() {
        let cfg = config(8, WindowKind::Kaiser, FilterPhase::Linear, 98.0);
        let g = DesignGrid::new(&cfg, 8);
        assert_eq!((g.gain, g.scale), (8, 8));
        assert_eq!(g.pass, cfg.band.passband_end / 16.0);
        assert_eq!(g.stop, cfg.band.stopband_start / 16.0);
        assert_eq!(g.taps(3), 49);
        assert_eq!(g.max_half_spans(), AUTO_MAX_HALF_SPANS);
        assert_eq!(
            DesignGrid::new(&cfg, 64).max_half_spans(),
            AUTO_MAX_HALF_SPANS
        );
        let h = try_design_kernel(&automatic_design(&cfg, 8).unwrap()).unwrap();
        assert_eq!(
            protected_error(&h, 8.0, g.pass, g.stop),
            protected_error_with(&mut RealFftPlanner::new(), &h, 8.0, g.pass, g.stop)
        );
    }

    #[test]
    fn rational_ratios_have_their_own_gain_scale_and_cache_entry() {
        let cfg = config(2, WindowKind::Kaiser, FilterPhase::Linear, 120.0);
        let up = RateRatio::from_rates(44_100, 48_000);
        let down = RateRatio::from_rates(48_000, 44_100);
        let (gu, gd) = (
            DesignGrid::for_ratio(&cfg, up),
            DesignGrid::for_ratio(&cfg, down),
        );
        assert_eq!((gu.gain, gu.scale), (160, 160));
        assert_eq!((gd.gain, gd.scale), (147, 160));
        assert_eq!((gu.pass, gu.stop), (gd.pass, gd.stop));
        assert!(automatic_estimate_for(&cfg, up).is_ok());
        assert!(automatic_estimate(&cfg, 160).is_err());

        // Both directions: the same grid length (the band is the lower
        // Nyquist either way), the kernel scaled to its own gain, and on
        // the real 160x grid within the budget although the half spans
        // were searched at x16.
        let du = automatic_design_for(&cfg, up).unwrap();
        let dd = automatic_design_for(&cfg, down).unwrap();
        assert_eq!((du.ratio, dd.ratio), (160, 160));
        assert_eq!(du.taps, dd.taps);
        assert_eq!((du.taps - 1) % (2 * 160), 0);
        let bound = 10.0_f64.powf(-120.0 / 20.0);
        for (d, r, grid) in [(du, up, gu), (dd, down, gd)] {
            let h = try_design_kernel_for(&d, r).unwrap();
            let sum: f64 = h.iter().sum();
            assert!(
                (sum / grid.gain as f64 - 1.0).abs() < 1e-6,
                "{r}: gain {sum}"
            );
            let err = protected_error(&h, grid.gain as f64, grid.pass, grid.stop);
            assert!(err <= bound, "{r}: {:.2} dB", 20.0 * err.log10());
        }

        // Other windows and phases hold on the real grid too.
        for (window, phase, a) in [
            (WindowKind::Gaussian, FilterPhase::Minimum, 140.0),
            (WindowKind::Kaiser, FilterPhase::Intermediate, 150.0),
        ] {
            let c = config(2, window, phase, a);
            let d = automatic_design_for(&c, up).unwrap();
            let h = try_design_kernel_for(&d, up).unwrap();
            let g = DesignGrid::for_ratio(&c, up);
            let err = protected_error(&h, g.gain as f64, g.pass, g.stop);
            assert!(
                err <= 10.0_f64.powf(-a / 20.0),
                "{window:?} {phase:?} {a} dB: {:.2} dB",
                20.0 * err.log10()
            );
        }

        // A small scale is searched on its own grid: 96 -> 48 kHz (1/2),
        // gain 1, scale 2, the same length as x2.
        let half = RateRatio::from_rates(96_000, 48_000);
        let g = DesignGrid::for_ratio(&cfg, half);
        assert_eq!((g.gain, g.scale), (1, 2));
        let d = automatic_design_for(&cfg, half).unwrap();
        assert_eq!(d.taps, automatic_design(&cfg, 2).unwrap().taps);
        let h = try_design_kernel_for(&d, half).unwrap();
        assert!(protected_error(&h, 1.0, g.pass, g.stop) <= bound);
    }

    /// A refusal cached for a shared rational grid names the grid, not one
    /// ratio, so its text fits every ratio on that grid.
    #[test]
    fn a_shared_scale_refusal_names_the_grid_not_the_ratio() {
        // 44.1k -> 768k (2560/147) and 768k -> 44.1k (147/2560) share the
        // 2560x grid and its cached refusal: the text must fit both.
        for phase in [FilterPhase::Minimum, FilterPhase::Linear] {
            let mut cfg = config(2, WindowKind::Kaiser, phase, 121.37);
            cfg.band.passband_end = 0.99;
            let up = RateRatio::new(2560, 147);
            let down = RateRatio::new(147, 2560);
            let eu = automatic_design_for(&cfg, up).unwrap_err();
            let ed = automatic_design_for(&cfg, down).unwrap_err();
            assert_eq!(eu, ed, "{phase:?}");
            assert!(eu.contains("on the 2560x grid"), "{eu}");
            assert!(!eu.contains("2560/147") && !eu.contains("147/2560"), "{eu}");
            // Integer ratios still name theirs.
            let mut wide = cfg.clone();
            wide.band.passband_end = 0.9999;
            let e = automatic_design_for(&wide, RateRatio::integer(32)).unwrap_err();
            assert!(e.contains("at x32"), "{e}");
        }
    }

    /// Minimum and intermediate phase above scale 16: the x16 half-span
    /// count can miss the budget on the real grid. The real-grid check
    /// lengthens them until they pass, and the derived length stays a lower
    /// bound.
    #[test]
    fn rational_phase_designs_meet_the_budget_on_the_real_grid() {
        for (i, o, phase, mix, a) in [
            (48_000, 44_100, FilterPhase::Minimum, 0.5, 133.0),
            (176_400, 288_000, FilterPhase::Minimum, 0.5, 186.0),
            (44_100, 48_000, FilterPhase::Intermediate, 0.2, 186.0),
        ] {
            let r = RateRatio::from_rates(i, o);
            let mut cfg = config(2, WindowKind::Gaussian, phase, a);
            cfg.phase_mix = mix;
            let d = automatic_design_for(&cfg, r).unwrap();
            let lower = automatic_taps_lower_bound(&cfg, r).unwrap();
            assert!(d.taps >= lower, "{r}: {} < {lower}", d.taps);
            let g = DesignGrid::for_ratio(&cfg, r);
            let h = try_design_kernel_for(&d, r).unwrap();
            let err = protected_error(&h, g.gain as f64, g.pass, g.stop);
            assert!(
                err <= 10.0_f64.powf(-a / 20.0),
                "{i} -> {o} {phase:?} {a} dB: {:.2} dB at {} taps",
                -20.0 * err.log10(),
                d.taps
            );
            // Cached under the ratio and under its grid scale: the other
            // direction (same scale) is a cache hit with the same design.
            let t = std::time::Instant::now();
            assert_eq!(
                automatic_design_for(&cfg, RateRatio::new(r.down, r.up)).unwrap(),
                d
            );
            assert!(t.elapsed() < std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn long_kernels_are_measured_at_full_density() {
        // Kernels past 2^23 / 32 taps are measured on interleaved shifted
        // grids; together they are the full-density grid, so the error is
        // the full-density measurement (within FFT rounding).
        let mut planner = RealFftPlanner::new();
        for (scale, half_spans, beta, phase) in [
            // 300 001 taps: two grids of 2^23 points (16 per tap).
            (300, 500, 16.0, FilterPhase::Linear),
            // 576 001 taps: four grids (8 per tap), minimum phase.
            (640, 450, 18.0, FilterPhase::Minimum),
        ] {
            let taps = 2 * half_spans * scale + 1;
            let d = UpsamplerDesign {
                ratio: scale,
                taps,
                kaiser_beta: beta,
                cutoff: 0.95,
                window: WindowKind::Kaiser,
                phase,
                ..UpsamplerDesign::try_from_config(&ManualConfig::default(), 2).unwrap()
            };
            let h = try_design_kernel(&d).unwrap();
            assert_eq!(h.len(), taps);
            assert!((taps * 32).next_power_of_two() > AUTO_GRID_MAX_FFT);
            let gain = scale as f64;
            let (pass, stop) = (0.9 / (2.0 * scale as f64), 1.0 / (2.0 * scale as f64));
            let full = protected_error_with(&mut planner, &h, gain, pass, stop);
            let (split, complete) =
                protected_error_bounded(&mut planner, &h, gain, pass, stop, 1.0);
            assert!(complete);
            let db = |e: f64| -20.0 * e.log10();
            assert!(
                (db(full) - db(split)).abs() < 1e-3,
                "{taps} taps: full {:.4} dB, split {:.4} dB",
                db(full),
                db(split)
            );
            // The single coarse grid alone is a lower bound only.
            let m = if taps * 16 <= AUTO_GRID_MAX_FFT {
                16
            } else {
                8
            };
            let n = (taps * m).next_power_of_two();
            let coarse = protected_error_n(&mut planner, &h, gain, pass, stop, n);
            assert!(coarse <= full * (1.0 + 1e-9), "{taps} taps");
            // A bound the first grid already exceeds stops there, and says
            // the error is partial (a lower bound).
            let bound = coarse / 2.0;
            let early = protected_error_bounded(&mut planner, &h, gain, pass, stop, bound);
            assert_eq!(early, (coarse, false));
            // Ranked against the search's best: a partial error below the
            // best is completed (the best stays exact); one at or above it
            // already proves the candidate worse.
            let ranked = measure_ranked(&mut planner, &h, gain, pass, stop, bound, f64::INFINITY);
            assert_eq!(ranked, (split, true));
            assert!(coarse < split, "{taps} taps: the partial error is lower");
            let worse = measure_ranked(&mut planner, &h, gain, pass, stop, bound, coarse * 0.9);
            assert_eq!(worse, (coarse, false));
        }
        // A long Kaiser intermediate design at 44.1k -> 768k: the accepted
        // design meets its budget at full density, and the split
        // measurement agrees.
        let mut cfg = config(2, WindowKind::Kaiser, FilterPhase::Intermediate, 160.0);
        cfg.phase_mix = 0.5;
        let ratio = RateRatio::new(2560, 147);
        let d = automatic_design_for(&cfg, ratio).unwrap();
        let h = try_design_kernel_for(&d, ratio).unwrap();
        assert!((h.len() * 32).next_power_of_two() > AUTO_GRID_MAX_FFT);
        let grid = DesignGrid::for_ratio(&cfg, ratio);
        let full = protected_error_with(&mut planner, &h, grid.gain as f64, grid.pass, grid.stop);
        let bound = 10.0_f64.powf(-(160.0 + AUTO_MEASURE_MARGIN_DB) / 20.0);
        assert!(
            full <= bound,
            "{} taps: {:.3} dB",
            h.len(),
            -20.0 * full.log10()
        );
        let (split, complete) = protected_error_bounded(
            &mut planner,
            &h,
            grid.gain as f64,
            grid.pass,
            grid.stop,
            bound,
        );
        assert!(complete);
        assert!(
            (full.log10() - split.log10()).abs() < 1e-4,
            "{full} {split}"
        );
        let short = vec![0.5; 1001];
        assert_eq!(
            protected_error_bounded(&mut planner, &short, 1.0, 0.1, 0.2, 1.0),
            (protected_error(&short, 1.0, 0.1, 0.2), true)
        );
    }

    #[test]
    fn unresolved_match_output_is_an_error() {
        let mut cfg = config(2, WindowKind::Kaiser, FilterPhase::Linear, 98.0);
        cfg.auto_taps = Some(FilterErrorBudget::MatchOutput);
        assert!(automatic_target(&cfg).unwrap_err().contains("resolved"));
        assert!(automatic_design(&cfg, 2).is_err());
        let out = OutputConfig {
            bits: 32,
            dither: DitherKind::None,
            headroom_db: 0.0,
        };
        // 32-bit output resolves to the 24-bit equivalent.
        assert_eq!(
            automatic_target(&cfg.with_output(&out)).unwrap(),
            audivane_protocol::attenuation_for_bits(24)
        );
    }

    #[test]
    fn rejects_invalid_automatic_designs() {
        let mut cfg = config(2, WindowKind::Kaiser, FilterPhase::Linear, f64::NAN);
        assert!(automatic_design(&cfg, 2).is_err());
        cfg.auto_taps = Some(FilterErrorBudget::Db(140.0));
        cfg.band.passband_end = f64::NAN;
        assert!(automatic_design(&cfg, 2).is_err());
        cfg.band = Default::default();
        cfg.band.passband_end = 1.0 - f64::EPSILON / 2.0;
        cfg.band.stopband_start = 1.0;
        assert!(automatic_design(&cfg, 32).unwrap_err().contains("length"));
        cfg.band = Default::default();
        cfg.window = WindowKind::Tukey;
        assert!(automatic_design(&cfg, 2).is_err());
        assert!(automatic_design(&cfg, 0).is_err());
    }
}
