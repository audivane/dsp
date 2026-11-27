//! The harmonics stage's output and peak gain for a battery of configs,
//! signals and rates, against golden hashes (`fixtures/harmonics-identity.tsv`):
//! every sample's bits, the certified peak gain's bits and the small-signal
//! gain's bits must stay the same.
//!
//! - `HARMONICS_IDENTITY_WRITE=1`: rewrite the fixture from this build.
//! - `HARMONICS_IDENTITY_DUMP=<dir>`: also write every case's raw output
//!   there; `HARMONICS_IDENTITY_CMP=<dir>`: compare with such a dump sample
//!   by sample (reports the first difference and the largest).

use audivane_dsp_core::Stage;
use audivane_dsp_core::harmonics::Harmonics;
use audivane_protocol::{
    ChebyshevLayer, HARMONICS_DEFAULT_SIGNS, HarmonicSign, HarmonicsConfig, HarmonicsLayer,
    HarmonicsTilt, OrderTilt, TiltShelf, harmonics_presets,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

const RATES: [f64; 3] = [44_100.0, 96_000.0, 768_000.0];
const LEN: usize = 12_000;
const CHANNELS: usize = 3;

fn layer(id: &str) -> HarmonicsLayer {
    HarmonicsConfig::from_preset(id).unwrap().layers[0].clone()
}

fn stage_of(layers: Vec<HarmonicsLayer>) -> HarmonicsConfig {
    HarmonicsConfig {
        system: None,
        enabled: true,
        layers,
        dc_block: true,
        routing: Default::default(),
    }
}

fn cheb(levels: [Option<f64>; 8], signs: [HarmonicSign; 8]) -> ChebyshevLayer {
    ChebyshevLayer {
        enabled: true,
        preset: None,
        levels_db: levels,
        signs,
        invert_even: false,
        drive_db: 0.0,
        amount: 1.0,
        tilt: None,
        sag: None,
        bias: None,
    }
}

fn shelf(hz: f64, db: f64) -> TiltShelf {
    TiltShelf::new(hz, db)
}

fn with_cheb(mut l: HarmonicsLayer, f: impl FnOnce(&mut ChebyshevLayer)) -> HarmonicsLayer {
    if let HarmonicsLayer::Chebyshev(c) = &mut l {
        f(c);
    }
    l
}

fn configs() -> Vec<(String, HarmonicsConfig)> {
    use HarmonicSign::{Negative as N, Positive as P};
    let mut out: Vec<(String, HarmonicsConfig)> = harmonics_presets()
        .iter()
        .map(|p| (p.id.clone(), HarmonicsConfig::from_preset(&p.id).unwrap()))
        .collect();
    let every = [-40.0, -58.0, -70.0, -75.0, -80.0, -85.0, -90.0, -95.0].map(Some);
    let loud = [-30.0, -35.0, -40.0, -45.0, -24.0, -22.0, -26.0, -20.0].map(Some);
    let tilt_all = HarmonicsTilt {
        low: shelf(60.0, 12.0),
        high: shelf(6_000.0, 6.0),
        low_even: true,
        orders: Default::default(),
    };
    let mut overrides = tilt_all.clone();
    let mut orders: [Option<OrderTilt>; 8] = Default::default();
    orders[1] = Some(OrderTilt {
        low: shelf(120.0, -6.0),
        high: shelf(3_000.0, 9.0),
    });
    orders[4] = Some(OrderTilt {
        low: shelf(60.0, 0.0),
        high: shelf(6_000.0, 0.0),
    });
    overrides.orders = orders;
    let custom = |levels, signs| HarmonicsLayer::Chebyshev(cheb(levels, signs));
    let tilted = |levels, signs, t: &HarmonicsTilt| {
        let mut c = cheb(levels, signs);
        c.tilt = Some(Box::new(t.clone()));
        HarmonicsLayer::Chebyshev(c)
    };
    let mut mixes: Vec<(&str, Vec<HarmonicsLayer>)> = vec![
        ("no layers", vec![]),
        (
            "H2-H9 mixed signs, trimmed",
            vec![custom(loud, [P, N, N, P, N, P, P, N])],
        ),
        (
            "H2-H9 tilted, low all",
            vec![tilted(every, HARMONICS_DEFAULT_SIGNS, &tilt_all)],
        ),
        (
            "H2-H9 tilted, overrides",
            vec![tilted(loud, [P, N, N, P, N, P, P, N], &overrides)],
        ),
        (
            "cheb + flux",
            vec![layer("se_triode_high_mu"), layer("output_transformer")],
        ),
        (
            "cheb + hyst",
            vec![layer("se_triode_high_mu"), layer("tape_studio")],
        ),
        ("two flux", vec![layer("output_transformer"), layer("tape")]),
        (
            "three hyst",
            vec![
                layer("tape_studio"),
                layer("tape_master"),
                layer("tape_vintage"),
            ],
        ),
        (
            "tilt + hyst",
            vec![layer("pp_pentode_feedback"), layer("tape_studio")],
        ),
        (
            "tilt + two hyst",
            vec![
                layer("tape_master"),
                layer("solid_state_feedback"),
                layer("tape_vintage"),
            ],
        ),
        (
            "tilt + flux + hyst",
            vec![
                layer("solid_state_feedback"),
                layer("pp_pentode_feedback"),
                layer("output_transformer"),
                layer("tape"),
                layer("tape_studio"),
                layer("tape_master"),
            ],
        ),
        (
            "shared tilt filters",
            vec![
                layer("solid_state_feedback"),
                layer("solid_state_feedback"),
                layer("triode"),
            ],
        ),
        (
            "hyst, flux, cheb",
            vec![layer("tape_vintage"), layer("tape"), layer("triode")],
        ),
        (
            "off and zero layers",
            vec![
                layer("pentode"),
                {
                    let mut l = layer("tape_studio");
                    if let HarmonicsLayer::Hysteresis(h) = &mut l {
                        h.amount = 0.0;
                    }
                    l
                },
                with_cheb(layer("se_pentode"), |c| c.enabled = false),
                with_cheb(layer("diff_pair"), |c| c.amount = 0.0),
                {
                    let mut l = layer("tape");
                    if let HarmonicsLayer::Flux(f) = &mut l {
                        f.enabled = false;
                    }
                    l
                },
            ],
        ),
        (
            "drive, amount, invert_even",
            vec![
                with_cheb(layer("se_triode_low_mu"), |c| {
                    c.drive_db = -6.0;
                    c.amount = 0.5;
                    c.invert_even = true;
                }),
                with_cheb(layer("pp_triode_imbalanced"), |c| c.drive_db = -12.0),
            ],
        ),
        (
            "eight layers",
            vec![
                layer("se_triode_high_mu"),
                layer("solid_state_feedback"),
                layer("pp_pentode_feedback"),
                layer("output_transformer"),
                layer("tape"),
                layer("tape_master"),
                layer("tape_studio"),
                layer("tape_vintage"),
            ],
        ),
        (
            "hyst at half amount + flux",
            vec![
                {
                    let mut l = layer("tape_master");
                    if let HarmonicsLayer::Hysteresis(h) = &mut l {
                        h.amount = 0.5;
                    }
                    l
                },
                layer("output_transformer"),
            ],
        ),
    ];
    for (name, layers) in mixes.drain(..) {
        out.push((name.to_string(), stage_of(layers)));
    }
    out.push((
        "stage off".into(),
        HarmonicsConfig {
            enabled: false,
            ..HarmonicsConfig::from_preset("pentode").unwrap()
        },
    ));
    out
}

/// Three channels: noise with overs and non-finite samples, a sweep of
/// tones at rising level, and silence with a burst.
fn signals(rate: f64) -> Vec<Vec<f64>> {
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut noise = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    };
    let mut lp = 0.0;
    let mut a: Vec<f64> = (0..LEN)
        .map(|_| {
            lp = 0.9 * lp + 0.25 * noise();
            1.2 * lp
        })
        .collect();
    a[100] = f64::NAN;
    a[101] = f64::INFINITY;
    a[2_000] = 1.7;
    a[2_001] = -1.7;
    let mut phase = 0.0;
    let b: Vec<f64> = (0..LEN)
        .map(|i| {
            let t = i as f64 / LEN as f64;
            phase += 2.0 * std::f64::consts::PI * (30.0 + 18_000.0 * t * t) / rate;
            (0.05 + 0.97 * t) * phase.sin()
        })
        .collect();
    let c: Vec<f64> = (0..LEN)
        .map(|i| {
            if (3_000..3_400).contains(&i) {
                0.9 * (i as f64 * 0.07).sin()
            } else {
                0.0
            }
        })
        .collect();
    vec![a, b, c]
}

/// Block sizes cycling through odd lengths.
fn blocks(len: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let (mut i, mut b) = (0, 1);
    while i < len {
        let e = (i + b).min(len);
        out.push((i, e));
        i = e;
        b = b % 997 + 37;
    }
    out
}

/// The output of `cfg` at `rate`: all channels in one stage (`split` false)
/// or the first third in one stage and the rest on split channels.
fn run(cfg: &HarmonicsConfig, rate: f64, split: bool) -> (Vec<Vec<f64>>, f64, f64) {
    let mut st = Harmonics::try_new(cfg, rate, CHANNELS).unwrap();
    let trim = st.peak_gain_db();
    let small = st.small_signal_gain_db();
    let mut bufs = signals(rate);
    let cut = if split { LEN / 3 } else { LEN };
    for (s, e) in blocks(cut) {
        let mut chans: Vec<&mut [f64]> = bufs.iter_mut().map(|b| &mut b[s..e]).collect();
        st.process(&mut chans);
    }
    if split {
        let mut parts = st.split_channels();
        for (part, buf) in parts.iter_mut().zip(bufs.iter_mut()) {
            for (s, e) in blocks(LEN - cut) {
                part.process(&mut [&mut buf[cut + s..cut + e]]);
            }
        }
    }
    (bufs, trim, small)
}

fn fnv(bufs: &[Vec<f64>]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bufs {
        for x in b {
            for byte in x.to_bits().to_le_bytes() {
                h ^= u64::from(byte);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    h
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/harmonics-identity.tsv")
}

#[test]
fn harmonics_output_and_peak_gains_are_unchanged() {
    let write = std::env::var_os("HARMONICS_IDENTITY_WRITE").is_some();
    let dump = std::env::var_os("HARMONICS_IDENTITY_DUMP").map(PathBuf::from);
    let cmp = std::env::var_os("HARMONICS_IDENTITY_CMP").map(PathBuf::from);
    let golden: BTreeMap<String, (u64, u64, u64)> = if write {
        BTreeMap::new()
    } else {
        std::fs::read_to_string(fixture_path())
            .expect("fixture")
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                let hex = |s: &str| u64::from_str_radix(s, 16).unwrap();
                (f[0].to_string(), (hex(f[1]), hex(f[2]), hex(f[3])))
            })
            .collect()
    };
    let mut table = String::from("# case\toutput hash\tpeak gain bits\tsmall-signal bits\n");
    let mut failures = Vec::new();
    let mut cases = 0;
    let mut worst_trim = 0.0_f64;
    for (name, cfg) in configs() {
        for dc_block in [true, false] {
            let cfg = HarmonicsConfig {
                dc_block,
                ..cfg.clone()
            };
            for rate in RATES {
                for split in [false, true] {
                    let key = format!(
                        "{name} | dc {} | {rate} | {}",
                        if dc_block { "on" } else { "off" },
                        if split { "split" } else { "joint" }
                    );
                    let (bufs, trim, small) = run(&cfg, rate, split);
                    let hash = fnv(&bufs);
                    cases += 1;
                    writeln!(
                        table,
                        "{key}\t{hash:016x}\t{:016x}\t{:016x}",
                        trim.to_bits(),
                        small.to_bits()
                    )
                    .unwrap();
                    let file = |dir: &PathBuf| {
                        dir.join(key.replace([' ', '|', '/', ','], "_").replace("__", "_"))
                    };
                    if let Some(dir) = &dump {
                        std::fs::create_dir_all(dir).unwrap();
                        let bytes: Vec<u8> = bufs
                            .iter()
                            .flatten()
                            .flat_map(|x| x.to_le_bytes())
                            .collect();
                        std::fs::write(file(dir), bytes).unwrap();
                    }
                    if let Some(dir) = &cmp {
                        let bytes = std::fs::read(file(dir)).unwrap();
                        let old: Vec<f64> = bytes
                            .as_chunks::<8>()
                            .0
                            .iter()
                            .map(|c| f64::from_le_bytes(*c))
                            .collect();
                        let new: Vec<f64> = bufs.iter().flatten().copied().collect();
                        let first = old
                            .iter()
                            .zip(&new)
                            .position(|(a, b)| a.to_bits() != b.to_bits());
                        let max = old
                            .iter()
                            .zip(&new)
                            .map(|(a, b)| (a - b).abs())
                            .fold(0.0, f64::max);
                        if let Some(i) = first {
                            eprintln!(
                                "{key}: first difference at sample {} of channel {} ({:e} vs {:e}), max {max:e}",
                                i % LEN,
                                i / LEN,
                                old[i],
                                new[i]
                            );
                        }
                    }
                    if !write {
                        match golden.get(&key) {
                            None => failures.push(format!("{key}: not in the fixture")),
                            Some(&(h, t, s)) => {
                                let dt = (f64::from_bits(t) - trim).abs();
                                worst_trim = worst_trim.max(dt);
                                if dt > 1e-12 {
                                    failures.push(format!(
                                        "{key}: peak gain {trim} vs {} ({dt:e})",
                                        f64::from_bits(t)
                                    ));
                                }
                                if h != hash || t != trim.to_bits() || s != small.to_bits() {
                                    failures.push(format!(
                                        "{key}: output hash {hash:016x} vs {h:016x}, peak gain bits {}, small-signal bits {}",
                                        if t == trim.to_bits() { "same" } else { "differ" },
                                        if s == small.to_bits() { "same" } else { "differ" },
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if write {
        std::fs::write(fixture_path(), table).unwrap();
        return;
    }
    eprintln!("{cases} cases, largest peak gain difference {worst_trim:e}");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The readouts (with sweeps) of every config at 96 and 768 kHz, written to
/// `HARMONICS_IDENTITY_DUMP` or compared with `HARMONICS_IDENTITY_CMP` (a
/// check across a refactor, not a fixture: slow with hysteresis layers).
#[test]
#[ignore]
fn readouts_match_a_dump() {
    let dump = std::env::var_os("HARMONICS_IDENTITY_DUMP").map(PathBuf::from);
    let cmp = std::env::var_os("HARMONICS_IDENTITY_CMP").map(PathBuf::from);
    let mut differ = Vec::new();
    for (name, cfg) in configs() {
        for rate in [96_000.0, 768_000.0] {
            let t = std::time::Instant::now();
            let r = audivane_dsp_core::harmonics::readout(&cfg, rate, true)
                .map(|r| format!("{r:?}"))
                .unwrap_or_else(|e| format!("error {e}"));
            let ms = t.elapsed().as_secs_f64() * 1e3;
            let file = format!("readout_{}_{rate}", name.replace([' ', ',', '+'], "_"));
            if let Some(dir) = &dump {
                std::fs::create_dir_all(dir).unwrap();
                std::fs::write(dir.join(&file), &r).unwrap();
            }
            let same = cmp
                .as_ref()
                .map(|dir| std::fs::read_to_string(dir.join(&file)).unwrap() == r);
            eprintln!(
                "{name} at {rate}: {ms:.0} ms{}",
                match same {
                    Some(true) => ", same",
                    Some(false) => ", DIFFERENT",
                    None => "",
                }
            );
            if same == Some(false) {
                differ.push(format!("{name} at {rate}"));
            }
        }
    }
    eprintln!("different: {differ:?}");
}
