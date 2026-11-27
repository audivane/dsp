//! Per-parameter bounds of a design: every bound is checked before anything
//! is allocated or constructed, so no design reaches a panic or an assert. A
//! failure names
//! the field (`design.window.beta: must be between 0 and 40 (got 41)`).
//!
//! [`check`] holds a design to the table; the ratio-dependent bounds (a
//! length within the budget at a ratio, a block FFT block per ratio, a
//! decimating ratio with `preserve_samples`) are [`super::feasibility`]'s.
//! [`super::feasibility`] runs it on every new combination; the presets and
//! the Custom designs keep their own validation (they pass it).

use super::{
    ArithmeticPath, BlockSize, Cutoff, Decimation, FilterDesign, FilterErrorBudget, KernelDesign,
    Length, Phase, PhaseMethod, Sampling, Shape, Span, Stepped, Window, budget,
};
use crate::resample::{
    BAND_ATTENUATION_RANGE_DB, BAND_MAX_STOPBAND_START, BAND_MIN_PASSBAND_END,
    kaiser_attenuation_db,
};

/// A field out of bounds (the wire type).
pub use audivane_protocol::design::FieldError;

/// Largest `per_input` length, lobe count and decimation base.
pub const MAX_PER_INPUT: usize = 65_535;

/// The Kaiser beta range.
pub const BETA_RANGE: (f64, f64) = (0.0, 40.0);

/// The Kaiser beta range of a reflected phase.
pub const REFLECT_BETA_RANGE: (f64, f64) = (
    crate::minphase::MIN_MINIMUM_PHASE_BETA,
    crate::minphase::MAX_MINIMUM_PHASE_BETA,
);

/// The DPSS time-bandwidth range (the window model's,
/// [`super::window_model::DPSS_NW_RANGE`]).
pub const NW_RANGE: (f64, f64) = super::window_model::DPSS_NW_RANGE;

/// The Gaussian alpha range.
pub const ALPHA_RANGE: (f64, f64) = (0.05, 0.5);

/// The Chebyshev sidelobe range, dB (positive).
pub const SIDELOBE_RANGE_DB: (f64, f64) = (40.0, 220.0);

/// The deepest Chebyshev sidelobes a reflected phase takes, dB.
pub const REFLECT_MAX_SIDELOBE_DB: f64 = 190.0;

/// The largest input block an output-sized block FFT block may resolve
/// to (`P / L`, at the ratio): the catalogue's `block_fft_large` (2^20
/// output frames) at x2, its largest geometry. An `input` block stays
/// within 2^10 ..= 2^16.
pub const BLOCK_OUTPUT_MAX_INPUT: usize = 1 << 19;

/// The deepest automatic budget of an optimal design, dB: where f64
/// still resolves its error (the equiripple exchange places its extremals
/// to about 1e-10 of the gain; a least-squares solve floors near 150 dB),
/// measured by `paths::optimal::tests::measure_limits`.
pub fn optimal_max_db(method: super::OptimalMethod) -> f64 {
    match method {
        super::OptimalMethod::Equiripple => super::paths::optimal::EQUIRIPPLE_MAX_DB,
        super::OptimalMethod::LeastSquares => super::paths::optimal::LEAST_SQUARES_MAX_DB,
    }
}

/// The automatic budget range, dB.
pub const BUDGET_RANGE_DB: (f64, f64) = (55.0, 200.0);

/// The deepest band a minimum or intermediate reflected phase promises.
pub const REFLECT_MAX_BAND_DB: f64 = 190.0;

/// The output-span lengths: powers of two in this range.
pub const SPAN_RANGE: (usize, usize) = (1 << 12, 1 << 22);

/// The headroom range, dB.
pub const HEADROOM_RANGE_DB: (f64, f64) = (0.0, 12.0);

struct Checker {
    errors: Vec<FieldError>,
}

impl Checker {
    fn fail(&mut self, path: &str, message: impl Into<String>) {
        self.errors.push(FieldError {
            path: format!("design.{path}"),
            message: message.into(),
        });
    }

    fn finite(&mut self, path: &str, v: f64) -> bool {
        if v.is_finite() {
            true
        } else {
            self.fail(path, format!("must be finite (got {v})"));
            false
        }
    }

    fn range(&mut self, path: &str, v: f64, (lo, hi): (f64, f64)) {
        if self.finite(path, v) && !(lo..=hi).contains(&v) {
            self.fail(path, format!("must be between {lo} and {hi} (got {v})"));
        }
    }

    fn count(&mut self, path: &str, v: usize, lo: usize, hi: usize) {
        if !(lo..=hi).contains(&v) {
            self.fail(path, format!("must be between {lo} and {hi} (got {v})"));
        }
    }
}

/// The per-parameter bounds on `d`: the first field out of bounds.
pub fn check(d: &FilterDesign) -> Result<(), FieldError> {
    match check_all(d).into_iter().next() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Every field of `d` out of bounds.
pub fn check_all(d: &FilterDesign) -> Vec<FieldError> {
    let mut c = Checker { errors: Vec::new() };
    match d {
        FilterDesign::Kernel(k) => kernel(&mut c, k),
        FilterDesign::BlockFft(b) => match b.block {
            BlockSize::Input(n) => {
                if !n.is_power_of_two() || !(1 << 10..=1 << 16).contains(&n) {
                    c.fail(
                        "block.input",
                        format!("must be a power of two from 1024 to 65536 (got {n})"),
                    );
                }
            }
            BlockSize::Output(p) => {
                // 2^10 <= P / ratio <= BLOCK_OUTPUT_MAX_INPUT at the
                // ratio: feasibility.
                if !p.is_power_of_two() || !(1 << 11..=1 << 22).contains(&p) {
                    c.fail(
                        "block.output",
                        format!("must be a power of two from 2048 to 4194304 (got {p})"),
                    );
                }
            }
        },
        FilterDesign::Cascade(_) | FilterDesign::Interpolator(_) => {}
    }
    c.errors
}

/// The field a refusal of `d` is about, as a [`FieldError`]: a bound's
/// own path; `design.rates` for a ratio the design does not run at; the
/// length (`design.length`, `design.lobes`, `design.block`) for a budget;
/// `design.cutoff` for a band, `design.phase` for a phase transform,
/// `design.window` for an attenuation the window cannot keep; `design`
/// otherwise.
pub fn refusal(e: &super::DesignError, d: &FilterDesign) -> FieldError {
    use super::DesignError as E;
    let length = match d {
        FilterDesign::Kernel(KernelDesign {
            shape: Shape::Lanczos { .. },
            ..
        }) => "design.lobes",
        FilterDesign::Kernel(_) => "design.length",
        FilterDesign::BlockFft(_) => "design.block",
        _ => "design",
    };
    let optimal = matches!(
        d,
        FilterDesign::Kernel(KernelDesign {
            shape: Shape::Optimal { .. },
            ..
        })
    );
    let path = match e {
        E::Field(f) => return f.clone(),
        E::Ratio { .. } => "design.rates",
        E::TooLong { .. } | E::Budget(_) => length,
        E::Band(crate::resample::BandError::Automatic(_) | crate::resample::BandError::Length) => {
            length
        }
        E::Band(_) => "design.cutoff",
        E::Phase(_) => "design.phase",
        // An optimal design has no window: what it cannot reach, and the
        // limits of its length, are its length's.
        E::Claim(_) | E::Unsupported(_) if optimal => length,
        E::Claim(_) => "design.window",
        E::Unsupported(_) | E::NotAKernel | E::Failed(_) | E::Unsafe(_) => "design",
    };
    FieldError {
        path: path.into(),
        message: e.to_string(),
    }
}

/// The headroom bound (it sits beside the design).
pub fn check_headroom(db: f64) -> Result<(), FieldError> {
    let mut c = Checker { errors: Vec::new() };
    c.range("headroom_db", db, HEADROOM_RANGE_DB);
    match c.errors.pop() {
        Some(mut e) => {
            e.path = "headroom_db".into();
            Err(e)
        }
        None => Ok(()),
    }
}

fn reflected(k: &KernelDesign) -> bool {
    matches!(
        k.phase,
        Phase::Minimum(PhaseMethod::Reflect)
            | Phase::Intermediate {
                method: PhaseMethod::Reflect,
                ..
            }
    )
}

fn window(c: &mut Checker, path: &str, w: &Window, k: &KernelDesign) {
    let p = |f: &str| format!("{path}.{f}");
    let standard = k.numerics.path == ArithmeticPath::Standard;
    let reflect = reflected(k);
    let sampling = match *w {
        Window::Kaiser { beta, sampling } => {
            if let Some(b) = beta {
                c.range(&p("beta"), b, BETA_RANGE);
                if reflect && b.is_finite() {
                    c.range(&p("beta"), b, REFLECT_BETA_RANGE);
                }
            }
            sampling
        }
        Window::Gaussian { alpha, sampling } => {
            c.range(&p("alpha"), alpha, ALPHA_RANGE);
            if standard {
                c.fail(
                    &p("alpha"),
                    "the standard path's Gaussian takes a stopband, not alpha",
                );
            }
            if k.numerics.path == ArithmeticPath::DoubleDouble
                && sampling != Sampling::IntegerCentre
            {
                c.fail(
                    &p("sampling"),
                    "the double-double path builds the integer-centre Gaussian only",
                );
            }
            if sampling == Sampling::IntegerCentre {
                if reflect {
                    c.fail(
                        &p("sampling"),
                        "a reflected phase needs a linear-phase prototype: symmetric sampling",
                    );
                }
                return;
            }
            sampling
        }
        Window::GaussianStopband { beta, sampling } => {
            if let Some(b) = beta
                && c.finite(&p("stopband_db"), b)
            {
                c.range(
                    &p("stopband_db"),
                    kaiser_attenuation_db(b),
                    BAND_ATTENUATION_RANGE_DB,
                );
            }
            if !standard {
                c.fail(&p("type"), "the stopband Gaussian is the standard path's");
            }
            sampling
        }
        Window::Flattop { sampling, .. }
        | Window::Nuttall { sampling }
        | Window::Hann { sampling } => {
            if standard {
                c.fail(
                    &p("type"),
                    "this window is not on the standard path: use a preset path",
                );
            }
            sampling
        }
        Window::Chebyshev {
            sidelobe_ratio,
            sampling,
        } => {
            if c.finite(&p("sidelobe_ratio"), sidelobe_ratio) {
                let db = if sidelobe_ratio > 0.0 {
                    -20.0 * sidelobe_ratio.log10()
                } else {
                    f64::INFINITY
                };
                let (lo, hi) = SIDELOBE_RANGE_DB;
                if !(lo..=hi).contains(&db) {
                    c.fail(
                        &p("sidelobe_ratio"),
                        format!(
                            "must be between 10^(-{hi}/20) and 10^(-{lo}/20) (got {sidelobe_ratio})"
                        ),
                    );
                } else if reflect && db > REFLECT_MAX_SIDELOBE_DB {
                    c.fail(
                        &p("sidelobe_ratio"),
                        format!(
                            "a reflected phase takes sidelobes down to -{REFLECT_MAX_SIDELOBE_DB} \
                             dB (got {db:.1} dB)"
                        ),
                    );
                }
            }
            if standard {
                c.fail(
                    &p("type"),
                    "this window is not on the standard path: use a preset path",
                );
            }
            sampling
        }
        Window::Dpss { nw, sampling } => {
            if let Some(nw) = nw {
                c.range(&p("nw"), nw, NW_RANGE);
                // Kaiser's reflected limit at beta = pi NW.
                let most = REFLECT_BETA_RANGE.1 / std::f64::consts::PI;
                if reflect && nw.is_finite() && nw > most {
                    c.fail(
                        &p("nw"),
                        format!(
                            "a reflected phase takes NW up to {most:.2} (pi NW at most {}, as a \
                             Kaiser beta; got {nw})",
                            REFLECT_BETA_RANGE.1
                        ),
                    );
                }
            }
            if standard {
                c.fail(
                    &p("type"),
                    "this window is not on the standard path: use a preset path",
                );
            }
            if sampling != Sampling::Symmetric {
                // The periodic DPSS (N + 1 taps cut to N) is no longer
                // the concentration optimum: up to 15 dB shallower.
                c.fail(&p("sampling"), "the DPSS window is sampled symmetric only");
                return;
            }
            sampling
        }
        Window::Tukey { taper, sampling } => {
            if c.finite(&p("taper"), taper) && !(taper > 0.0 && taper <= 1.0) {
                c.fail(&p("taper"), format!("must be in (0, 1] (got {taper})"));
            }
            if reflect || !matches!(k.phase, Phase::Linear) {
                c.fail(
                    &p("type"),
                    "the Tukey window has no calibrated stopband: linear phase only",
                );
            }
            sampling
        }
    };
    if sampling == Sampling::IntegerCentre {
        c.fail(&p("sampling"), "integer_centre is the Gaussian's sampling");
    } else if reflect && !standard && sampling != Sampling::Symmetric {
        c.fail(
            &p("sampling"),
            "a reflected phase needs a linear-phase prototype: symmetric sampling",
        );
    }
}

fn stepped_usize(c: &mut Checker, path: &str, s: &Stepped<usize>, lo: usize, hi: usize) {
    match s {
        Stepped::Fixed(t) => c.count(path, *t, lo, hi),
        Stepped::BySourceRate(steps) => {
            steps_shape(c, path, steps.iter().map(|s| s.0));
            for (i, (_, t)) in steps.iter().enumerate() {
                c.count(&format!("{path}[{i}]"), *t, lo, hi);
            }
        }
    }
}

fn steps_shape(c: &mut Checker, path: &str, bounds: impl Iterator<Item = Option<u32>>) {
    let bounds: Vec<Option<u32>> = bounds.collect();
    let ok = !bounds.is_empty()
        && bounds.last() == Some(&None)
        && bounds[..bounds.len() - 1].iter().all(Option::is_some)
        && bounds.windows(2).all(|w| match (w[0], w[1]) {
            (Some(a), Some(b)) => a < b,
            _ => true,
        });
    if !ok {
        c.fail(
            path,
            "the steps need increasing bounds and an unbounded last step",
        );
    }
}

fn kernel(c: &mut Checker, k: &KernelDesign) {
    use ArithmeticPath::*;
    let path = k.numerics.path;
    let reflect = reflected(k);
    match k.phase {
        Phase::Intermediate { mix, .. } => c.range("phase.mix", mix, (0.0, 1.0)),
        Phase::Minimum(_) | Phase::Linear => {}
    }
    if reflect && !matches!(path, Fir | FirAfter | Standard) {
        c.fail(
            "phase.method",
            "a reflected phase is built on the fir paths or the standard path",
        );
    }
    if let Decimation::Scale {
        base_taps: Some(b), ..
    } = k.decimation
    {
        c.count("decimation.base_taps", b, 1, MAX_PER_INPUT);
    }
    if path == Standard && k.decimation != Decimation::Keep {
        c.fail(
            "decimation.length",
            "the standard path counts frames: keep only",
        );
    }
    if path != Standard
        && k.numerics.grid_cutoff == super::GridCutoff::DesignRatio
        && k.decimation != Decimation::Keep
    {
        c.fail(
            "decimation.length",
            "the design-ratio grid cutoff designs the kernel at the decimation ratio itself: \
             keep only",
        );
    }
    if let Some(f) = k.rates.fallback_window {
        if matches!(f.window, Window::Tukey { .. }) {
            c.fail(
                "rates.fallback_window",
                "the Tukey window cannot be a fallback",
            );
        }
        window(c, "rates.fallback_window.window", &f.window, k);
    }
    match &k.shape {
        Shape::Optimal {
            method,
            length,
            passband_end,
            stopband_start,
        } => {
            let (pe, se) = (*passband_end, *stopband_start);
            if c.finite("passband_end", pe) & c.finite("stopband_start", se)
                && (pe < BAND_MIN_PASSBAND_END
                    || pe > 1.0
                    || se <= pe
                    || se > BAND_MAX_STOPBAND_START)
            {
                c.fail(
                    "passband_end",
                    format!(
                        "needs {BAND_MIN_PASSBAND_END} <= passband end ({pe}) <= 1 and passband \
                         end < stopband start ({se}) <= {BAND_MAX_STOPBAND_START}"
                    ),
                );
            }
            match length {
                Length::Fixed(t) => c.count("length.taps", *t, 1, budget::MAX_KERNEL_TAPS),
                Length::PerInput(t) => stepped_usize(c, "length.taps", t, 1, MAX_PER_INPUT),
                Length::Auto(FilterErrorBudget::MatchOutput) => {}
                Length::Auto(FilterErrorBudget::Db(a)) => {
                    let max = optimal_max_db(*method);
                    c.range("length.budget", *a, (BUDGET_RANGE_DB.0, max));
                }
                Length::OutputSpan(_) | Length::Band => c.fail(
                    "length.rule",
                    "an optimal design takes a fixed, per-input or automatic length",
                ),
            }
            if !matches!(path, Fir | FirAfter) {
                c.fail(
                    "numerics.path",
                    "an optimal design is built on the fir paths",
                );
            }
            if k.preserve_samples {
                c.fail(
                    "preserve_samples",
                    "an optimal low-pass is not a Nyquist filter: its zeros are off the input grid",
                );
            }
            if k.rates.fallback_window.is_some() {
                c.fail(
                    "rates.fallback_window",
                    "an optimal design has no window to fall back from",
                );
            }
        }
        Shape::Lanczos { lobes } => {
            c.count("lobes", *lobes, 1, MAX_PER_INPUT);
            if !matches!(path, Fir | FirAfter) {
                c.fail(
                    "numerics.path",
                    "the Lanczos kernel is built on the fir paths",
                );
            }
            if k.preserve_samples {
                c.fail(
                    "preserve_samples",
                    "a Lanczos kernel's zeros are off the input grid",
                );
            }
        }
        Shape::WindowedSinc {
            window: w,
            length,
            cutoff,
        } => {
            window(c, "window", w, k);
            match length {
                Length::Fixed(t) => c.count("length.taps", *t, 1, budget::MAX_KERNEL_TAPS),
                Length::PerInput(t) => stepped_usize(c, "length.taps", t, 1, MAX_PER_INPUT),
                Length::OutputSpan(Span::Ladder) => {}
                Length::OutputSpan(Span::Taps(n)) => {
                    if !n.is_power_of_two() || !(SPAN_RANGE.0..=SPAN_RANGE.1).contains(n) {
                        c.fail(
                            "length.taps",
                            format!(
                                "must be \"ladder\" or a power of two from {} to {} (got {n})",
                                SPAN_RANGE.0, SPAN_RANGE.1
                            ),
                        );
                    }
                }
                Length::Band => {}
                Length::Auto(FilterErrorBudget::MatchOutput) => {}
                Length::Auto(FilterErrorBudget::Db(a)) => {
                    c.range("length.budget", *a, BUDGET_RANGE_DB);
                    if matches!(w, Window::Tukey { .. }) {
                        c.fail("length.rule", "the Tukey window has no calibrated stopband");
                    }
                }
            }
            if matches!(length, Length::Band | Length::Auto(_)) != matches!(cutoff, Cutoff::Band(_))
            {
                c.fail(
                    "cutoff.mode",
                    "a band or automatic length goes with a band cutoff, and only with one",
                );
            }
            match cutoff {
                Cutoff::Fraction(fc) => {
                    let values: Vec<f64> = match fc {
                        Stepped::Fixed(v) => vec![*v],
                        Stepped::BySourceRate(steps) => {
                            steps_shape(c, "cutoff.value", steps.iter().map(|s| s.0));
                            steps.iter().map(|s| s.1).collect()
                        }
                    };
                    for v in &values {
                        if c.finite("cutoff.value", *v) && !(*v > 0.0 && *v <= 1.0) {
                            c.fail("cutoff.value", format!("must be in (0, 1] (got {v})"));
                        }
                    }
                    if k.preserve_samples && values.iter().any(|&v| v != 1.0) {
                        c.fail("preserve_samples", "needs the cutoff fraction 1.0");
                    }
                }
                Cutoff::NyquistStop => {
                    if matches!(w, Window::Tukey { .. }) {
                        c.fail("cutoff.mode", "the Tukey window has no calibrated stopband");
                    }
                    if k.preserve_samples {
                        c.fail("preserve_samples", "needs the cutoff fraction 1.0");
                    }
                }
                Cutoff::Band(b) => {
                    let (pe, se, a) = (b.passband_end, b.stopband_start, b.attenuation_db);
                    let fin =
                        c.finite("cutoff.passband_end", pe) & c.finite("cutoff.stopband_start", se);
                    if fin
                        && (pe < BAND_MIN_PASSBAND_END
                            || pe > 1.0
                            || se <= pe
                            || se > BAND_MAX_STOPBAND_START)
                    {
                        c.fail(
                            "cutoff.passband_end",
                            format!(
                                "needs {BAND_MIN_PASSBAND_END} <= passband end ({pe}) <= 1 and \
                                 passband end < stopband start ({se}) <= {BAND_MAX_STOPBAND_START}"
                            ),
                        );
                    }
                    // An absolute passband end replaces the fraction above,
                    // which stays the fallback at a source rate where the
                    // frequency does not fit the band
                    // (`design::band_passband_end`).
                    if let Some(hz) = b.passband_end_hz
                        && !(hz.is_finite() && hz > 0.0)
                    {
                        c.fail(
                            "cutoff.passband_end_hz",
                            format!("must be a frequency above 0 Hz (got {hz})"),
                        );
                    }
                    if !matches!(length, Length::Auto(_)) {
                        c.range("cutoff.attenuation_db", a, BAND_ATTENUATION_RANGE_DB);
                        if reflect && a.is_finite() && a > REFLECT_MAX_BAND_DB {
                            c.fail(
                                "cutoff.attenuation_db",
                                format!(
                                    "a reflected phase promises at most {REFLECT_MAX_BAND_DB} dB \
                                     (got {a})"
                                ),
                            );
                        }
                    }
                    if matches!(w, Window::Tukey { .. }) {
                        c.fail("cutoff.mode", "the Tukey window has no calibrated stopband");
                    }
                    if k.preserve_samples {
                        c.fail("preserve_samples", "needs the cutoff fraction 1.0");
                    }
                }
            }
            if k.preserve_samples && k.phase != Phase::Linear {
                c.fail("preserve_samples", "needs linear phase");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::design::catalogue;
    use audivane_protocol::FilterPreset;

    /// Every catalogue design is within the table.
    #[test]
    fn the_catalogue_passes() {
        for &p in FilterPreset::ALL {
            let d = catalogue::design(p);
            assert_eq!(check_all(&d), vec![], "{}", p.name());
        }
        assert_eq!(check_all(&catalogue::recommended()), vec![]);
    }
}
