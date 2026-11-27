//! Tests of the varispeed stage: constant-speed quality against the stage's
//! own transfer function, the rational path at the same instants, the
//! resampler's error during speed changes, the intended modulation of the
//! planner's trajectories and the latency; plus the pull contract seen from
//! the stage, the envelope's rules and the speed limit. The no-allocation
//! test is in `tests/no_alloc.rs`.

// Output frame m is indexed alongside the reference at m.
#![allow(clippy::needless_range_loop)]

use super::*;
use crate::adaptive::{CoreTable, position_after};
use std::f64::consts::PI;

const ONE_F: f64 = 18_446_744_073_709_551_616.0; // 2^64
const G: u32 = 44_100;
const SPEEDS: [f64; 6] = [0.5, 0.93, 1.0, 1.0123, 1.08, 2.0];

fn db(x: f64) -> f64 {
    20.0 * x.abs().max(1e-300).log10()
}

fn lcg(seed: &mut u64) -> f64 {
    *seed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*seed >> 11) as f64) / ((1u64 << 53) as f64) * 2.0 - 1.0
}

fn unit(seed: &mut u64) -> f64 {
    0.5 * (lcg(seed) + 1.0)
}

/// Zero-phase response of a symmetric kernel at `theta` rad per grid
/// sample.
fn zero_phase(h: &[f64], theta: f64) -> f64 {
    let c = (h.len() - 1) as f64 / 2.0;
    h.iter()
        .enumerate()
        .map(|(n, v)| v * (theta * (n as f64 - c)).cos())
        .sum()
}

fn stage(rate: u32, channels: usize, speed: f64) -> Varispeed {
    Varispeed::new(VarispeedSpec {
        sample_rate: rate,
        channels,
        max_frames: MAX_FRAMES,
        speed,
    })
    .unwrap()
}

/// A pull loop with the same analytic source on every channel and,
/// optionally, the position of every core output logged.
struct Run {
    vs: Varispeed,
    src: Vec<Vec<f64>>,
    out: Vec<Vec<f64>>,
    /// Source frames delivered.
    fed: u64,
    /// Output frames produced.
    produced: usize,
    /// Positions of the core outputs, ×2-stream frames (absolute).
    log: Option<Vec<f64>>,
    /// Core outputs of the last call.
    last_core: usize,
    /// The largest step of the last call.
    last_peak: f64,
}

impl Run {
    fn new(vs: Varispeed, log: bool) -> Self {
        let ch = vs.channels();
        let n = vs.max_frames();
        Self {
            src: vec![vec![0.0; 2 * n + 2]; ch],
            out: vec![vec![0.0; n]; ch],
            vs,
            fed: 0,
            produced: 0,
            log: log.then(Vec::new),
            last_core: 0,
            last_peak: 0.0,
        }
    }

    /// One call of exactly `n` frames; `x(j)` is the source at absolute
    /// source frame `j`. The output is `self.out[c][..n]`.
    fn pull(&mut self, n: usize, x: &mut dyn FnMut(u64) -> f64) -> usize {
        let bound = self.vs.max_source_frames(n);
        let n_src = self.vs.plan(n).unwrap();
        assert!(
            n_src <= bound,
            "n = {n}: {n_src} source frames, bound {bound}"
        );
        let positions = planned_positions(&self.vs);
        self.last_core = positions.len();
        self.last_peak = planned_steps(&self.vs).into_iter().fold(0.0, f64::max);
        if let Some(log) = &mut self.log {
            log.extend(
                positions
                    .iter()
                    .map(|&q| (q >> 64) as f64 + (q as u64) as f64 / ONE_F),
            );
        }
        for j in 0..n_src {
            let v = x(self.fed + j as u64);
            for c in &mut self.src {
                c[j] = v;
            }
        }
        let ins: Vec<&[f64]> = self.src.iter().map(|c| &c[..n_src]).collect();
        let mut outs: Vec<&mut [f64]> = self.out.iter_mut().map(|c| &mut c[..n]).collect();
        self.vs.process(&ins, &mut outs);
        self.fed += n_src as u64;
        self.produced += n;
        n_src
    }
}

/// The exact 64.64 positions of every core output of the latched call,
/// and the position after it, replayed on a copy of the accumulator.
fn planned_trajectory(vs: &Varispeed) -> (Vec<u128>, u128) {
    let latch = vs.latched.expect("a plan");
    let mut acc = vs.core().accumulator().clone();
    let mut out = Vec::new();
    for part in &latch.parts[..latch.n_parts] {
        assert_eq!(acc.plan(part.outputs, part.seg), Ok(part.need));
        let p = acc.planned().unwrap();
        let pos = acc.position();
        out.extend((0..part.outputs).map(|k| position_after(pos, p.step, p.d, k).unwrap()));
        acc.commit(part.need);
    }
    assert_eq!(out.len(), latch.n_core);
    (out, acc.position())
}

fn planned_positions(vs: &Varispeed) -> Vec<u128> {
    planned_trajectory(vs).0
}

/// The step at every core output of the latched call (to the next output).
fn planned_steps(vs: &Varispeed) -> Vec<f64> {
    let (mut q, next) = planned_trajectory(vs);
    q.push(next);
    q.windows(2).map(|w| (w[1] - w[0]) as f64 / ONE_F).collect()
}

/// The stage's own transfer function and timing at constant speed.
struct Model {
    up: Vec<f64>,
    down: Vec<f64>,
    table: std::sync::Arc<CoreTable>,
    /// Delays on the 2G grid: ×2, core, ÷2.
    du: f64,
    dc: f64,
    dd: f64,
    rate: f64,
}

impl Model {
    fn of(vs: &Varispeed) -> Self {
        let d = vs.design();
        Self {
            up: d.up.clone(),
            down: d.down.clone(),
            table: vs.core().table().clone(),
            du: d.up_delay(),
            dc: vs.core().delay(),
            dd: d.down_delay(),
            rate: f64::from(vs.sample_rate()),
        }
    }

    /// The core's gain at source frequency `f` Hz (independent of speed).
    fn core_gain(&self, f: f64) -> f64 {
        self.table.response(f / (2.0 * self.rate))
    }

    /// The product of the three transfer functions at source frequency
    /// `f`, speed `s`, given the core's gain there.
    fn gain(&self, f: f64, s: f64, core: f64) -> f64 {
        let w2 = 2.0 * PI * f / (2.0 * self.rate);
        zero_phase(&self.up, w2) / 2.0 * core * zero_phase(&self.down, w2 * s)
    }

    /// The source time (frames) output frame `m` stands for at constant
    /// speed `s` from a reset.
    fn source_time(&self, m: f64, s: f64) -> f64 {
        ((2.0 * m - self.dd) * s - self.dc - self.du) / 2.0
    }
}

#[test]
fn bad_specs_and_commands_are_refused() {
    let spec = VarispeedSpec {
        sample_rate: 48_000,
        channels: 2,
        max_frames: 1024,
        speed: 1.0,
    };
    let bad = |s: VarispeedSpec| Varispeed::new(s).err().unwrap();
    assert_eq!(
        bad(VarispeedSpec {
            sample_rate: 0,
            ..spec
        }),
        VarispeedError::Rate(0)
    );
    assert_eq!(
        bad(VarispeedSpec {
            channels: 0,
            ..spec
        }),
        VarispeedError::Channels(0)
    );
    assert_eq!(
        bad(VarispeedSpec {
            channels: 129,
            ..spec
        }),
        VarispeedError::Channels(129)
    );
    assert_eq!(
        bad(VarispeedSpec {
            max_frames: 0,
            ..spec
        }),
        VarispeedError::MaxFrames(0)
    );
    assert_eq!(
        bad(VarispeedSpec {
            max_frames: MAX_FRAMES + 1,
            ..spec
        }),
        VarispeedError::MaxFrames(MAX_FRAMES + 1)
    );
    assert_eq!(
        bad(VarispeedSpec { speed: 0.4, ..spec }),
        VarispeedError::Speed(0.4)
    );
    let mut vs = Varispeed::new(spec).unwrap();
    for s in [0.0, 0.49, 2.01, f64::NAN, f64::INFINITY, -1.0] {
        assert!(
            matches!(vs.set_speed(s, 0.0), Err(VarispeedError::Speed(_))),
            "{s}"
        );
    }
    for r in [-1.0, 10_000.5, f64::NAN, f64::INFINITY] {
        assert!(
            matches!(vs.set_speed(1.5, r), Err(VarispeedError::Ramp(_))),
            "{r}"
        );
    }
    assert!(!vs.ramping(), "a refused command changes nothing");
    assert_eq!(vs.target(), 1.0);
    assert_eq!(
        vs.plan(1025),
        Err(VarispeedError::TooManyFrames { n: 1025, max: 1024 })
    );
    assert!(vs.core().planned().is_none());
    vs.set_speed(2.0, 10_000.0).unwrap();
    vs.set_speed(0.5, 0.0).unwrap();
    assert_eq!(vs.target(), 0.5);
}

/// Tones at -6 and -1 dBFS, a multitone and a -60 dBFS tone
/// at constant speeds, against the product of the three transfer
/// functions with the exact delays: residual <= -150 dBFS, and the
/// predicted gain flat to 1e-4 dB inside `0.907 G/2 min(1, s)`.
#[test]
fn constant_speed_matches_the_stages_own_transfer_function() {
    let model = Model::of(&stage(G, 1, 1.0));
    let freqs = [997.0, 15_003.0, 19_997.0, 21_500.0];
    let multi = [101.0, 997.0, 4_999.0, 10_007.0, 17_011.0];
    let gc: Vec<f64> = freqs.iter().map(|&f| model.core_gain(f)).collect();
    let gm: Vec<f64> = multi.iter().map(|&f| model.core_gain(f)).collect();
    let edge = STEEP_PASSBAND * f64::from(G) / 2.0;
    let total = 60_000usize;
    let ph = 0.3;
    let mut worst_pass = f64::NEG_INFINITY;
    for &s in &SPEEDS {
        // Flatness inside the output passband, source-referred.
        // Source-referred: f <= edge and f s <= edge.
        let top = edge / s.max(1.0);
        let mut flat = 0.0f64;
        for i in 0..=40 {
            let f = 20.0 + (top - 20.0) * i as f64 / 40.0;
            flat = flat.max(db(model.gain(f, s, model.core_gain(f))).abs());
        }
        assert!(flat <= 1e-4, "s = {s}: passband deviation {flat} dB");
        let mut row = format!("s = {s:<6} flat {flat:.1e} dB |");
        let case = |tones: &[(f64, f64, f64)]| -> f64 {
            // (frequency, amplitude, core gain)
            let mut run = Run::new(stage(G, 1, s), false);
            let mut x = |j: u64| {
                tones
                    .iter()
                    .map(|&(f, a, _)| a * (2.0 * PI * f / f64::from(G) * j as f64 + ph).sin())
                    .sum()
            };
            let gains: Vec<f64> = tones
                .iter()
                .map(|&(f, a, g)| a * model.gain(f, s, g))
                .collect();
            let mut worst = 0.0f64;
            while run.produced < total {
                let m0 = run.produced;
                run.pull(1024, &mut x);
                for i in 0..1024 {
                    let m = (m0 + i) as f64;
                    if m < 4096.0 {
                        continue;
                    }
                    let t = model.source_time(m, s);
                    let want: f64 = tones
                        .iter()
                        .zip(&gains)
                        .map(|(&(f, _, _), &g)| g * (2.0 * PI * f / f64::from(G) * t + ph).sin())
                        .sum();
                    worst = worst.max((run.out[0][i] - want).abs());
                }
            }
            db(worst)
        };
        for (i, &f) in freqs.iter().enumerate() {
            for amp_db in [-6.0, -1.0] {
                let a = 10f64.powf(amp_db / 20.0);
                let r = case(&[(f, a, gc[i])]);
                let passband = f <= edge && f * s <= edge;
                if passband {
                    worst_pass = worst_pass.max(r);
                }
                assert!(r <= -150.0, "s = {s}, {f} Hz at {amp_db} dBFS: {r:.1} dBFS");
                if amp_db == -6.0 {
                    row += &format!(" {f:.0} Hz {:+.5} dB {r:.1}", db(model.gain(f, s, gc[i])));
                } else {
                    row += &format!(" (-1 dBFS {r:.1})");
                }
            }
        }
        let a = 10f64.powf(-20.0 / 20.0);
        let tones: Vec<(f64, f64, f64)> = multi.iter().zip(&gm).map(|(&f, &g)| (f, a, g)).collect();
        let r = case(&tones);
        assert!(r <= -150.0, "s = {s}, multitone: {r:.1} dBFS");
        row += &format!(" | multitone {r:.1}");
        let r = case(&[(997.0, 1e-3, gc[0])]);
        assert!(r <= -150.0, "s = {s}, -60 dBFS: {r:.1} dBFS");
        row += &format!(" | -60 dBFS {r:.1}");
        println!("{row}");
    }
    println!("worst passband residual: {worst_pass:.1} dBFS");
}

/// Constant speed: the output does not depend on how the calls are cut,
/// bit for bit, and channel 0 of a stereo stage equals a mono stage.
#[test]
fn constant_speed_is_independent_of_the_block_sizes() {
    for s in [0.5, 1.0123, 2.0] {
        let mut seed = 9;
        let noise: Vec<f64> = (0..200_000).map(|_| lcg(&mut seed)).collect();
        let mut x = |j: u64| noise[j as usize % noise.len()];
        let collect = |sizes: &[usize], ch: usize, x: &mut dyn FnMut(u64) -> f64| {
            let mut run = Run::new(stage(G, ch, s), false);
            let mut out = Vec::new();
            let mut i = 0;
            while out.len() < 40_000 {
                let n = sizes[i % sizes.len()];
                i += 1;
                run.pull(n, x);
                out.extend_from_slice(&run.out[0][..n]);
            }
            out.truncate(40_000);
            out
        };
        let a = collect(&[1024], 1, &mut x);
        let b = collect(&[1, 0, 8192, 37, 4095, 2, 513], 1, &mut x);
        let c = collect(&[700, 1], 2, &mut x);
        assert!(a == b, "s = {s}: block sizes changed the output");
        assert!(a == c, "s = {s}: stereo channel 0 differs from mono");
    }
}

/// The trajectory of one speed-change case: what happens at which output
/// frame.
#[derive(Clone, Copy, Debug)]
enum Change {
    /// An instant change of the step (the test hook; commands always ramp).
    Step(f64),
    /// `set_speed(speed, ramp_ms)`.
    Speed(f64, f64),
    /// `set_speed_limit(max)`.
    Limit(f64),
}

struct ChangeCase {
    name: &'static str,
    s0: f64,
    /// (output frame, change)
    events: &'static [(usize, Change)],
    /// Frames to run.
    total: usize,
}

const START: usize = 8192;

const CHANGE_CASES: &[ChangeCase] = &[
    ChangeCase {
        name: "step 0.5 -> 2",
        s0: 0.5,
        events: &[(START, Change::Step(2.0))],
        total: 30_000,
    },
    ChangeCase {
        name: "step 2 -> 0.5",
        s0: 2.0,
        events: &[(START, Change::Step(0.5))],
        total: 30_000,
    },
    ChangeCase {
        name: "cosine 0.5 -> 2, T_min (200 ms)",
        s0: 0.5,
        events: &[(START, Change::Speed(2.0, 0.0))],
        total: START + 14_000,
    },
    ChangeCase {
        name: "cosine 2 -> 0.5, T_min (200 ms)",
        s0: 2.0,
        events: &[(START, Change::Speed(0.5, 0.0))],
        total: START + 14_000,
    },
    ChangeCase {
        name: "cosine 1 -> 1.08, 50 ms",
        s0: 1.0,
        events: &[(START, Change::Speed(1.08, 50.0))],
        total: START + 6_000,
    },
    ChangeCase {
        name: "cosine 0.5 -> 2, 400 ms, retarget to 1 halfway",
        s0: 0.5,
        events: &[
            (START, Change::Speed(2.0, 400.0)),
            (START + 8_820, Change::Speed(1.0, 400.0)),
        ],
        total: START + 32_000,
    },
    ChangeCase {
        name: "cosine 1 -> 1.000013, 20 ms",
        s0: 1.0,
        events: &[(START, Change::Speed(1.000_013, 20.0))],
        total: START + 4_000,
    },
    ChangeCase {
        name: "1 -> 1.5, 200 ms, retarget to 2 at a quarter",
        s0: 1.0,
        events: &[
            (START, Change::Speed(1.5, 200.0)),
            (START + 2_205, Change::Speed(2.0, 200.0)),
        ],
        total: START + 16_000,
    },
    ChangeCase {
        name: "0.5 -> 2 at T_min, reversed to 0.5 halfway",
        s0: 0.5,
        events: &[
            (START, Change::Speed(2.0, 0.0)),
            (START + 4_410, Change::Speed(0.5, 0.0)),
        ],
        total: START + 24_000,
    },
    ChangeCase {
        // Applied up to 8 192 frames late (the call sizes of the
        // speed-change case), the brake from there still stops below 1.9: no
        // fade.
        name: "1 -> 2, 400 ms, capped at 1.9 early in the ramp",
        s0: 1.0,
        events: &[
            (START, Change::Speed(2.0, 400.0)),
            (START + 2_000, Change::Limit(1.9)),
        ],
        total: START + 30_000,
    },
];

fn apply(vs: &mut Varispeed, c: Change) {
    match c {
        Change::Step(s) => vs.jump(s),
        Change::Speed(s, r) => vs.set_speed(s, r).unwrap(),
        Change::Limit(l) => vs.set_speed_limit(l),
    }
}

/// At constant speeds, at every common rate, the delay
/// measured from a tone's phase equals `(d_up + d_core) / (2 s) + d_down /
/// 2` (136.75/s + 130.75 up to 192 kHz) within 0.01 frame; during ramps,
/// the status estimate and the delay mapped through the logged trajectory
/// both lie within the bound the trajectory gives.
#[test]
fn latency_matches_the_formula_and_the_trajectory_bound() {
    let mut worst = 0.0f64;
    for rate in [44_100u32, 48_000, 96_000, 192_000, 384_000, 768_000] {
        for &s in &SPEEDS {
            let mut run = Run::new(stage(rate, 1, s), false);
            let w_in = 2.0 * PI * 49.9 / 44_100.0; // rad per source frame
            let ph = 0.7;
            let mut x = |j: u64| 0.5 * (w_in * j as f64 + ph).sin();
            let w = w_in * s; // rad per output frame
            let (mut ss, mut sc, mut cc, mut ys, mut yc) = (0.0, 0.0, 0.0, 0.0, 0.0);
            while run.produced < 50_000 {
                let m0 = run.produced;
                run.pull(1000, &mut x);
                for i in 0..1000 {
                    let m = (m0 + i) as f64;
                    if m < 4000.0 {
                        continue;
                    }
                    let (sn, cs) = (w * m).sin_cos();
                    let y = run.out[0][i];
                    ss += sn * sn;
                    sc += sn * cs;
                    cc += cs * cs;
                    ys += y * sn;
                    yc += y * cs;
                }
            }
            // y = a sin(wm) + b cos(wm) = A sin(wm + θ), θ = ph - w D.
            let det = ss * cc - sc * sc;
            let a = (ys * cc - yc * sc) / det;
            let b = (yc * ss - ys * sc) / det;
            let theta = b.atan2(a);
            let lag = (ph - theta).rem_euclid(2.0 * PI);
            let measured = lag / w;
            let want = run.vs.latency_at(s);
            worst = worst.max((measured - want).abs());
            assert!(
                (measured - want).abs() < 0.01,
                "{rate} Hz, s = {s}: measured {measured}, formula {want}"
            );
        }
    }
    println!("constant speed: measured delay within {worst:.2e} frames of the formula");

    // During ramps.
    for case in CHANGE_CASES
        .iter()
        .filter(|c| !c.events.iter().any(|(_, e)| matches!(e, Change::Step(_))))
    {
        let mut run = Run::new(stage(G, 1, case.s0), true);
        let model = Model::of(&run.vs);
        let mut zero = |_| 0.0;
        let mut ev = 0;
        let (mut max_diff, mut settling, mut exact_calls) = (0.0f64, 0.0f64, 0usize);
        while run.produced < case.total {
            while ev < case.events.len() && run.produced >= case.events[ev].0 {
                apply(&mut run.vs, case.events[ev].1);
                ev += 1;
            }
            run.pull(512, &mut zero);
            let log = run.log.as_ref().unwrap();
            let m = run.produced as f64 - 1.0; // the call's last output
            if m < 1000.0 {
                continue;
            }
            // The true delay: output m plays source time t, consumed when
            // the position passed 2t (the core's output index q).
            let interp = |x: f64| {
                let i = x.floor() as usize;
                log[i] + (log[i + 1] - log[i]) * (x - i as f64)
            };
            let goal = interp(2.0 * m - model.dd) - model.dc - model.du;
            let mut q = log.partition_point(|&p| p < goal);
            q = q.saturating_sub(1);
            let qf = q as f64 + (goal - log[q]) / (log[q + 1] - log[q]);
            let truth = m - qf / 2.0;
            // The speeds over the window from q to the next output.
            let (lo, hi) = log[q..]
                .windows(2)
                .map(|w| w[1] - w[0])
                .chain([run.vs.speed()])
                .fold((f64::INFINITY, 0.0f64), |(a, b), v| (a.min(v), b.max(v)));
            let (d_lo, d_hi) = (run.vs.latency_at(hi), run.vs.latency_at(lo));
            let est = run.vs.latency_frames();
            let tol = 1e-6;
            assert!(
                truth >= d_lo - tol && truth <= d_hi + tol,
                "{}: truth {truth} outside {d_lo}..{d_hi}",
                case.name
            );
            assert!(est >= d_lo - tol && est <= d_hi + tol);
            if run.vs.latency_exact() {
                // Exact only once the step has been constant over the
                // whole span the delay depends on.
                assert!(
                    (truth - est).abs() < 1e-6,
                    "{}: reported exact, {truth} vs {est}",
                    case.name
                );
                exact_calls += 1;
            } else {
                max_diff = max_diff.max((truth - est).abs());
                if !run.vs.ramping() {
                    settling = settling.max((truth - est).abs());
                }
            }
        }
        assert!(run.vs.latency_exact(), "{}: never exact again", case.name);
        assert!(exact_calls > 0);
        println!(
            "{:<48} largest |estimate - trajectory delay| while not exact: {max_diff:.2} frames \
             ({settling:.2} after the ramp, before the filters settle)",
            case.name
        );
    }
}

/// The smallest and largest step the latched call applies.
fn planned_range(vs: &Varispeed) -> (f64, f64) {
    planned_steps(vs)
        .into_iter()
        .fold((f64::INFINITY, 0.0f64), |(a, b), v| (a.min(v), b.max(v)))
}

/// Process the latched call on silence.
fn process_silence(vs: &mut Varispeed, n: usize) {
    let latch = vs.latched.expect("a plan");
    let ch = vs.channels();
    let x = vec![vec![0.0; latch.n_src]; ch];
    let mut y = vec![vec![0.0; n]; ch];
    let ins: Vec<&[f64]> = x.iter().map(|c| &c[..]).collect();
    let mut outs: Vec<&mut [f64]> = y.iter_mut().map(|c| &mut c[..]).collect();
    vs.process(&ins, &mut outs);
}

fn pull_silence(vs: &mut Varispeed, n: usize) -> (f64, f64) {
    vs.plan(n).unwrap();
    let r = planned_range(vs);
    process_silence(vs, n);
    r
}

/// A retarget near the end of a brake-free ramp, whose brake ends inside
/// the call: the piece must stop at the brake's end, not cross it on the
/// way to the ramp's end (it peaked at 2.001173, outside the core's range,
/// and the mirror case dipped to 0.499575, inside the core's range but
/// outside the stage's).
#[test]
fn a_retarget_never_crosses_the_brake_boundary() {
    for (s0, s1, s2) in [(0.5, 2.0, 1.999), (2.0, 0.5, 0.500_01)] {
        for cut in [[8192usize, 308], [1024, 1024]] {
            let mut vs = stage(G, 1, s0);
            vs.set_speed(s1, 0.0).unwrap();
            let mut done = 0;
            let mut i = 0;
            while done < 8500 {
                let n = cut[i % 2].min(8500 - done);
                i += 1;
                pull_silence(&mut vs, n);
                done += n;
            }
            vs.set_speed(s2, 20.0).unwrap();
            for _ in 0..40 {
                let (lo, hi) = pull_silence(&mut vs, 1024);
                assert!(
                    lo >= MIN_SPEED && hi <= MAX_SPEED,
                    "{s0} -> {s1} -> {s2}: steps {lo}..{hi}"
                );
            }
            assert_eq!(vs.fallbacks(), 0);
            assert_eq!(vs.speed(), s2);
        }
    }
}

/// The gain state of each frame of the latched call.
fn frame_fades(vs: &Varispeed) -> Vec<Fade> {
    let latch = vs.latched.expect("a plan");
    let mut f = latch.fade;
    let mut v = Vec::with_capacity(latch.n);
    for _ in 0..latch.n {
        if let Fade::Mute {
            left,
            jumped: false,
            extend,
        } = f
        {
            f = Fade::Mute {
                left,
                jumped: true,
                extend,
            };
        }
        v.push(f);
        f = vs.fade_step(f);
    }
    v
}

/// Hold at 1.5, cap at 1.0 (a cap drop), and from the first silent frame
/// on call `during(vs, frames since)` before every call of 64 frames, for
/// `frames` frames in all. Every call applies steps within the cap in
/// force at its plan, except on a fade-out. Returns the lengths of the
/// silent runs, and the stage.
fn cap_drop_with_commands(
    frames: usize,
    mut during: impl FnMut(&mut Varispeed, usize),
) -> (Vec<usize>, Varispeed) {
    let call = 64;
    let mut vs = stage(G, 1, 1.5);
    pull_silence(&mut vs, 512);
    vs.set_speed_limit(1.0);
    let (mut runs, mut run, mut since, mut done) = (Vec::new(), 0, None, 0);
    while done < frames {
        if let Some(t) = since {
            during(&mut vs, t);
        }
        vs.plan(call).unwrap();
        let latch = vs.latched.unwrap();
        assert!(!latch.fallback);
        let (lo, hi) = planned_range(&vs);
        assert!(lo >= MIN_SPEED && hi <= MAX_SPEED);
        if !matches!(latch.fade, Fade::Out { .. }) {
            assert!(
                hi <= vs.speed_limit(),
                "after {done} frames: step {hi} above the cap {}",
                vs.speed_limit()
            );
        }
        for f in frame_fades(&vs) {
            if let Fade::Mute { .. } = f {
                run += 1;
                since.get_or_insert(0);
            } else if run > 0 {
                runs.push(run);
                run = 0;
            }
        }
        process_silence(&mut vs, call);
        if let Some(t) = &mut since {
            *t += call;
        }
        done += call;
    }
    if run > 0 {
        runs.push(run);
    }
    (runs, vs)
}

/// A cap drop's silence ends by its deadline whatever commands arrive
/// during it: the deadline is fixed, a lower target moves it at most one
/// ÷2 span (`2 × mute_frames` in all), a higher one waits for the fade-in,
/// and the same target changes nothing.
#[test]
fn a_cap_drops_silence_ends_by_its_deadline() {
    let span = stage(G, 1, 1.0).mute_frames();
    // Once silent, the cap alternates between 1.05 and 1.0 every 128
    // frames for 12 800 frames. The rise waits, the return to 1.0 drops it:
    // the deadline never moves.
    let (runs, vs) = cap_drop_with_commands(30_000, |vs, t| {
        if t < 12_800 && t % 128 == 0 {
            vs.set_speed_limit([1.05, 1.0][(t / 128) % 2]);
        }
    });
    assert_eq!(runs[0], span, "{runs:?}");
    assert!(runs.iter().all(|&r| r <= 2 * span), "{runs:?}");
    // Later drops (from a ramp towards 1.05 under a cap of 1.0 again) mostly
    // turn back during their fade-out, when the cap rises again.
    assert!(runs.iter().sum::<usize>() < 12_800 / 4, "{runs:?}");
    println!(
        "cap 1.0 <-> 1.05 every 128 frames: silences of {runs:?} frames (bound {})",
        2 * span
    );
    assert_eq!(vs.fallbacks(), 0);
    // A cap lowered by 0.01 every 64 frames, through the silence and past
    // its deadline: every lower target jumps at once (the cap holds), the
    // deadline moves to cover them until it stands one span late, and the
    // silence ends there.
    let (runs, vs) = cap_drop_with_commands(30_000, |vs, t| {
        if t < 64 * 40 && t % 64 == 0 {
            vs.set_speed_limit(1.0 - 0.01 * (t / 64) as f64);
        }
    });
    assert_eq!(runs[0], 2 * span, "{runs:?}");
    assert!(runs.iter().all(|&r| r <= 2 * span), "{runs:?}");
    assert_eq!(vs.speed(), 0.61);
    assert!(!vs.ramping());
    println!("cap lowered every 64 frames: silences of {runs:?} frames");
    // Random speeds and caps on every call, through the silence and after.
    let mut seed = 5;
    let (runs, vs) = cap_drop_with_commands(30_000, |vs, t| {
        if t < 12_800 {
            if unit(&mut seed) < 0.3 {
                vs.set_speed(0.5 + 1.5 * unit(&mut seed), 0.0).unwrap();
            }
            if unit(&mut seed) < 0.3 {
                vs.set_speed_limit(0.6 + 1.4 * unit(&mut seed));
            }
        }
    });
    assert!(runs.iter().all(|&r| r <= 2 * span), "{runs:?}");
    assert_eq!(vs.fallbacks(), 0);
    println!("random commands: silences of {runs:?} frames");
}

/// The cap is exact: a cap a hair under the applied speed still needs the
/// fade (a relative slack of 1e-9 let a smooth ramp start at 1.0 under a
/// cap of 0.9999999995), and a ramp up to the cap never passes it.
#[test]
fn the_cap_is_exact() {
    let cap = 0.999_999_999_5;
    for after_the_fade in [false, true] {
        let mut vs = stage(G, 1, 1.5);
        pull_silence(&mut vs, 512);
        vs.set_speed_limit(1.0);
        while !matches!(vs.fade, Fade::In { .. }) {
            pull_silence(&mut vs, 64);
        }
        while after_the_fade && vs.fading() {
            pull_silence(&mut vs, 64);
        }
        assert_eq!(vs.speed(), 1.0);
        vs.set_speed_limit(cap);
        assert!(vs.peak_until_settled() >= 1.0);
        vs.plan(64).unwrap();
        assert!(matches!(vs.latched.unwrap().fade, Fade::Out { .. }));
        process_silence(&mut vs, 64);
        for _ in 0..40 {
            vs.plan(64).unwrap();
            let fade = vs.latched.unwrap().fade;
            let (_, hi) = planned_range(&vs);
            if !matches!(fade, Fade::Out { .. }) {
                assert!(hi <= cap, "{hi} above {cap}");
            }
            process_silence(&mut vs, 64);
        }
        assert_eq!(vs.speed(), cap);
    }
    // Ramps that end on the cap, from rest and mid-ramp: smooth, never
    // above it.
    for (from, cap, call) in [(1.0, 1.25, 1024), (0.5, 2.0, 8192), (0.7, 0.7123, 37)] {
        let mut vs = stage(G, 1, from);
        vs.set_speed_limit(cap);
        vs.set_speed(MAX_SPEED, 0.0).unwrap();
        let mut top = 0.0f64;
        for i in 0..3000 {
            if i > 3 && !vs.ramping() {
                break;
            }
            if i == 3 {
                vs.set_speed(MAX_SPEED, 50.0).unwrap();
            }
            vs.plan(call).unwrap();
            assert_eq!(vs.latched.unwrap().fade, Fade::Open);
            top = top.max(planned_range(&vs).1);
            process_silence(&mut vs, call);
        }
        assert!(top <= cap, "{from} -> {cap}: {top}");
        assert_eq!(vs.speed(), cap);
        assert_eq!(vs.fallbacks(), 0);
    }
}

/// `peak_until_settled` covers a call already latched: a ramp latched,
/// then a cap under it before `process` (the bound must include the latched
/// call, not just the cap it will settle to), and a case silent at 1.0, a
/// cap of 1.2, a plan, then a cap of 0.9.
#[test]
fn the_peak_bound_covers_a_latched_call() {
    let follow = |vs: &mut Varispeed, n: usize, bound: f64| {
        process_silence(vs, n);
        for _ in 0..80 {
            vs.plan(256).unwrap();
            let (_, hi) = planned_range(vs);
            assert!(hi <= bound * (1.0 + 1e-9), "{hi} above {bound}");
            process_silence(vs, 256);
        }
        assert!(!vs.ramping());
    };
    // A ramp latched, then the cap.
    let mut vs = stage(G, 1, 1.0);
    vs.set_speed(1.5, 0.0).unwrap();
    pull_silence(&mut vs, 2048);
    vs.plan(4096).unwrap();
    let (_, latched) = planned_range(&vs);
    assert!(latched > 1.01);
    vs.set_speed_limit(1.0);
    let bound = vs.peak_until_settled();
    assert!(bound >= latched, "{bound} under the latched {latched}");
    follow(&mut vs, 4096, bound);
    assert_eq!(vs.speed(), 1.0);

    // The rise waits for the fade-in, so the latched call holds at 1.0,
    // and the bound says so.
    let mut vs = stage(G, 1, 1.5);
    pull_silence(&mut vs, 512);
    vs.set_speed_limit(1.0);
    while !matches!(vs.fade, Fade::Mute { jumped: true, .. }) {
        pull_silence(&mut vs, 64);
    }
    vs.set_speed_limit(1.2);
    vs.plan(64).unwrap();
    let (_, latched) = planned_range(&vs);
    vs.set_speed_limit(0.9);
    let bound = vs.peak_until_settled();
    assert!(bound >= latched, "{bound} under the latched {latched}");
    follow(&mut vs, 64, bound);
    assert_eq!(vs.speed(), 0.9);
}

#[test]
fn the_tail_covers_the_whole_impulse_response() {
    // A drain that pads only the group delay cuts the response's second
    // half off; the tail count reaches its last non-zero output.
    for speed in [0.5, 1.0, 2.0] {
        let mut vs = stage(48_000, 1, speed);
        let n = 512;
        let mut out = Vec::new();
        let mut first = true;
        while out.len() < 8 * n {
            let n_src = vs.plan(n).unwrap();
            let mut src = vec![0.0; n_src];
            if first && n_src > 0 {
                src[0] = 1.0;
                first = false;
            }
            let mut y = vec![0.0; n];
            vs.process(&[&src[..]], &mut [&mut y[..]]);
            out.extend_from_slice(&y);
        }
        let last = out.iter().rposition(|x| x.abs() > 0.0).unwrap();
        let lat = vs.latency_frames().ceil() as usize;
        assert!(
            last > lat + 16,
            "s = {speed}: the response ends at {last}, delay {lat}"
        );
        assert!(
            last < vs.tail_frames(),
            "s = {speed}: last non-zero output {last}, tail {}",
            vs.tail_frames()
        );
        assert!(vs.tail_frames() <= 2 * lat + 8, "s = {speed}");
    }
}

/// Exactly `n` frames per call for any `n`, at most `max_source_frames`
/// source frames, at most 2 retained, and at most `ceil(n s) + 2` pulled,
/// through random commands, limits, zero calls and abandoned plans.
#[test]
fn pull_contract_holds_for_any_block_size() {
    let mut seed = 11;
    let mut run = Run::new(stage(G, 2, 1.0), false);
    let mut twin = Run::new(stage(G, 2, 1.0), false);
    let mut src = |j: u64| (j as f64 * 0.01).sin() * 0.5;
    let (mut max_over, mut max_ret) = (0usize, 0usize);
    let mut excess = 0i64;
    for i in 0..400 {
        let n = match i % 9 {
            0 => 0,
            1 => 1,
            2 => MAX_FRAMES,
            _ => (unit(&mut seed) * MAX_FRAMES as f64) as usize,
        };
        if unit(&mut seed) < 0.3 {
            let s = 0.5 + 1.5 * unit(&mut seed);
            let r = if unit(&mut seed) < 0.5 {
                0.0
            } else {
                2000.0 * unit(&mut seed)
            };
            run.vs.set_speed(s, r).unwrap();
            twin.vs.set_speed(s, r).unwrap();
        }
        if unit(&mut seed) < 0.05 {
            let lim = 1.0 + unit(&mut seed);
            run.vs.set_speed_limit(lim);
            twin.vs.set_speed_limit(lim);
        }
        if i % 7 == 3 {
            // An abandoned plan and a zero call leave no trace: the twin
            // never sees them.
            let a = run.vs.plan(n / 2 + 1).unwrap();
            assert!(a <= 2 * (n / 2 + 1) + 1);
            let mut none: [&mut [f64]; 2] = [&mut [], &mut []];
            assert_eq!(run.vs.plan(0).unwrap(), 0);
            run.vs.process(&[&[], &[]], &mut none);
        }
        let n_src = run.pull(n, &mut src);
        let t_src = twin.pull(n, &mut src);
        assert_eq!(n_src, t_src);
        assert_eq!(run.out[0][..n], twin.out[0][..n], "call {i}");
        let core = run.vs.core().accumulator();
        max_ret = max_ret.max(core.retained());
        if n > 0 {
            max_over = max_over.max(n_src.saturating_sub(2 * n));
            let s_max = run.last_peak;
            excess = excess.max(n_src as i64 - (n as f64 * s_max).ceil() as i64);
        } else {
            assert_eq!(n_src, 0);
        }
    }
    assert_eq!(run.vs.fallbacks(), 0);
    println!(
        "400 random pulls: retained at most {max_ret}, source frames at most 2n + {max_over}, \
         at most ceil(n s) + {excess} (s the call's largest step)"
    );
    assert!(max_ret <= crate::adaptive::MAX_RETAINED);
    assert!(max_over <= 1);
    assert!(excess <= 2, "the over-pull bound is ceil(n s) + 2");
}

/// Random command sequences (speeds, ramps, caps) at three rates (four
/// sequences each) and
/// random call sizes: every piece applied stays within the stage's
/// [0.5, 2] and, outside a cap drop's fade-out, under the cap; the source
/// frames `max_source_frames` gives before the plan are the plan's; and no
/// call falls back.
#[test]
fn random_commands_keep_every_piece_within_the_stage_limits() {
    let mut seed = 77;
    let mut fades = 0;
    let mut calls = 0;
    let mut most_parts = 0;
    for case in 0..12 {
        let rate = [44_100u32, 24_000, 96_000][case % 3];
        let mut vs = stage(rate, 1, 0.5 + 1.5 * unit(&mut seed));
        for i in 0..120 {
            if unit(&mut seed) < 0.25 {
                let s = 0.5 + 1.5 * unit(&mut seed);
                let r = [0.0, 20.0, 42.65, 200.0, 3000.0][(unit(&mut seed) * 5.0) as usize];
                vs.set_speed(s, r).unwrap();
            }
            if unit(&mut seed) < 0.08 {
                vs.set_speed_limit(if unit(&mut seed) < 0.3 {
                    f64::INFINITY
                } else {
                    0.6 + 1.4 * unit(&mut seed)
                });
            }
            let n = if i % 10 == 9 {
                MAX_FRAMES
            } else {
                [0, 1, 37, 256, 1024, 2048][(unit(&mut seed) * 6.0) as usize]
            };
            let preview = vs.max_source_frames(n);
            let bound = vs.peak_until_settled();
            let n_src = vs.plan(n).unwrap();
            assert_eq!(n_src, preview, "case {case} call {i}");
            assert!(n_src <= 2 * n + 1);
            let latch = vs.latched.unwrap();
            assert!(!latch.fallback, "case {case} call {i}");
            most_parts = most_parts.max(latch.n_parts);
            let (lo, hi) = planned_range(&vs);
            // The peak bound, before the plan and with it latched.
            assert!(
                hi <= bound * (1.0 + 1e-9) && hi <= vs.peak_until_settled() * (1.0 + 1e-9),
                "case {case} call {i}: step {hi} above the bound {bound}"
            );
            assert!(
                lo >= MIN_SPEED && hi <= MAX_SPEED,
                "case {case} call {i}: steps {lo}..{hi}"
            );
            if matches!(latch.fade, Fade::Open | Fade::In { .. }) {
                assert!(
                    hi <= vs.speed_limit(),
                    "case {case} call {i}: step {hi} above the cap {}",
                    vs.speed_limit()
                );
            } else {
                fades += 1;
            }
            calls += 1;
            process_silence(&mut vs, n);
        }
        assert_eq!(vs.fallbacks(), 0);
    }
    println!(
        "{calls} calls, {fades} during a cap drop's fade, at most {most_parts} core calls in one \
         (limit {MAX_PARTS})"
    );
}
