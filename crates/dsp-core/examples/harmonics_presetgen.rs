//! Offline harmonic-preset generator: device equations to Chebyshev tables.
//!
//! For each preset the static transfer curve is solved over the stated grid
//! swing, projected onto Chebyshev terms `T2..T9` with a DCT at Chebyshev
//! nodes, and normalised so the fundamental of a full-scale sine is 1. The
//! printed `h2..h9` values are then the signed levels in dBc at 0 dBFS (the
//! model is static: no frequency dependence). The levels at -6, -12 and -20
//! dBFS follow from the same table (parallel Chebyshev terms, linear path
//! calibrated for unit full-scale fundamental), so they check the level law
//! the engine will actually produce.
//!
//! Run:
//!
//! ```text
//! cargo run --release -p audivane-dsp-core --example harmonics_presetgen
//! cargo test -p audivane-dsp-core --example harmonics_presetgen
//! ```
//!
//! Output is JSON on stdout:
//!
//! ```text
//! { "schema": ..., "reference": ..., "conventions": ..., "presets": [ ... ] }
//! ```
//!
//! Signed dBc convention: amplitude `a_k = -sign(x) * 10^(-|x|/20)`; a negative
//! value is the same polarity as the fundamental, a positive value is inverted.
//! The values are floored at the engine's -120 dB minimum.
//!
//! Tube equations and parameters: Norman Koren, "Improved vacuum tube models
//! for SPICE simulations", Table 1,
//! <https://www.normankoren.com/Audio/Tubemodspice_article.html>. The page
//! carries a copyright notice ("Images and text copyright (C) 2000-2012 by
//! Norman Koren") but states no licence or terms of use. Only the published
//! equations and numeric parameter values are used here, with attribution;
//! no text or images are copied.

use std::f64::consts::PI;
use std::fmt::Write as _;

/// Highest harmonic order in a preset.
const ORDERS: usize = 9;
/// Chebyshev nodes in the DCT. Ten terms need only ten; 64 leaves room to see
/// a non-polynomial device curve's residual, and keeps the projection exact
/// for any curve of degree <= 63.
const NODES: usize = 64;
/// Floor in dB, matching `HARMONICS_MIN_DB` in the protocol.
const MIN_DB: f64 = -120.0;
/// Largest Taylor term used by the independent `tanh` check.
#[cfg(test)]
const TANH_SERIES_TERMS: usize = 81;

const KOREN_URL: &str = "https://www.normankoren.com/Audio/Tubemodspice_article.html (Table 1)";

// --------------------------------------------------------------- device models

/// Koren triode parameters from the article's Table 1.
#[derive(Clone, Copy)]
struct Triode {
    kind: &'static str,
    mu: f64,
    ex: f64,
    kg1: f64,
    kp: f64,
    kvb: f64,
}

/// Koren pentode parameters from the article's Table 1. `kg2` only affects
/// screen current, which the plate-current equation does not use; it is kept
/// so the source line names the full published set.
#[derive(Clone, Copy)]
struct Pentode {
    kind: &'static str,
    mu: f64,
    ex: f64,
    kg1: f64,
    kg2: f64,
    kp: f64,
    kvb: f64,
}

const T12AX7: Triode = Triode {
    kind: "12AX7",
    mu: 100.0,
    ex: 1.4,
    kg1: 1060.0,
    kp: 600.0,
    kvb: 300.0,
};

const T6DJ8: Triode = Triode {
    kind: "6DJ8",
    mu: 28.0,
    ex: 1.3,
    kg1: 330.0,
    kp: 320.0,
    kvb: 300.0,
};

const T12AU7: Triode = Triode {
    kind: "12AU7",
    mu: 21.5,
    ex: 1.3,
    kg1: 1180.0,
    kp: 84.0,
    kvb: 300.0,
};

const P6550: Pentode = Pentode {
    kind: "6550",
    mu: 7.9,
    ex: 1.35,
    kg1: 890.0,
    kg2: 4800.0,
    kp: 60.0,
    kvb: 24.0,
};

/// `log(1 + exp(x))` without overflow on either side.
fn softplus(x: f64) -> f64 {
    if x > 0.0 {
        x + (-x).exp().ln_1p()
    } else {
        x.exp().ln_1p()
    }
}

impl Triode {
    /// Koren triode plate current, equation (4) of the article. The published
    /// `(1 + sgn(E1))` factor is kept as written; some SPICE libraries halve it,
    /// which scales current and moves the load-line operating point.
    fn plate_current(&self, vp: f64, vg: f64) -> f64 {
        let e1 =
            (vp / self.kp) * softplus(self.kp * (1.0 / self.mu + vg / (self.kvb + vp * vp).sqrt()));
        if e1 <= 0.0 {
            0.0
        } else {
            2.0 * e1.powf(self.ex) / self.kg1
        }
    }

    /// Plate voltage on a resistive load line: solve `vp + ip(vp, vg) R = V+`.
    fn plate_voltage(&self, supply: f64, load: f64, vg: f64) -> f64 {
        solve_plate(supply, load, |vp| self.plate_current(vp, vg))
    }

    fn source(&self) -> String {
        format!(
            "Koren triode model for {} (mu={}, ex={}, kg1={}, kp={}, kvb={})",
            self.kind, self.mu, self.ex, self.kg1, self.kp, self.kvb
        )
    }
}

impl Pentode {
    /// Koren pentode plate current, equation (5) of the article.
    fn plate_current(&self, vp: f64, vg: f64, screen: f64) -> f64 {
        let e1 = (screen / self.kp) * softplus(self.kp * (1.0 / self.mu + vg / screen));
        if e1 <= 0.0 {
            0.0
        } else {
            2.0 * e1.powf(self.ex) / self.kg1 * (vp / self.kvb).atan()
        }
    }

    fn plate_voltage(&self, supply: f64, load: f64, vg: f64, screen: f64) -> f64 {
        solve_plate(supply, load, |vp| self.plate_current(vp, vg, screen))
    }

    fn source(&self) -> String {
        format!(
            "Koren pentode model for {} (mu={}, ex={}, kg1={}, kg2={} [screen current, \
             not used], kp={}, kvb={})",
            self.kind, self.mu, self.ex, self.kg1, self.kg2, self.kp, self.kvb
        )
    }
}

/// Solve `vp + current(vp) * load = supply` by bisection. The plate current is
/// monotonic in `vp` for both models, and the grid current is ignored (the
/// stated swings stay below 0 V).
fn solve_plate<F>(supply: f64, load: f64, current: F) -> f64
where
    F: Fn(f64) -> f64,
{
    let (mut lo, mut hi) = (0.0, supply);
    for _ in 0..100 {
        let vp = 0.5 * (lo + hi);
        if vp + current(vp) * load < supply {
            lo = vp;
        } else {
            hi = vp;
        }
    }
    0.5 * (lo + hi)
}

// ------------------------------------------------------- Chebyshev projection

/// DCT-II at Chebyshev nodes: `a[j] = 2/N sum f(cos theta_i) cos(j theta_i)`,
/// with `a[0]` halved. For a curve of degree <= N-1 this is exact.
fn dct<F>(curve: F) -> [f64; ORDERS + 1]
where
    F: Fn(f64) -> f64,
{
    let mut a = [0.0; ORDERS + 1];
    for (j, slot) in a.iter_mut().enumerate() {
        let mut sum = 0.0;
        for i in 0..NODES {
            let theta = PI * (i as f64 + 0.5) / NODES as f64;
            sum += curve(theta.cos()) * (j as f64 * theta).cos();
        }
        *slot = 2.0 * sum / NODES as f64;
    }
    a[0] *= 0.5;
    a
}

/// Divide by the fundamental so a full-scale sine has unit fundamental. The
/// DC term is dropped: the engine removes it per even order.
fn normalized_orders(mut a: [f64; ORDERS + 1]) -> [f64; ORDERS + 1] {
    let a1 = a[1];
    assert!(a1.abs() > 1e-12, "fundamental vanished in projection");
    for value in &mut a {
        *value /= a1;
    }
    a[0] = 0.0;
    a
}

/// Chebyshev sum `c[0] + c[1] T1(x) + ...`, used by the round-trip test.
#[cfg(test)]
fn chebyshev_sum(c: &[f64; ORDERS + 1], x: f64) -> f64 {
    let mut sum = c[0];
    let (mut t_prev, mut t_cur) = (1.0, x);
    sum += c[1] * t_cur;
    for &ck in c.iter().skip(2) {
        let t_next = 2.0 * x * t_cur - t_prev;
        sum += ck * t_next;
        t_prev = t_cur;
        t_cur = t_next;
    }
    sum
}

/// `T_k(0)`: 0 for odd k, `(-1)^(k/2)` for even k.
fn cheb_at_zero(k: usize) -> f64 {
    if !k.is_multiple_of(2) {
        return 0.0;
    }
    if (k / 2).is_multiple_of(2) { 1.0 } else { -1.0 }
}

/// `T_k'(0)`: 0 for even k, `k (-1)^((k-1)/2)` for odd k.
fn cheb_deriv_at_zero(k: usize) -> f64 {
    if k.is_multiple_of(2) {
        0.0
    } else if ((k - 1) / 2).is_multiple_of(2) {
        k as f64
    } else {
        -(k as f64)
    }
}

/// Static curve of the engine's Chebyshev layer. With
/// `L = 1 + sum_odd h_k T_k'(0)` and `D_k = T_k - T_k'(0) x - T_k(0)`,
/// `y = L x + sum_{k=2..9} h_k D_k(x)`, which equals
/// `x + sum h_k T_k(x)` minus the even terms' DC. A full-scale sine then has
/// unit fundamental and `h_k` at order k.
fn model_output(h: &[f64; ORDERS + 1], x: f64) -> f64 {
    // The direct path carries the linear parts of the odd distortion terms, so
    // that a full-scale sine keeps unit fundamental.
    let linear_gain = 1.0
        + (3..=ORDERS)
            .step_by(2)
            .map(|k| h[k] * cheb_deriv_at_zero(k))
            .sum::<f64>();
    let mut y = linear_gain * x;
    let (mut t_prev, mut t_cur) = (1.0, x);
    for (k, &hk) in h.iter().enumerate().skip(2) {
        let t_k = 2.0 * x * t_cur - t_prev;
        y += hk * (t_k - cheb_deriv_at_zero(k) * x - cheb_at_zero(k));
        t_prev = t_cur;
        t_cur = t_k;
    }
    y
}

/// Harmonic amplitudes of the model for an input amplitude `a` (linear scale).
fn model_harmonics(h: &[f64; ORDERS + 1], a: f64) -> [f64; ORDERS + 1] {
    dct(|x| model_output(h, a * x))
}

/// Signed dB of an amplitude relative to the fundamental. Positive dB means
/// the harmonic is inverted relative to the fundamental; a value at or below
/// -120 dB is floored at -120.
fn signed_db(amplitude: f64) -> f64 {
    let magnitude = amplitude.abs();
    if magnitude < 10f64.powf(MIN_DB / 20.0) {
        return MIN_DB;
    }
    let db = 20.0 * magnitude.log10();
    let signed = if amplitude < 0.0 { -db } else { db };
    signed.clamp(MIN_DB, -MIN_DB)
}

// ------------------------------------------------------------------- presets

struct LevelCheck {
    input_dbfs: f64,
    fundamental_db: f64,
    h: [f64; ORDERS + 1],
}

struct Preset {
    name: &'static str,
    device: &'static str,
    operating_point: Vec<(&'static str, f64)>,
    h: [f64; ORDERS + 1],
    checks: Vec<LevelCheck>,
    source: String,
}

/// Project `curve(x)` (x = -1..1 is the stated grid swing) and build a preset.
fn preset_from_curve<F>(
    name: &'static str,
    device: &'static str,
    operating_point: Vec<(&'static str, f64)>,
    source: String,
    curve: F,
) -> Preset
where
    F: Fn(f64) -> f64,
{
    let h = normalized_orders(dct(curve));
    let checks = [-6.0, -12.0, -20.0]
        .into_iter()
        .map(|input_dbfs| {
            let a = model_harmonics(&h, 10f64.powf(input_dbfs / 20.0));
            LevelCheck {
                input_dbfs,
                fundamental_db: 20.0 * a[1].abs().log10(),
                h: std::array::from_fn(|k| if k < 2 { 0.0 } else { a[k] / a[1] }),
            }
        })
        .collect();
    Preset {
        name,
        device,
        operating_point,
        h,
        checks,
        source,
    }
}

/// Single-ended triode common-cathode stage with a resistive plate load.
fn triode_stage(
    name: &'static str,
    tube: Triode,
    supply_v: f64,
    load_ohm: f64,
    bias_v: f64,
    swing_v: f64,
) -> Preset {
    let plate = |x: f64| tube.plate_voltage(supply_v, load_ohm, bias_v + swing_v * x);
    let vp_q = plate(0.0);
    let ip_q_ma = (supply_v - vp_q) / load_ohm * 1000.0;
    let source = format!(
        "{}; common cathode with resistive load, supply {} V, plate load {} kohm, \
         fixed bias {} V, grid swing +/-{} V peak = 0 dBFS (grid current ignored); \
         quiescent plate {:.1} V at {:.2} mA; {}",
        tube.source(),
        supply_v,
        load_ohm / 1000.0,
        bias_v,
        swing_v,
        vp_q,
        ip_q_ma,
        KOREN_URL
    );
    preset_from_curve(
        name,
        "triode common cathode",
        vec![
            ("supply_v", supply_v),
            ("load_ohm", load_ohm),
            ("bias_v", bias_v),
            ("grid_swing_v_peak", swing_v),
            ("quiescent_plate_v", vp_q),
            ("quiescent_current_ma", ip_q_ma),
        ],
        source,
        plate,
    )
}

/// Single-ended pentode common-cathode stage with a resistive plate load and a
/// fixed screen supply.
fn pentode_stage(
    name: &'static str,
    tube: Pentode,
    supply_v: f64,
    screen_v: f64,
    load_ohm: f64,
    bias_v: f64,
    swing_v: f64,
) -> Preset {
    let plate = |x: f64| tube.plate_voltage(supply_v, load_ohm, bias_v + swing_v * x, screen_v);
    let vp_q = plate(0.0);
    let ip_q_ma = (supply_v - vp_q) / load_ohm * 1000.0;
    let source = format!(
        "{}; common cathode with resistive load, supply {} V, screen {} V, plate load \
         {} kohm, fixed bias {} V, grid swing +/-{} V peak = 0 dBFS (grid current \
         ignored); quiescent plate {:.1} V at {:.2} mA; {}",
        tube.source(),
        supply_v,
        screen_v,
        load_ohm / 1000.0,
        bias_v,
        swing_v,
        vp_q,
        ip_q_ma,
        KOREN_URL
    );
    preset_from_curve(
        name,
        "pentode common cathode",
        vec![
            ("supply_v", supply_v),
            ("screen_v", screen_v),
            ("load_ohm", load_ohm),
            ("bias_v", bias_v),
            ("grid_swing_v_peak", swing_v),
            ("quiescent_plate_v", vp_q),
            ("quiescent_current_ma", ip_q_ma),
        ],
        source,
        plate,
    )
}

/// Two triode halves in difference. `second_half_gain_db` is the gain error of
/// the inverting half: 0 dB is perfectly balanced, even orders cancel.
fn push_pull_stage(
    name: &'static str,
    tube: Triode,
    supply_v: f64,
    load_ohm_per_half: f64,
    bias_v: f64,
    swing_v: f64,
    second_half_gain_db: f64,
) -> Preset {
    let half = |x: f64| tube.plate_voltage(supply_v, load_ohm_per_half, bias_v + swing_v * x);
    let gain = 10f64.powf(second_half_gain_db / 20.0);
    let vp_q = half(0.0);
    let ip_q_ma = (supply_v - vp_q) / load_ohm_per_half * 1000.0;
    let source = format!(
        "{}; two halves in difference, supply {} V, {} kohm per half, fixed bias {} V, \
         grid swing +/-{} V peak per half = 0 dBFS (grid current ignored); \
         second-half gain {:+.1} dB; quiescent plate {:.1} V at {:.2} mA per half; {}",
        tube.source(),
        supply_v,
        load_ohm_per_half / 1000.0,
        bias_v,
        swing_v,
        second_half_gain_db,
        vp_q,
        ip_q_ma,
        KOREN_URL
    );
    preset_from_curve(
        name,
        "push-pull triode pair",
        vec![
            ("halves", 2.0),
            ("supply_v", supply_v),
            ("load_ohm_per_half", load_ohm_per_half),
            ("bias_v", bias_v),
            ("grid_swing_v_peak", swing_v),
            ("second_half_gain_db", second_half_gain_db),
            ("quiescent_plate_v", vp_q),
            ("quiescent_current_ma", ip_q_ma),
        ],
        source,
        move |x| half(x) - gain * half(-x),
    )
}

/// Ideal differential pair: `y = tanh(g x)`, odd orders only.
fn differential_pair(name: &'static str, gain: f64) -> Preset {
    let thermal_mv = 25.85;
    let full_scale_mv = 2.0 * thermal_mv * gain;
    let source = format!(
        "Ideal differential pair (exponential BJT pair), y = tanh(g x), g = {}, \
         x = v_id / (2 V_T), 2 V_T = {:.2} mV at 300 K; full-scale input {:.1} mV peak \
         = 0 dBFS; odd orders only, no DC",
        gain,
        2.0 * thermal_mv,
        full_scale_mv
    );
    preset_from_curve(
        name,
        "differential pair",
        vec![
            ("tanh_gain", gain),
            ("thermal_voltage_mv", thermal_mv),
            ("input_full_scale_mv_peak", full_scale_mv),
        ],
        source,
        move |x| (gain * x).tanh(),
    )
}

/// Operating points: the grid swings up to the bias (grid-current onset)
/// where the stage is otherwise too clean to hear at programme levels
/// (music sits 15-20 dB below full scale).
fn presets() -> Vec<Preset> {
    vec![
        triode_stage(
            "single-ended triode, high-mu",
            T12AX7,
            300.0,
            100_000.0,
            -1.5,
            1.0,
        ),
        triode_stage(
            "single-ended triode, low-mu",
            T6DJ8,
            250.0,
            10_000.0,
            -3.0,
            3.0,
        ),
        pentode_stage(
            "single-ended pentode",
            P6550,
            400.0,
            250.0,
            3000.0,
            -24.0,
            4.0,
        ),
        push_pull_stage(
            "push-pull triode, balanced",
            T12AU7,
            300.0,
            20_000.0,
            -8.0,
            8.0,
            0.0,
        ),
        push_pull_stage(
            "push-pull triode, 1 dB imbalance",
            T12AU7,
            300.0,
            20_000.0,
            -8.0,
            8.0,
            1.0,
        ),
        differential_pair("differential pair, odd-order", 1.0),
    ]
}

// ---------------------------------------------------------------------- JSON

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => write!(out, "\\u{:04x}", c as u32).unwrap(),
            c => out.push(c),
        }
    }
    out
}

/// Fixed-point JSON number; `-0.0` is normalised so the output has no `-0.000`.
fn num(v: f64, decimals: usize) -> String {
    let v = if v == 0.0 { 0.0 } else { v };
    if v.is_finite() {
        format!("{:.*}", decimals, v)
    } else {
        "null".to_string()
    }
}

fn harmonics_json(h: &[f64; ORDERS + 1]) -> String {
    let mut out = String::from("{");
    for (k, &hk) in h.iter().enumerate().skip(2) {
        if k > 2 {
            out.push_str(", ");
        }
        write!(out, "\"h{k}\": {}", num(signed_db(hk), 3)).unwrap();
    }
    out.push('}');
    out
}

fn operating_point_json(op: &[(&str, f64)]) -> String {
    let mut out = String::from("{");
    for (i, (key, value)) in op.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write!(out, "\"{key}\": {}", num(*value, 3)).unwrap();
    }
    out.push('}');
    out
}

fn emit_json(presets: &[Preset]) -> String {
    let mut out = String::new();
    writeln!(out, "{{").unwrap();
    writeln!(out, "  \"schema\": \"audivane-harmonics-presetgen/1\",").unwrap();
    writeln!(
        out,
        "  \"reference\": {{\"level_dbfs\": 0.000, \"frequency_hz\": 1000.000}},"
    )
    .unwrap();
    writeln!(out, "  \"conventions\": {{").unwrap();
    writeln!(
        out,
        "    \"harmonic_levels\": \"signed dBc at 0 dBFS relative to the fundamental: \
         amplitude = -sign(x) * 10^(-|x|/20); negative = same polarity, positive = \
         inverted; -120 = floored at the engine's minimum\","
    )
    .unwrap();
    writeln!(
        out,
        "    \"fundamental_db\": \"dB relative to the full-scale fundamental; it can sit \
         above the input dBFS at low levels because the table is normalised at 0 dBFS\","
    )
    .unwrap();
    writeln!(
        out,
        "    \"levels_at_dbfs\": \"model levels from the same table; not a re-fit at each \
         input level\","
    )
    .unwrap();
    writeln!(
        out,
        "    \"model\": \"parallel Chebyshev terms T2..T9 with the linear path calibrated \
         so a full-scale sine has unit fundamental (wave-2 layer; static, \
         frequency-independent)\""
    )
    .unwrap();
    writeln!(out, "  }},").unwrap();
    writeln!(out, "  \"presets\": [").unwrap();
    for (i, preset) in presets.iter().enumerate() {
        writeln!(out, "    {{").unwrap();
        writeln!(out, "      \"name\": \"{}\",", json_escape(preset.name)).unwrap();
        writeln!(out, "      \"device\": \"{}\",", json_escape(preset.device)).unwrap();
        writeln!(
            out,
            "      \"operating_point\": {},",
            operating_point_json(&preset.operating_point)
        )
        .unwrap();
        writeln!(out, "      \"harmonics\": {},", harmonics_json(&preset.h)).unwrap();
        writeln!(out, "      \"levels_at_dbfs\": [").unwrap();
        for (j, check) in preset.checks.iter().enumerate() {
            write!(
                out,
                "        {{\"input_dbfs\": {}, \"fundamental_db\": {}, \"harmonics\": {}}}",
                num(check.input_dbfs, 1),
                num(check.fundamental_db, 3),
                harmonics_json(&check.h)
            )
            .unwrap();
            writeln!(
                out,
                "{}",
                if j + 1 < preset.checks.len() { "," } else { "" }
            )
            .unwrap();
        }
        writeln!(out, "      ],").unwrap();
        writeln!(out, "      \"source\": \"{}\"", json_escape(&preset.source)).unwrap();
        writeln!(
            out,
            "    }}{}",
            if i + 1 < presets.len() { "," } else { "" }
        )
        .unwrap();
    }
    writeln!(out, "  ]").unwrap();
    writeln!(out, "}}").unwrap();
    out
}

fn main() {
    print!("{}", emit_json(&presets()));
}

// ---------------------------------------------------------------------- tests

/// Coefficients of `tanh x`: `tanh x = sum c_n x^n`, n odd. From
/// `c_n = 2^(n+1) (2^(n+1) - 1) B_(n+1) / (n+1)!`.
#[cfg(test)]
fn tanh_taylor_coeff(n: usize) -> f64 {
    debug_assert!(!n.is_multiple_of(2));
    let two_pow = 2f64.powi((n + 1) as i32);
    two_pow * (two_pow - 1.0) * bernoulli_even(n + 1) / factorial(n + 1)
}

/// `B_m` for even `m >= 2`, via `B_(2k) = (-1)^(k+1) 2 (2k)! zeta(2k) / (2 pi)^(2k)`.
/// The first four are exact literals; for the rest the zeta sum to 32 terms is
/// accurate to well below 1e-15 (zeta(2k) is 1 + 2^-2k + ...).
#[cfg(test)]
fn bernoulli_even(m: usize) -> f64 {
    debug_assert!(m.is_multiple_of(2) && m >= 2);
    match m {
        2 => 1.0 / 6.0,
        4 => -1.0 / 30.0,
        6 => 1.0 / 42.0,
        8 => -1.0 / 30.0,
        _ => {
            let k = m / 2;
            let zeta: f64 = (1..=32).map(|i| (i as f64).powi(-(m as i32))).sum();
            let sign = if !k.is_multiple_of(2) { 1.0 } else { -1.0 };
            sign * 2.0 * factorial(m) * zeta / (2.0 * PI).powi(m as i32)
        }
    }
}

#[cfg(test)]
fn factorial(n: usize) -> f64 {
    (1..=n).map(|i| i as f64).product()
}

#[cfg(test)]
fn binomial(n: usize, k: usize) -> f64 {
    let k = k.min(n - k);
    (0..k).fold(1.0, |acc, i| acc * (n - i) as f64 / (i + 1) as f64)
}

/// Closed-form odd harmonics of `tanh(g cos theta)`, normalised to H1.
/// `cos^n theta = 2^(1-n) sum_j C(n,j) cos((n-2j) theta)` for odd n, so the
/// amplitude of harmonic k is `sum_n c_n g^n 2^(1-n) C(n,(n-k)/2)`. The series
/// radius is pi/2; at g = 1.0, 81 terms leave truncation far below 1e-15.
#[cfg(test)]
fn tanh_harmonics(gain: f64) -> [f64; ORDERS + 1] {
    let coeff: Vec<f64> = (0..=TANH_SERIES_TERMS)
        .map(|n| {
            if !n.is_multiple_of(2) {
                tanh_taylor_coeff(n)
            } else {
                0.0
            }
        })
        .collect();
    let mut h = [0.0; ORDERS + 1];
    for k in (1..=ORDERS).step_by(2) {
        let mut sum = 0.0;
        for n in (k..=TANH_SERIES_TERMS).step_by(2) {
            let j = (n - k) / 2;
            sum += coeff[n] * gain.powi(n as i32) * 2f64.powi(1 - n as i32) * binomial(n, j);
        }
        h[k] = sum;
    }
    let h1 = h[1];
    for value in &mut h {
        *value /= h1;
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The computed presets shipped in the protocol crate's
    /// `harmonics_presets.json` are this generator's output, converted to
    /// the engine's convention (level `-|x|`, sign `+` for a negative `x`,
    /// floored orders off).
    #[test]
    fn shipped_presets_match_the_generator() {
        use audivane_protocol::{HarmonicSign, HarmonicsLayer, harmonics_preset};
        let ids = [
            ("single-ended triode, high-mu", "se_triode_high_mu"),
            ("single-ended triode, low-mu", "se_triode_low_mu"),
            ("single-ended pentode", "se_pentode"),
            ("push-pull triode, balanced", "pp_triode"),
            ("push-pull triode, 1 dB imbalance", "pp_triode_imbalanced"),
            ("differential pair, odd-order", "diff_pair"),
        ];
        let generated = presets();
        assert_eq!(generated.len(), ids.len());
        for p in &generated {
            let id = ids.iter().find(|(n, _)| *n == p.name).unwrap().1;
            let shipped = harmonics_preset(id).unwrap_or_else(|| panic!("{id} missing"));
            assert!(shipped.source.contains(&p.source), "{id}: source");
            let HarmonicsLayer::Chebyshev(l) = &shipped.layer else {
                panic!("{id} is not a Chebyshev layer")
            };
            for k in 2..=ORDERS {
                let x = signed_db(p.h[k]);
                let (level, sign) = (l.levels_db[k - 2], l.signs[k - 2]);
                if x.abs() >= -MIN_DB {
                    assert_eq!(level, None, "{id} H{k}");
                } else {
                    let level = level.unwrap_or_else(|| panic!("{id} H{k} off"));
                    assert!((level + x.abs()).abs() <= 0.05, "{id} H{k}: {level} vs {x}");
                    let want = if x < 0.0 {
                        HarmonicSign::Positive
                    } else {
                        HarmonicSign::Negative
                    };
                    assert_eq!(sign, want, "{id} H{k}");
                }
            }
        }
    }
}
