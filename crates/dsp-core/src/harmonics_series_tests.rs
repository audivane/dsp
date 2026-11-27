//! Series routing of the harmonics stage ([`super`], "Series").

use super::dynamic_tests::{
    bias, cheb, dynamic, peak, preset_cheb, preset_layer, run, sag, sine, tilted_cheb,
    with_sign_flips,
};
use super::*;
use audivane_protocol::{HARMONICS_DEFAULT_SIGNS, HarmonicSign};

const P: HarmonicSign = HarmonicSign::Positive;
const N: HarmonicSign = HarmonicSign::Negative;

fn series(layers: Vec<HarmonicsLayer>) -> HarmonicsConfig {
    HarmonicsConfig {
        system: None,
        enabled: true,
        layers,
        dc_block: true,
        routing: HarmonicsRouting::Series,
    }
}

fn parallel(layers: Vec<HarmonicsLayer>) -> HarmonicsConfig {
    HarmonicsConfig {
        routing: HarmonicsRouting::Parallel,
        ..series(layers)
    }
}

/// H2-H5 at -20 dB: a strong static curve.
fn hot_cheb() -> HarmonicsLayer {
    HarmonicsLayer::Chebyshev(cheb(
        [
            Some(-20.0),
            Some(-20.0),
            Some(-20.0),
            Some(-20.0),
            None,
            None,
            None,
            None,
        ],
        HARMONICS_DEFAULT_SIGNS,
    ))
}

/// Every order at -20 dB, mixed signs.
fn every_order() -> HarmonicsLayer {
    HarmonicsLayer::Chebyshev(cheb([Some(-20.0); 8], [P, N, N, P, N, P, P, N]))
}

fn off(mut l: HarmonicsLayer) -> HarmonicsLayer {
    match &mut l {
        HarmonicsLayer::Chebyshev(c) => c.enabled = false,
        HarmonicsLayer::Flux(f) => f.enabled = false,
        HarmonicsLayer::Hysteresis(h) => h.enabled = false,
    }
    l
}

fn silent(mut l: HarmonicsLayer) -> HarmonicsLayer {
    *l.amount_mut().expect("a layer with an amount") = 0.0;
    l
}

fn noise(len: usize, seed: u64) -> Vec<f64> {
    let mut s = seed;
    (0..len)
        .map(|_| {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((s >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) * 0.9
        })
        .collect()
}

/// Held DC then release (tape remanence, the high-passes' charge), and DC
/// then a low full-scale tone.
fn held_dc(rate: f64) -> Vec<Vec<f64>> {
    let hold = (0.4 * rate) as usize;
    let gap = |s: f64| std::iter::repeat_n(0.0, (s * rate) as usize);
    let mut a = vec![1.0; hold];
    a.extend(gap(0.05));
    a.extend(std::iter::repeat_n(-1.0, 64));
    a.extend(gap(0.02));
    let mut b = vec![-1.0; hold];
    b.extend(std::iter::repeat_n(1.0, (0.01 * rate) as usize));
    b.extend(gap(0.05));
    let mut c = vec![0.5; hold];
    c.extend(sine(rate, 40.0, 1.0, 0.05));
    c.extend(gap(0.02));
    vec![a, b, c]
}

/// The series test chains: name, layers, the envelope times the signals
/// alternate at (sag attack, sag release, bias attack, bias release; s).
type TestChain = (&'static str, Vec<HarmonicsLayer>, (f64, f64, f64, f64));

fn chains() -> Vec<TestChain> {
    let fast = (1e-4, 1e-3, 1e-4, 5e-3);
    let slow = (0.02, 0.2, 0.03, 0.3);
    vec![
        (
            "chebyshev H2-H5 -20 dB -> tape_vintage",
            vec![hot_cheb(), preset_layer("tape_vintage")],
            slow,
        ),
        (
            "se_triode_high_mu -> tape_master",
            vec![
                preset_layer("se_triode_high_mu"),
                preset_layer("tape_master"),
            ],
            slow,
        ),
        (
            "tape_master -> tape_vintage",
            vec![preset_layer("tape_master"), preset_layer("tape_vintage")],
            slow,
        ),
        (
            "tilted sag/bias (fast) -> flux -> tape",
            vec![
                dynamic(
                    preset_cheb("pp_pentode_feedback"),
                    sag(6.0, 0.1, 1.0),
                    bias(0.25, 0.1, 5.0),
                ),
                preset_layer("output_transformer"),
                preset_layer("tape_studio"),
            ],
            fast,
        ),
        (
            "tilted sag/bias (slow) -> flux -> tape",
            vec![
                dynamic(
                    tilted_cheb(
                        preset_cheb("se_pentode"),
                        (60.0, 6.0),
                        (5_000.0, -6.0),
                        false,
                    ),
                    sag(3.0, 20.0, 200.0),
                    bias(-0.1, 30.0, 300.0),
                ),
                preset_layer("tape"),
                preset_layer("tape_vintage"),
            ],
            slow,
        ),
        (
            "five mixed: sag/bias, pentode, flux, tape, tilt",
            vec![
                dynamic(
                    preset_cheb("se_triode_high_mu"),
                    sag(3.0, 5.0, 50.0),
                    bias(0.1, 20.0, 200.0),
                ),
                preset_layer("pentode"),
                preset_layer("output_transformer"),
                preset_layer("tape_studio"),
                preset_layer("solid_state_feedback"),
            ],
            (0.005, 0.05, 0.02, 0.2),
        ),
        (
            "three strong curves",
            vec![hot_cheb(), every_order(), hot_cheb()],
            slow,
        ),
    ]
}

/// A series config with one active layer builds the parallel stage, and
/// the series runtime with that one layer runs it bit for bit too.
#[test]
fn one_active_layer_in_series_is_the_parallel_stage_bit_for_bit() {
    let cases = vec![
        vec![preset_layer("se_triode_high_mu")],
        vec![preset_layer("tape_vintage"), off(preset_layer("pentode"))],
        vec![
            silent(preset_layer("pentode")),
            dynamic(
                preset_cheb("pp_triode"),
                sag(3.0, 5.0, 50.0),
                bias(0.1, 20.0, 200.0),
            ),
            off(preset_layer("tape_master")),
        ],
        vec![off(hot_cheb()), preset_layer("output_transformer")],
        vec![
            preset_layer("pp_pentode_feedback"),
            silent(preset_layer("tape_master")),
        ],
    ];
    let mut input = noise(30_000, 7);
    input[100] = f64::NAN;
    input[200] = 1.3;
    input[201] = -1.7;
    input[300] = f64::INFINITY;
    for layers in cases {
        for rate in [44_100.0, 192_000.0] {
            let p_cfg = parallel(layers.clone());
            let s_cfg = series(layers.clone());
            let mut a = Harmonics::try_new(&p_cfg, rate, 2).unwrap();
            let mut b = Harmonics::try_new(&s_cfg, rate, 2).unwrap();
            assert!(!b.is_series());
            let one = Harmonics::series_layers(&s_cfg, rate, 2).unwrap();
            assert_eq!(one.len(), 1);
            let mut c = Harmonics::try_series(&s_cfg, rate, 2, one).unwrap();
            assert!(c.is_series());
            assert_eq!(a.peak_gain_db(), c.peak_gain_db());
            assert_eq!(a.small_signal_gain_db(), c.small_signal_gain_db());
            let mut out = Vec::new();
            for st in [&mut a, &mut b, &mut c] {
                let (mut l, mut r) = (input.clone(), noise(30_000, 9));
                let mut i = 0;
                let mut blk = 1;
                while i < l.len() {
                    let e = (i + blk).min(l.len());
                    st.process(&mut [&mut l[i..e], &mut r[i..e]]);
                    i = e;
                    blk = blk % 1021 + 31;
                }
                out.push((l, r));
            }
            let bits = |v: &[f64]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
            for o in &out[1..] {
                assert_eq!(bits(&o.0), bits(&out[0].0), "{layers:?} at {rate}");
                assert_eq!(bits(&o.1), bits(&out[0].1), "{layers:?} at {rate}");
            }
        }
    }
}

#[test]
fn series_keeps_silence_non_finite_input_and_overs() {
    let cfg = series(vec![
        preset_layer("se_triode_high_mu"),
        preset_layer("tape_studio"),
        preset_layer("output_transformer"),
    ]);
    let st = Harmonics::try_new(&cfg, 96_000.0, 1).unwrap();
    // Silence stays exactly zero.
    let mut s = st.clone();
    let mut buf = vec![0.0; 10_000];
    s.process(&mut [&mut buf]);
    assert!(buf.iter().all(|&x| x == 0.0));
    // After music, the tail never goes subnormal between or after the
    // layers, and settles to exact zero.
    let mut s = st.clone();
    let mut buf = noise(20_000, 12);
    buf.extend(std::iter::repeat_n(0.0, 400_000));
    let mut x = buf.clone();
    for layer in &st.series {
        let mut layer = layer.clone();
        layer.process(&mut [&mut x]);
        assert!(!x.iter().any(|v| v.is_subnormal()));
    }
    s.process(&mut [&mut buf]);
    assert!(!buf.iter().any(|v| v.is_subnormal()));
    assert!(buf[buf.len() - 1_000..].iter().all(|&v| v == 0.0));
    // Non-finite samples are zeros.
    let src = noise(20_000, 11);
    let mut with_nan = src.clone();
    let mut with_zero = src.clone();
    for i in [10, 500, 501, 9_000] {
        with_nan[i] = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::NAN][i % 4];
        with_zero[i] = 0.0;
    }
    let (mut a, mut b) = (st.clone(), st.clone());
    a.process(&mut [&mut with_nan]);
    b.process(&mut [&mut with_zero]);
    assert!(with_nan.iter().all(|x| x.is_finite()));
    assert_eq!(
        with_nan.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
        with_zero.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
    );
    // An over still comes out above the full-scale sample's output.
    let out_at = |v: f64| {
        let mut s = st.clone();
        let mut buf = vec![0.0; 100];
        buf[50] = v;
        s.process(&mut [&mut buf]);
        buf[50]
    };
    assert!(
        out_at(1.5) > out_at(1.0) + 0.1,
        "{} {}",
        out_at(1.5),
        out_at(1.0)
    );
}

/// A Chebyshev layer with every order at -20 dB under ±24 dB shelves: a
/// peak gain near +20 dB.
fn strong_tilt() -> HarmonicsLayer {
    HarmonicsLayer::Chebyshev(tilted_cheb(
        cheb([Some(-20.0); 8], HARMONICS_DEFAULT_SIGNS),
        (20.0, 24.0),
        (200.0, 24.0),
        true,
    ))
}

/// The worst inputs of the first layer's joint tilt bound (both signs,
/// [`JointBound::worst_input`]) followed by a held extreme and a jump: the
/// first layer's output at its largest, then what follows it.
fn first_layer_worst(cfg: &HarmonicsConfig, rate: f64) -> Vec<Vec<f64>> {
    let first = HarmonicsConfig {
        routing: HarmonicsRouting::Parallel,
        layers: cfg.layers[..1].to_vec(),
        ..cfg.clone()
    };
    let (flat, filtered) = split_terms(&first, rate).unwrap();
    if filtered.is_empty() {
        return Vec::new();
    }
    let mut a = [0.0; LEN];
    a[2..].copy_from_slice(&flat);
    let total = amplitudes(&first).unwrap();
    let linear = 1.0
        + (3..LEN)
            .step_by(2)
            .map(|k| total[k - 2] * T_SLOPE_0[k])
            .sum::<f64>();
    let st = Harmonics::try_new(&first, rate, 1).unwrap();
    let hpf = (st.bank.hpf_b0, st.bank.hpf_a1);
    let terms = joint_terms(&a, &filtered, cfg.dc_block);
    let n = (0.5 * rate) as usize;
    [true, false]
        .into_iter()
        .map(|positive| {
            let mut v = JointBound::worst_input(&terms, hpf, linear, n, positive);
            let last = v[v.len() - 1];
            v.extend(std::iter::repeat_n(last, (0.01 * rate) as usize));
            v.extend(std::iter::repeat_n(-last.signum(), (0.01 * rate) as usize));
            v
        })
        .collect()
}

/// The bound battery: every chain (and a strongly tilted layer into tape)
/// within its certified bound at both rate extremes, on the adversarial
/// signals and the first layer's own worst inputs; each layer's output
/// within the range the stage hands on (module docs, "Series", "Bound").
#[test]
fn series_output_stays_within_the_bound() {
    let mut all = chains();
    all.push((
        "strong tilt -> tape_vintage",
        vec![strong_tilt(), preset_layer("tape_vintage")],
        (0.02, 0.2, 0.03, 0.3),
    ));
    all.push((
        "strong tilt -> hot curve -> flux",
        vec![
            strong_tilt(),
            hot_cheb(),
            preset_layer("output_transformer"),
        ],
        (0.02, 0.2, 0.03, 0.3),
    ));
    let mut report = Vec::new();
    for (name, layers, times) in all {
        let cfg = series(layers);
        for rate in [44_100.0, 768_000.0] {
            let st = Harmonics::try_new(&cfg, rate, 1).unwrap();
            assert!(st.is_series(), "{name}");
            let n = st.series.len();
            // The ranges each layer hands on.
            let bounds = st.peak_bounds();
            let ranges: Vec<f64> = (1..=n).map(|j| peak_reach(&bounds[..j], 1.0)).collect();
            let total = db_to_lin(st.peak_gain_db());
            assert!(
                (ranges[n - 1] / total - 1.0).abs() < 1e-12,
                "{name}: {ranges:?}"
            );
            let mut signals = with_sign_flips(rate, times);
            signals.extend(held_dc(rate));
            signals.extend(first_layer_worst(&cfg, rate));
            let mut worst: f64 = 0.0;
            let mut worst_layer = vec![0.0_f64; n];
            for (i, v) in signals.into_iter().enumerate() {
                let mut x = v.clone();
                for (j, layer) in st.series.iter().enumerate() {
                    let mut layer = layer.clone();
                    run(&mut layer, &mut x);
                    let p = peak(&x);
                    assert!(
                        p <= ranges[j],
                        "{name} at {rate}: layer {j} on signal {i}: {p} > {}",
                        ranges[j]
                    );
                    worst_layer[j] = worst_layer[j].max(p);
                }
                let mut y = v;
                let mut whole = st.clone();
                run(&mut whole, &mut y);
                assert_eq!(
                    y.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
                    x.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
                    "{name}: the stage runs its layers in order, unscaled"
                );
                let p = peak(&y);
                assert!(
                    p <= total,
                    "{name} at {rate}: signal {i} peak {p} > {total}"
                );
                worst = worst.max(p);
            }
            let layers: Vec<String> = worst_layer.iter().map(|p| format!("{p:.3}")).collect();
            let mut par = cfg.clone();
            par.routing = HarmonicsRouting::Parallel;
            let par_gain = Harmonics::try_new(&par, rate, 1).unwrap().peak_gain_db();
            report.push(format!(
                "{name} @ {rate}: peak gain {:+.2} dB (parallel {par_gain:+.2}); worst peak \
                 {worst:.4} ({:.2} dB under the bound); per layer [{}]",
                st.peak_gain_db(),
                lin_to_db(total / worst),
                layers.join(", ")
            ));
        }
    }
    eprintln!("{}", report.join("\n"));
}
