//! Series routing of the harmonics stage ([`super`], "Series").

use super::dynamic_tests::{
    bias, brute_under_load, cheb, dynamic, peak, preset_cheb, preset_layer, run, sag, sine,
    tilted_cheb, with_sign_flips,
};
use super::*;
use crate::loudness::stage_loudness_change_db;
use audivane_protocol::{FluxLayer, HARMONICS_DEFAULT_SIGNS, HarmonicSign};

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

/// Two H3-only layers: in series their composition makes H5 and H9 (`T3 ∘
/// T3 = T9`), which the parallel sum never has; the peak bounds compose.
#[test]
fn series_makes_the_cross_terms_and_composes_the_bounds() {
    let h3 = HarmonicsLayer::Chebyshev(cheb(
        [None, Some(-20.0), None, None, None, None, None, None],
        HARMONICS_DEFAULT_SIGNS,
    ));
    let rate = 96_000.0;
    let rp = readout_with(
        &parallel(vec![h3.clone(), h3.clone()]),
        rate,
        false,
        &ReadoutCache::new(),
    )
    .unwrap();
    let rs = readout_with(
        &series(vec![h3.clone(), h3.clone()]),
        rate,
        false,
        &ReadoutCache::new(),
    )
    .unwrap();
    let (p, s) = (&rp.points[0], &rs.points[0]);
    assert_eq!((p.freq_hz, p.input_dbfs), (1_000.0, 0.0));
    // H5 (index 3) and H9 (index 7).
    assert!(
        p.harmonics_db[3] < -150.0 && p.harmonics_db[7] < -150.0,
        "{p:?}"
    );
    assert!(s.harmonics_db[3] > -40.0, "{s:?}");
    assert!(s.harmonics_db[7] > -100.0, "{s:?}");
    // About twice the H3 of one layer in either routing (−14 dB).
    assert!(
        (s.harmonics_db[1] - p.harmonics_db[1]).abs() < 3.0,
        "{p:?}\n{s:?}"
    );

    // Hot layers that each go over full scale: the stage's bound composes
    // theirs (module docs, "Series", "Bound"), past their sum in dB.
    let cfg = series(vec![hot_cheb(), every_order()]);
    let st = Harmonics::try_new(&cfg, rate, 1).unwrap();
    let alone: Vec<f64> = (0..2)
        .map(|i| {
            Harmonics::try_new(&only_layer(&cfg, i), rate, 1)
                .unwrap()
                .peak_bounds()[0]
        })
        .collect();
    assert!(alone.iter().all(|b| *b > 1.0), "{alone:?}");
    assert_eq!(st.peak_bounds(), alone);
    let want = lin_to_db(peak_reach(&alone, 1.0));
    assert!((st.peak_gain_db() - want).abs() < 1e-9);
}

/// Largest difference of two points, dB (harmonics below -120 dBc in both
/// count as equal; GedLee `Gm` relative).
fn delta(a: &HarmonicsReadoutPoint, b: &HarmonicsReadoutPoint) -> f64 {
    delta_above(a, b, -120.0)
}

/// As [`delta`], counting harmonics above `floor` dBc only.
fn delta_above(a: &HarmonicsReadoutPoint, b: &HarmonicsReadoutPoint, floor: f64) -> f64 {
    let mut d = (a.thd_db - b.thd_db)
        .abs()
        .max((a.gain_db - b.gain_db).abs());
    for (x, y) in a.harmonics_db.iter().zip(&b.harmonics_db) {
        if x.max(*y) > floor {
            d = d.max((x - y).abs());
        }
    }
    let gm = (a.gedlee_gm - b.gedlee_gm).abs() / a.gedlee_gm.abs().max(1e-3);
    d.max(lin_to_db(1.0 + gm))
}

/// The point `p`'s tone through `stage` (built at `rate`) from rest, settled
/// for `settle_s`, in odd blocks.
fn direct_point(
    stage: &Harmonics,
    rate: f64,
    p: &HarmonicsReadoutPoint,
    settle_s: f64,
    measure_s: f64,
) -> HarmonicsReadoutPoint {
    let amp = db_to_lin(p.input_dbfs);
    let (mut buf, settle, n, w) = tone_signal(rate, p.freq_hz, amp, settle_s, measure_s);
    let mut st = stage.clone();
    st.reset();
    run(&mut st, &mut buf);
    let h = coherent_dft(&buf[settle..], n, w);
    spectrum_point(p.input_dbfs, p.freq_hz, amp, &h, rate)
}

/// Static chains: every readout point against the stage run directly at
/// the readout's rate (96 kHz: no rate change), and the level change against
/// the reference through the stage at 48 kHz. Then the cache: a second
/// readout simulates nothing, an edit of any layer simulates again.
#[test]
fn series_readouts_match_a_direct_simulation() {
    let rate = 96_000.0;
    let cases = [
        (
            "tilt -> flux",
            vec![
                preset_layer("pp_pentode_feedback"),
                preset_layer("output_transformer"),
            ],
        ),
        (
            "chebyshev -> tape",
            vec![
                preset_layer("se_triode_high_mu"),
                preset_layer("tape_master"),
            ],
        ),
        (
            "tape -> strong curve -> tape",
            vec![
                preset_layer("tape_studio"),
                hot_cheb(),
                preset_layer("tape_vintage"),
            ],
        ),
    ];
    let mut worst: f64 = 0.0;
    for (name, layers) in cases {
        let cfg = series(layers);
        let cache = ReadoutCache::new();
        let r = readout_with(&cfg, rate, true, &cache).unwrap();
        let stage = Harmonics::try_new(&cfg, rate, 1).unwrap();
        assert_eq!(r.peak_gain_db, stage.peak_gain_db());
        let plan = ReadoutPlan::for_costs(stage.readout_cost(rate), 0.0, true);
        let settle_s = plan
            .settle_s
            .max(READOUT_SETTLE_SECONDS)
            .max(stage.readout_settle_s());
        let points: Vec<&HarmonicsReadoutPoint> = r
            .points
            .iter()
            .chain(&r.sweep)
            .chain(&r.freq_sweep)
            .collect();
        // Two tape layers thin the sweeps (the cheapest plan: 28 points).
        assert!(points.len() >= 28, "{name}: {}", points.len());
        let direct: Vec<HarmonicsReadoutPoint> =
            crate::harmonics_sim::par_map(&points, MAX_THREADS, |p| {
                direct_point(&stage, rate, p, settle_s, plan.measure_s)
            });
        for (a, b) in points.iter().zip(&direct) {
            let d = delta(a, b);
            worst = worst.max(d);
            assert!(d < 1e-6, "{name}: {d} dB\n{a:?}\nvs\n{b:?}");
        }
        // The level change at 48 kHz: the reference through the stage.
        let r48 = readout_with(&cfg, STAGE_LOUDNESS_RATE, false, &cache).unwrap();
        let mut whole = Harmonics::try_new(&cfg, STAGE_LOUDNESS_RATE, 1).unwrap();
        let level = stage_loudness_change_db(&mut whole);
        assert!(
            (level - r48.level_change_db).abs() < 1e-9,
            "{name}: {level} vs {}",
            r48.level_change_db
        );
        // Above the simulation rate: the 1 kHz points against the stage run
        // at the real rate (without tape, whose own path moves by up to
        // 0.08 dB on H7/H8 between 96 and 768 kHz, as in parallel). Orders
        // under -100 dBc are left out: the tilted layer feeds the flux
        // layer hot enough that its low orders move by up to 0.06 dB
        // between the two rates.
        let real = 768_000.0;
        let r768 = readout_with(&cfg, real, false, &cache).unwrap();
        let at_real = Harmonics::try_new(&cfg, real, 1).unwrap();
        let tape = at_real.hysteresis_layers() > 0;
        for p in r768.points.iter().filter(|p| p.freq_hz == 1_000.0 && !tape) {
            let q = direct_point(&at_real, real, p, settle_s, plan.measure_s);
            let d = delta_above(p, &q, -100.0);
            assert!(d < 0.06, "{name} at {real}: {d} dB\n{p:?}\nvs\n{q:?}");
        }
        // Cached: nothing simulated again; an edit of any layer simulates.
        let (_, misses) = cache.stats();
        readout_with(&cfg, rate, true, &cache).unwrap();
        assert_eq!(cache.stats().1, misses, "{name}");
        let mut edited = cfg.clone();
        *edited.layers[0].amount_mut().unwrap() = 0.9;
        readout_with(&edited, rate, false, &cache).unwrap();
        assert!(cache.stats().1 > misses, "{name}");
    }
    eprintln!("series readouts vs direct simulation: largest difference {worst:.2e} dB");
}

/// With sag and bias in a series chain: the loaded sweep against the stage
/// run directly with a settle long enough for the envelopes (no presettle),
/// and the level change against the reference through the presettled stage.
#[test]
fn series_readouts_with_sag_and_bias_match_a_long_settle() {
    let rate = 48_000.0;
    let cfg = series(vec![
        preset_layer("pentode"),
        dynamic(
            preset_cheb("se_triode_high_mu"),
            sag(4.0, 5.0, 50.0),
            bias(0.1, 20.0, 200.0),
        ),
        preset_layer("tape_studio"),
    ]);
    let cache = ReadoutCache::new();
    let r = readout_with(&cfg, rate, true, &cache).unwrap();
    assert!(!r.under_load.is_empty());
    let stage = Harmonics::try_new(&cfg, rate, 1).unwrap();
    let mut worst: f64 = 0.0;
    for p in r.under_load.iter().filter(|p| p.freq_hz == 1_000.0) {
        let h = brute_under_load(&cfg, rate, p.input_dbfs, 3.0);
        let q = spectrum_point(p.input_dbfs, p.freq_hz, db_to_lin(p.input_dbfs), &h, rate);
        let d = delta(p, &q);
        worst = worst.max(d);
        assert!(d < 0.02, "{d} dB\n{p:?}\nvs\n{q:?}");
    }
    let mut whole = Harmonics::try_new(&cfg, STAGE_LOUDNESS_RATE, 1).unwrap();
    whole.presettle(&stage_loudness_reference());
    let level = stage_loudness_change_db(&mut whole);
    assert!(
        (level - r.level_change_db).abs() < 1e-9,
        "{level} vs {}",
        r.level_change_db
    );
    // The static points are the curves at rest.
    let rest = Harmonics::try_new(&at_rest(&cfg), rate, 1).unwrap();
    assert!(rest.is_series());
    assert_eq!(r.peak_gain_db, stage.peak_gain_db());
    eprintln!("series under load vs a 3 s settle: largest difference {worst:.2e} dB");
}

#[test]
fn split_channels_run_series_bit_for_bit() {
    let cfg = series(vec![
        dynamic(
            preset_cheb("pp_pentode_feedback"),
            sag(2.0, 1.0, 20.0),
            bias(-0.1, 3.0, 60.0),
        ),
        preset_layer("output_transformer"),
        preset_layer("tape_studio"),
        preset_layer("se_triode_high_mu"),
    ]);
    for (cfg, rate) in [(&cfg, 48_000.0), (&cfg, 192_000.0)] {
        let mut whole = Harmonics::try_new(cfg, rate, 2).unwrap();
        assert!(whole.is_series());
        let (mut l, mut r) = (noise(20_000, 3), noise(20_000, 4));
        whole.process(&mut [&mut l[..], &mut r[..]]);
        let mut parts = whole.clone().split_channels();
        assert_eq!(parts.len(), 2);
        let (mut l, mut r) = (noise(40_000, 5), noise(40_000, 6));
        let (mut pl, mut pr) = (l.clone(), r.clone());
        for ((a, b), (c, d)) in l
            .chunks_mut(997)
            .zip(r.chunks_mut(997))
            .zip(pl.chunks_mut(997).zip(pr.chunks_mut(997)))
        {
            whole.process(&mut [a, b]);
            parts[0].process(&mut [c]);
            parts[1].process(&mut [d]);
        }
        let bits = |v: &[f64]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&l), bits(&pl), "{rate}");
        assert_eq!(bits(&r), bits(&pr), "{rate}");
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

#[test]
fn an_invalid_layer_in_series_names_its_index() {
    let bad = HarmonicsLayer::Flux(FluxLayer {
        enabled: true,
        preset: None,
        corner_hz: 100.0,
        max_boost_db: 12.0,
        target_h3_db: -5.0,
        calibration_hz: 40.0,
    });
    let cfg = series(vec![preset_layer("pentode"), bad]);
    let err = Harmonics::try_new(&cfg, 96_000.0, 1)
        .unwrap_err()
        .to_string();
    assert!(err.contains("layer 1"), "{err}");
    assert_eq!(
        series_indices(&series(vec![
            preset_layer("pentode"),
            off(preset_layer("pentode")),
            silent(preset_layer("tape_master")),
            preset_layer("tape"),
        ])),
        vec![0, 3]
    );
}

/// Cost of a 3-layer series chain against the same layers in parallel,
/// ns per sample (stereo, per channel), at 48 and 768 kHz.
#[test]
#[ignore = "timing: run alone with --ignored --nocapture"]
fn series_cost() {
    use std::time::Instant;
    let chains = [
        (
            "se_triode_high_mu, output_transformer, tape_studio",
            vec![
                preset_layer("se_triode_high_mu"),
                preset_layer("output_transformer"),
                preset_layer("tape_studio"),
            ],
        ),
        (
            "se_triode_high_mu, pp_pentode_feedback, output_transformer",
            vec![
                preset_layer("se_triode_high_mu"),
                preset_layer("pp_pentode_feedback"),
                preset_layer("output_transformer"),
            ],
        ),
    ];
    let n = 1 << 18;
    let src: Vec<f64> = noise(n, 1).iter().map(|x| 0.5 * x).collect();
    for (name, layers) in &chains {
        for rate in [48_000.0, 768_000.0] {
            for cfg in [parallel(layers.clone()), series(layers.clone())] {
                let mut st = Harmonics::try_new(&cfg, rate, 2).unwrap();
                let mut best = f64::INFINITY;
                for _ in 0..5 {
                    let (mut l, mut r) = (src.clone(), src.clone());
                    let t = Instant::now();
                    for (a, b) in l.chunks_mut(512).zip(r.chunks_mut(512)) {
                        st.process(&mut [a, b]);
                    }
                    best = best.min(t.elapsed().as_secs_f64());
                    std::hint::black_box(&l);
                }
                let ns = best * 1e9 / (2 * n) as f64;
                println!(
                    "{name} {:?} @ {rate}: {ns:.1} ns/sample ({:.2} % of a core, stereo)",
                    cfg.routing,
                    ns * 1e-9 * rate * 2.0 * 100.0
                );
            }
        }
    }
}

/// Readout time of a 3-layer series chain with a tape layer: cold, cached,
/// and after an edit of the static layer (the series unit simulates again).
#[test]
#[ignore = "timing: run alone with --ignored --nocapture"]
fn series_readout_time() {
    use std::time::Instant;
    let with_tape = vec![
        preset_layer("se_triode_high_mu"),
        preset_layer("output_transformer"),
        preset_layer("tape_studio"),
    ];
    let static_only = vec![
        preset_layer("se_triode_high_mu"),
        preset_layer("pp_pentode_feedback"),
        preset_layer("output_transformer"),
    ];
    for (layers, rate) in [
        (&with_tape, 96_000.0),
        (&with_tape, 768_000.0),
        (&static_only, 96_000.0),
    ] {
        for cfg in [parallel(layers.clone()), series(layers.clone())] {
            clear_readout_caches();
            let t = Instant::now();
            readout(&cfg, rate, true).unwrap();
            let cold = t.elapsed();
            let t = Instant::now();
            readout(&cfg, rate, true).unwrap();
            let cached = t.elapsed();
            let mut edited = cfg.clone();
            *edited.layers[0].amount_mut().unwrap() = 0.8;
            let t = Instant::now();
            readout(&edited, rate, true).unwrap();
            let edit = t.elapsed();
            let st = Harmonics::try_new(&cfg, rate.min(READOUT_MAX_SIM_RATE), 1).unwrap();
            println!(
                "{} layers, tape {} {:?} @ {rate}: cold {cold:.0?}, cached {cached:.0?}, after \
                 an edit of layer 1 {edit:.0?} (cost {:.2})",
                layers.len(),
                st.hysteresis_layers(),
                cfg.routing,
                st.readout_cost(rate.min(READOUT_MAX_SIM_RATE))
            );
        }
    }
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

/// A Chebyshev layer with `orders` at -40 dB.
fn orders_at_40(orders: &[usize]) -> HarmonicsLayer {
    let mut levels = [None; 8];
    for &k in orders {
        levels[k - 2] = Some(-40.0);
    }
    HarmonicsLayer::Chebyshev(cheb(levels, HARMONICS_DEFAULT_SIGNS))
}

#[test]
fn the_aliasing_is_measured_through_series_cross_terms() {
    let at = |c: &HarmonicsConfig, rate: f64| aliasing(c, rate).unwrap();
    // Degrees 3 and 5: cross terms up to H15; from H9 up they fold back at
    // 192 kHz, at about the product of the layers' levels (H9 is H3 of
    // H3: -80 dB and below), the loudest H9 at 192 kHz − 9 f.
    let layers = vec![orders_at_40(&[2, 3]), orders_at_40(&[2, 5])];
    let s = series(layers.clone());
    let p = parallel(layers);
    let a = at(&s, 192_000.0);
    assert!(a.level_db > -110.0 && a.level_db < -90.0, "{a:?}");
    assert!(
        (a.worst_hz - (192_000.0 - 9.0 * a.tone_hz)).abs() < 1e-6,
        "{a:?}"
    );
    // In parallel nothing folds (H5 of 20 kHz is below 172 kHz).
    assert_eq!(at(&p, 192_000.0).level_db, -200.0);
    assert!(at(&s, 352_800.0).level_db < -150.0);
    // Past H15 the cross terms are measured, not assumed: degrees 3 and 9
    // reach H27, which folds back at 352.8 kHz, but far below the threshold.
    let deep = series(vec![orders_at_40(&[3]), orders_at_40(&[9])]);
    let a = at(&deep, 352_800.0);
    assert!(!a.warn && a.level_db < -150.0, "{a:?}");
    let r = readout(&deep, 352_800.0, false).unwrap();
    assert_eq!(r.aliasing, a);
    // A two-stage chain at 768 kHz: nothing above the floor.
    let chain = series(vec![
        preset_layer("pp_pentode_feedback"),
        preset_layer("se_triode_high_mu"),
    ]);
    let a = at(&chain, 768_000.0);
    assert!(!a.warn && a.level_db < -150.0, "{a:?}");
    // A tape layer's aliasing counts, in series too.
    let tape = series(vec![orders_at_40(&[3]), preset_layer("tape_master")]);
    let a = at(&tape, 192_000.0);
    assert!(a.warn && a.level_db > -70.0, "{a:?}");
    // `tape_vintage` at 96 kHz: -33.8 dBc (H5 of 20 kHz folds there).
    let a = at(&parallel(vec![preset_layer("tape_vintage")]), 96_000.0);
    assert!(a.warn && (a.worst_db + 33.8).abs() < 1.5, "{a:?}");
    assert!(
        (a.worst_hz - (5.0 * a.tone_hz - 96_000.0)).abs() < 1e-6,
        "{a:?}"
    );
}

#[test]
fn series_readouts_report_cross_terms_above_h9() {
    // Measured at 96 kHz: H10 -82.4, H11 -84.2, H12 -85.4 dBc at 0 dBFS,
    // each layer trimmed. Untrimmed, the first layer (bound +0.8 dB) takes
    // a 0 dBFS sine past full scale and the tape layer passes that excess
    // unshaped (module docs, "Series"): the cross terms come up to these
    // (at -2 dBFS they are -85 to -87).
    let cfg = series(vec![
        preset_layer("se_triode_high_mu"),
        preset_layer("tape_master"),
    ]);
    let r = readout(&cfg, 96_000.0, false).unwrap();
    let p = r
        .points
        .iter()
        .find(|p| p.freq_hz == 1_000.0 && p.input_dbfs == 0.0)
        .unwrap();
    for (k, want) in [(10, -62.2), (11, -65.9), (12, -69.1)] {
        let got = p.harmonics_db[k - 2];
        assert!((got - want).abs() < 1.0, "H{k}: {got} vs {want}");
    }
    // THD counts them (every order is below Nyquist here).
    let power: f64 = p.harmonics_db.iter().map(|&db| db_to_lin(2.0 * db)).sum();
    assert!((lin_to_db(power.sqrt()) - p.thd_db).abs() < 1e-9);
    assert!(p.aliased.is_empty());
}

#[test]
fn harmonics_above_nyquist_are_folded_and_left_out_of_thd() {
    // 10 kHz at 44.1 kHz: H3 at 30 kHz folds to 14.1 kHz.
    let cfg = series(vec![orders_at_40(&[2, 3])]);
    let r = readout(&cfg, 44_100.0, true).unwrap();
    let p = r
        .freq_sweep
        .iter()
        .find(|p| p.freq_hz == 10_000.0 && p.input_dbfs == 0.0)
        .unwrap();
    assert_eq!(p.harmonics_db[1], READOUT_FLOOR_DB, "{p:?}");
    let h3 = p.aliased.iter().find(|a| a.order == 3).unwrap();
    assert!((h3.freq_hz - 14_100.0).abs() < 1e-6, "{h3:?}");
    assert!((h3.level_db + 40.0).abs() < 1.0, "{h3:?}");
    assert!((p.thd_db - p.harmonics_db[0]).abs() < 1e-9, "{p:?}");
}
