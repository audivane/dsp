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
use crate::resample::{ManualConfig, UpsamplerDesign, try_design_kernel_for};
use audivane_protocol::{BandSpec, CutoffMode, FilterPhase};
use realfft::RealFftPlanner;
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

/// The steep filters from the design builder are the band design the
/// stage used before (`UpsamplerDesign::try_for_ratio` and
/// `try_design_kernel_for`), bit for bit, at both passbands and at every
/// graph rate class.
#[test]
fn steep_filters_from_the_builder_equal_the_band_design() {
    for passband_end in [STEEP_PASSBAND, RELAXED_PASSBAND] {
        let cfg = ManualConfig {
            cutoff_mode: CutoffMode::Band,
            band: BandSpec {
                passband_end,
                stopband_start: STEEP_STOPBAND,
                passband_end_hz: None,
                attenuation_db: STEEP_ATTENUATION_DB,
            },
            phase: FilterPhase::Linear,
            ..ManualConfig::default()
        };
        // The catalogue entry is today's Custom band design.
        assert_eq!(
            catalogue::varispeed(passband_end),
            FilterDesign::Kernel(design::paths::standard::design_of(&cfg).unwrap())
        );
        let old = |ratio: RateRatio| {
            let d = UpsamplerDesign::try_for_ratio(&cfg, ratio).unwrap();
            try_design_kernel_for(&d, ratio).unwrap()
        };
        let (up, down) = (old(RateRatio::new(2, 1)), old(RateRatio::new(1, 2)));
        for rate in [44_100u32, 48_000, 96_000, 192_000, 384_000, 768_000] {
            let new = SteepDesign::from_design(&catalogue::varispeed(passband_end), rate).unwrap();
            let bits = |h: &[f64]| h.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
            assert_eq!(bits(&new.up), bits(&up), "{passband_end} at {rate}");
            assert_eq!(bits(&new.down), bits(&down), "{passband_end} at {rate}");
            assert_eq!(new.up_delay(), (up.len() - 1) as f64 / 2.0);
            assert_eq!(new.down_delay(), (down.len() - 1) as f64 / 2.0);
            assert_eq!(new.passband_end, Some(passband_end));
        }
    }
}

/// An optional user design: any windowed sinc or Lanczos design that
/// builds at 2/1 and 1/2 replaces the built-in filters; the runtime kinds
/// and designs that do not build at both ratios are refused.
#[test]
fn user_steep_designs() {
    use crate::design::{Accept, FilterPreset};
    let preset = |p| catalogue::design(p);
    // A preset windowed sinc on another arithmetic path.
    let d = SteepDesign::from_design(&preset(FilterPreset::KaiserSteepShort), G).unwrap();
    assert_eq!(d.passband_end, None);
    // The `fir` path's delay is its sinc centre, `floor(N / 2)`.
    assert_eq!(d.up_delay(), (d.up.len() / 2) as f64);
    // A Lanczos design that accepts decimation.
    let FilterDesign::Kernel(lz) = preset(FilterPreset::Lanczos23) else {
        unreachable!()
    };
    let any = FilterDesign::Kernel(KernelDesign {
        rates: design::Rates {
            accept: Accept::Any,
            ..lz.rates
        },
        ..lz
    });
    let l = SteepDesign::from_design(&any, G).unwrap();
    assert!(!l.up.is_empty() && !l.down.is_empty());
    // The catalogue's Lanczos only upsamples: refused at 1/2.
    let e = SteepDesign::from_design(&preset(FilterPreset::Lanczos23), G).unwrap_err();
    assert!(e.to_string().contains("1/2"), "{e}");
    // A minimum-phase design builds, with its delay at the peak.
    let mp = SteepDesign::from_design(&preset(FilterPreset::KaiserSteepShortMp), G).unwrap();
    assert!(mp.up_delay() < (mp.up.len() - 1) as f64 / 4.0);
    // The runtime kinds have no kernel.
    for p in [
        FilterPreset::CascadeFir,
        FilterPreset::Cosine,
        FilterPreset::BlockFft,
    ] {
        let e = SteepDesign::from_design(&preset(p), G).unwrap_err();
        assert!(e.to_string().contains("no kernel"), "{p:?}: {e}");
    }

    // A stage on the user design: a passband tone comes through at the
    // stage's gain and latency.
    let spec = VarispeedSpec {
        sample_rate: G,
        channels: 1,
        max_frames: 1024,
        speed: 1.0,
    };
    let mut vs = Varispeed::with_design(spec, d.clone()).unwrap();
    assert_eq!(vs.design(), &d);
    let latency = vs.latency_at(1.0);
    assert_eq!(
        latency,
        (d.up_delay() + vs.core().delay()) / 2.0 + d.down_delay() / 2.0
    );
    let w = 2.0 * PI * 1_000.0 / f64::from(G);
    let (mut j, mut m) = (0u64, 0usize);
    let mut worst = 0.0f64;
    let mut out = vec![0.0; 1024];
    while m < 20_000 {
        let n_src = vs.plan(1024).unwrap();
        let src: Vec<f64> = (0..n_src)
            .map(|_| {
                j += 1;
                (w * (j - 1) as f64).sin()
            })
            .collect();
        vs.process(&[&src], &mut [&mut out]);
        for (i, &y) in out.iter().enumerate() {
            let t = (m + i) as f64 - latency;
            if m + i > 4 * d.up.len() {
                worst = worst.max((y - (w * t).sin()).abs());
            }
        }
        m += 1024;
    }
    println!(
        "user design ({} taps): 1 kHz tone error {:.1} dB",
        d.up.len(),
        db(worst)
    );
    // At the stated latency: one frame off would measure about -16 dB.
    assert!(db(worst) < -150.0, "{}", db(worst));
}

#[test]
fn steep_designs_and_constants() {
    let d = SteepDesign::for_rate(44_100).unwrap();
    assert_eq!(d.passband_end, Some(STEEP_PASSBAND));
    assert_eq!((d.up.len(), d.down.len()), (524, 524));
    assert_eq!((d.up_delay(), d.down_delay()), (261.5, 261.5));
    let sum_up: f64 = d.up.iter().sum();
    let sum_down: f64 = d.down.iter().sum();
    assert!((sum_up - 2.0).abs() < 1e-9 && (sum_down - 1.0).abs() < 1e-9);
    assert_eq!(SteepDesign::for_rate(192_000).unwrap().up.len(), 524);
    let r = SteepDesign::for_rate(192_001).unwrap();
    assert_eq!(r.passband_end, Some(RELAXED_PASSBAND));
    assert_eq!((r.up.len(), r.down.len()), (252, 252));

    let vs = stage(44_100, 2, 1.0);
    assert_eq!(vs.latency_at(1.0), 267.5);
    assert_eq!(vs.latency_at(0.5), 404.25);
    assert!((vs.latency_at(2.0) - 199.125).abs() < 1e-12);
    assert_eq!(vs.latency_frames(), 267.5);
    assert!(vs.latency_exact());
    assert_eq!(
        [0.5, 1.0, 2.0].map(|s| vs.macs_per_frame(s)),
        [986.0, 1248.0, 1772.0]
    );
    let hi = stage(768_000, 2, 1.0);
    assert_eq!(hi.latency_at(1.0), 68.75 + 62.75);
    assert_eq!([1.0, 2.0].map(|s| hi.macs_per_frame(s)), [704.0, 956.0]);
    // Latency per common rate at speed 1, for the record.
    for rate in [44_100u32, 48_000, 96_000, 192_000, 384_000, 768_000] {
        let v = stage(rate, 2, 1.0);
        println!(
            "{rate} Hz: latency {} frames = {:.3} ms at s = 1 ({:.2} at 0.5, {:.3} at 2)",
            v.latency_at(1.0),
            v.latency_at(1.0) / f64::from(rate) * 1e3,
            v.latency_at(0.5),
            v.latency_at(2.0)
        );
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
    for i in 0..3000 {
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
        "3000 random pulls: retained at most {max_ret}, source frames at most 2n + {max_over}, \
         at most ceil(n s) + {excess} (s the call's largest step)"
    );
    assert!(max_ret <= crate::adaptive::MAX_RETAINED);
    assert!(max_over <= 1);
    assert!(excess <= 2, "the over-pull bound is ceil(n s) + 2");
}

/// A command given between `plan` and `process` waits for the next call;
/// a reset holds at the target with no ramp.
#[test]
fn commands_latch_at_plan_and_reset_holds_at_the_target() {
    let mut run = Run::new(stage(G, 1, 1.0), false);
    let mut zero = |_| 0.0;
    run.pull(512, &mut zero);
    let n_src = run.vs.plan(512).unwrap();
    run.vs.set_speed(2.0, 0.0).unwrap();
    let x = vec![0.0; n_src];
    let mut y = vec![0.0; 512];
    run.vs.process(&[&x], &mut [&mut y]);
    assert_eq!(run.vs.speed(), 1.0, "the command waits for the next call");
    assert!(run.vs.ramping());
    run.pull(512, &mut zero);
    assert!(run.vs.speed() > 1.0);
    run.vs.reset();
    assert_eq!(run.vs.speed(), 2.0);
    assert!(!run.vs.ramping());
    assert_eq!(run.vs.core().accumulator().position(), 0);
    // Ramps end on the target's bits and hold there.
    run.vs.set_speed(1.0123, 50.0).unwrap();
    for _ in 0..20 {
        run.pull(1024, &mut zero);
    }
    assert!(!run.vs.ramping());
    assert_eq!(run.vs.step(), Step::from_f64(1.0123).unwrap());
    assert_eq!(run.vs.fallbacks(), 0);
}

#[test]
fn the_speed_limit_caps_the_target_and_keeps_the_request() {
    let mut run = Run::new(stage(G, 1, 1.0), false);
    let mut zero = |_| 0.0;
    run.vs.set_speed(1.8, 0.0).unwrap();
    run.vs.set_speed_limit(1.25);
    assert_eq!(run.vs.target(), 1.25);
    assert_eq!(run.vs.requested(), 1.8);
    for _ in 0..30 {
        run.pull(1024, &mut zero);
        assert!(run.vs.speed() <= 1.25 + 1e-12);
    }
    assert_eq!(run.vs.speed(), 1.25);
    // The cap rises: back to the request.
    run.vs.set_speed_limit(f64::INFINITY);
    assert_eq!(run.vs.target(), 1.8);
    for _ in 0..30 {
        run.pull(1024, &mut zero);
    }
    assert_eq!(run.vs.speed(), 1.8);
    // A cap below the applied speed ramps down at once; a request below
    // the cap is unaffected.
    run.vs.set_speed_limit(1.1);
    for _ in 0..30 {
        run.pull(1024, &mut zero);
    }
    assert_eq!(run.vs.speed(), 1.1);
    run.vs.set_speed(0.9, 0.0).unwrap();
    for _ in 0..30 {
        run.pull(1024, &mut zero);
    }
    assert_eq!(run.vs.speed(), 0.9);
    run.vs.set_speed_limit(0.1);
    assert_eq!(run.vs.speed_limit(), MIN_SPEED);
    assert_eq!(run.vs.target(), 0.5);
}

/// `T_min`, the curvature limit, and the rules for commands mid-ramp:
/// random command sequences keep the step within the hull of the speeds
/// asked for, C¹, and every ramp lands on its target's bits.
#[test]
fn envelope_rules() {
    let rate = 2.0 * f64::from(G);
    let env = Envelope::new(rate, Step::ONE);
    let ms = |k: f64| k / rate * 1e3;
    // T_min = 200 ms sqrt(|ln(s1/s0)| / ln 4).
    assert!((ms(env.cosine_len(4f64.ln())) - 200.0).abs() < 1e-9);
    let t108 = ms(env.cosine_len(1.08f64.ln()));
    assert!((t108 - 200.0 * (1.08f64.ln() / 4f64.ln()).sqrt()).abs() < 1e-9);
    println!("T_min for 1 -> 1.08: {t108:.1} ms");
    assert!((t108 - 47.1).abs() < 0.1);

    // Curvature of the step at speed 2: s (l'' + l'^2), l' at most about
    // the reference's peak slope.
    let c_max = 2.0 * env.curvature_limit() * 2.0;
    let mut max_jump = 0.0f64;
    let mut seed = 5;
    for case in 0..200 {
        let mut run = Run::new(stage(G, 1, 1.0), false);
        let (mut lo, mut hi) = (1.0f64, 1.0f64);
        let (mut prev_step, mut prev_inc) = (None::<f64>, None::<f64>);
        for i in 0..120 {
            if i % 8 == 0 || unit(&mut seed) < 0.1 {
                let s = 0.5 + 1.5 * unit(&mut seed);
                let r = [0.0, 50.0, 400.0, 3000.0][(unit(&mut seed) * 4.0) as usize];
                run.vs.set_speed(s, r).unwrap();
                lo = lo.min(s);
                hi = hi.max(s);
            }
            let n = [1024, 256, 1, 4096, 777][(case + i) % 5];
            let n_src = run.vs.plan(n).unwrap();
            let after = run.vs.latched.unwrap().env;
            assert!(after.target().to_f64() <= hi && after.target().to_f64() >= lo);
            let steps = planned_steps(&run.vs);
            // C¹: the step's increments run on smoothly across calls; their
            // jumps stay within a few times the curvature limit (in step
            // units at speed 2).
            for &v in &steps {
                if let Some(p) = prev_step {
                    let inc: f64 = v - p;
                    if let Some(pi) = prev_inc {
                        let jump: f64 = inc - pi;
                        max_jump = max_jump.max(jump.abs());
                        assert!(
                            jump.abs() <= 4.0 * c_max + 1e-15,
                            "case {case} call {i}: {jump:e}"
                        );
                    }
                    prev_inc = Some(inc);
                }
                prev_step = Some(v);
            }
            let x = vec![0.0; n_src];
            let mut y = vec![0.0; n];
            run.vs.process(&[&x], &mut [&mut y]);
            let s = run.vs.speed();
            assert!(
                s >= lo * (1.0 - 1e-9) && s <= hi * (1.0 + 1e-9),
                "case {case} call {i}: speed {s} outside {lo}..{hi}"
            );
            if !run.vs.ramping() {
                assert_eq!(run.vs.speed(), run.vs.target());
            }
        }
        assert_eq!(run.vs.fallbacks(), 0);
    }
    println!(
        "largest change of the step's increment between outputs: {max_jump:.2e} (limit {:.2e})",
        4.0 * c_max
    );
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

/// The rational path at 160/147, 147/160 and 1/2 against its
/// own transfer function, and the stage at `s = M/L` (the same output
/// instants) against its own: both within a few dB of their floors.
#[test]
fn rational_path_and_stage_at_the_same_instants() {
    let cases: [(u32, u32, u32, [f64; 3]); 3] = [
        (160, 147, 44_100, [997.0, 15_003.0, 19_997.0]),
        (147, 160, 48_000, [997.0, 15_003.0, 19_997.0]),
        (1, 2, 44_100, [997.0, 5_003.0, 9_997.0]),
    ];
    let ph = 0.3;
    let a = 0.5;
    for (l, m, rate, freqs) in cases {
        let ratio = RateRatio::new(l, m);
        let cfg = ManualConfig {
            cutoff_mode: CutoffMode::Band,
            band: BandSpec {
                passband_end: STEEP_PASSBAND,
                stopband_start: STEEP_STOPBAND,
                passband_end_hz: None,
                attenuation_db: STEEP_ATTENUATION_DB,
            },
            phase: FilterPhase::Linear,
            ..ManualConfig::default()
        };
        let d = UpsamplerDesign::try_for_ratio(&cfg, ratio).unwrap();
        let h = try_design_kernel_for(&d, ratio).unwrap();
        let c = (h.len() - 1) as f64 / 2.0;
        let s = f64::from(m) / f64::from(l);
        let vs = stage(rate, 1, s);
        let model = Model::of(&vs);
        for f in freqs {
            let w = 2.0 * PI * f / f64::from(rate);
            // The rational path.
            let gain = zero_phase(&h, w / f64::from(l)) / f64::from(l);
            let mut r = RationalResampler::from_kernel(&h, ratio, c, 1, 1024);
            let mut src = vec![0.0; 1024];
            let mut out = vec![0.0; ratio.max_frames_out(1024)];
            let (mut k, mut j, mut worst_r) = (0usize, 0u64, 0.0f64);
            while k < 60_000 {
                for v in src.iter_mut() {
                    *v = a * (w * j as f64 + ph).sin();
                    j += 1;
                }
                let n = r.output_frames(1024);
                r.process(&[&src], &mut [&mut out[..n]]);
                for (i, v) in out[..n].iter().enumerate() {
                    let mm = (k + i) as f64;
                    let t = (mm * f64::from(m) - c) / f64::from(l);
                    if k + i > 8192 {
                        worst_r = worst_r.max((v - a * gain * (w * t + ph).sin()).abs());
                    }
                }
                k += n;
            }
            // The stage at s = M/L.
            let g = model.gain(f, s, model.core_gain(f));
            let mut run = Run::new(stage(rate, 1, s), false);
            let mut x = |j: u64| a * (w * j as f64 + ph).sin();
            let mut worst_s = 0.0f64;
            while run.produced < 60_000 {
                let m0 = run.produced;
                run.pull(1024, &mut x);
                for i in 0..1024 {
                    let mm = (m0 + i) as f64;
                    if mm > 8192.0 {
                        let want = a * g * (w * model.source_time(mm, s) + ph).sin();
                        worst_s = worst_s.max((run.out[0][i] - want).abs());
                    }
                }
            }
            let (dr, ds) = (db(worst_r), db(worst_s));
            println!(
                "{l}/{m} at {rate} Hz, {f} Hz: rational ({} grid taps) {dr:.1} dBFS, \
                 stage at s = {s:.5} {ds:.1} dBFS",
                h.len()
            );
            // Each near its own floor: the rational path on these edges
            // measures -170 to -177 dBFS (or far below on the plain
            // decimator), the stage -176 to -212.
            assert!(dr <= -165.0 && ds <= -170.0, "{dr:.1} / {ds:.1}");
        }
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

/// The stage's own error through steps, envelopes, mid-ramp retargets, a
/// reversal and a speed cap, against the core's exact output for the tone at
/// each logged position passed through the same ÷2 (so the intended
/// modulation is in both and cancels): 8 signal phases (from the zero
/// crossing on) at 997 Hz and 10 007 Hz, and a log sweep, at -6 dBFS, with
/// mixed call sizes. Target <= -150 dBFS.
#[test]
fn speed_changes_against_the_reference_through_the_same_decimator() {
    let blocks = [1024usize, 1024, 333, 1, 4096, 1024, 2048, 64, 1024, 8192];
    let a = 0.5;
    let mut worst_all = f64::NEG_INFINITY;
    for case in CHANGE_CASES {
        let mut row = format!("{:<48}", case.name);
        for f in [997.0, 10_007.0, 0.0] {
            let sweep = f == 0.0;
            let mut worst = 0.0f64;
            for phase in 0..if sweep { 1 } else { 8 } {
                let mut run = Run::new(stage(G, 1, case.s0), true);
                let model = Model::of(&run.vs);
                let d = &run.vs.design().down;
                let mut rf = RationalResampler::from_kernel(
                    d,
                    RateRatio::new(1, 2),
                    model.dd,
                    1,
                    2 * MAX_FRAMES,
                );
                let w = 2.0 * PI * f / f64::from(G);
                // Phase k π/4 at the source time audible when the change
                // starts.
                let ph = phase as f64 * PI / 4.0
                    - w * case.s0 * (START as f64 - run.vs.latency_at(case.s0));
                let g = if sweep {
                    1.0
                } else {
                    zero_phase(&run.vs.design().up, w / 2.0) / 2.0 * model.core_gain(f)
                };
                // Log sweep 50 Hz -> 9 kHz over the most source time the
                // run can take (in the passband at every speed up to 2).
                let span = 2.0 * case.total as f64 / f64::from(G);
                let ls = span / (9_000.0f64 / 50.0).ln();
                let sweep_phase =
                    |tau: f64| 2.0 * PI * 50.0 * ls * ((tau / f64::from(G) / ls).exp() - 1.0);
                let mut x = |j: u64| {
                    if sweep {
                        a * sweep_phase(j as f64).sin()
                    } else {
                        a * (w * j as f64 + ph).sin()
                    }
                };
                let mut z = vec![0.0; 2 * MAX_FRAMES];
                let mut refo = vec![0.0; MAX_FRAMES];
                let (mut ev, mut bi) = (0usize, 0usize);
                while run.produced < case.total {
                    while ev < case.events.len() && run.produced >= case.events[ev].0 {
                        apply(&mut run.vs, case.events[ev].1);
                        ev += 1;
                    }
                    let n = blocks[bi % blocks.len()];
                    bi += 1;
                    let before = run.log.as_ref().unwrap().len();
                    let m0 = run.produced;
                    run.pull(n, &mut x);
                    let log = run.log.as_ref().unwrap();
                    let n_core = log.len() - before;
                    for (zk, p) in z.iter_mut().zip(&log[before..]) {
                        let tau = (p - model.dc - model.du) / 2.0;
                        *zk = if sweep {
                            a * sweep_phase(tau).sin()
                        } else {
                            a * g * (w * tau + ph).sin()
                        };
                    }
                    rf.process(&[&z[..n_core]], &mut [&mut refo[..n]]);
                    if m0 > 4096 {
                        for i in 0..n {
                            worst = worst.max((run.out[0][i] - refo[i]).abs());
                        }
                    }
                }
                assert_eq!(run.vs.fallbacks(), 0);
            }
            let r = db(worst);
            worst_all = worst_all.max(r);
            row += &if sweep {
                format!(" sweep {r:7.1}")
            } else {
                format!(" {f:>6} Hz {r:7.1}")
            };
            assert!(r <= -150.0, "{}: {f} Hz: {r:.1} dBFS", case.name);
        }
        println!("{row}");
    }
    println!("worst: {worst_all:.1} dBFS");
}

/// The core positions of a case's trajectory (silent input, calls of
/// `block` frames, every event and the end `offset` frames later),
/// ×2-stream frames.
fn trajectory(case: &ChangeCase, block: usize, offset: usize) -> Vec<f64> {
    let mut run = Run::new(stage(G, 1, case.s0), true);
    let mut zero = |_| 0.0;
    let mut ev = 0;
    while run.produced < case.total + 2 * offset {
        while ev < case.events.len() && run.produced >= case.events[ev].0 + offset {
            apply(&mut run.vs, case.events[ev].1);
            ev += 1;
        }
        run.pull(block, &mut zero);
    }
    assert_eq!(run.vs.fallbacks(), 0);
    run.log.unwrap()
}

/// The splatter metric of the design note: the ideal output `sin(2π f
/// p(t) + φ)` along the trajectory (no resampler), Kaiser-windowed (β 38)
/// over 1 s centred on the change, at the core's 2G rate; the largest level
/// more than 500 Hz outside the swept band up to the output Nyquist, re
/// the tone, worst of 16 phases.
fn splatter(pos: &[f64], f: f64) -> f64 {
    let rate = 2.0 * f64::from(G);
    let steps: Vec<f64> = pos.windows(2).map(|w| w[1] - w[0]).collect();
    let s0 = steps[0];
    let s1 = *steps.last().unwrap();
    // (Steps from f64 positions carry rounding noise near 1e-11.)
    let first = steps.iter().position(|&v| (v - s0).abs() > 1e-9).unwrap();
    let last = steps.iter().rposition(|&v| (v - s1).abs() > 1e-9).unwrap();
    let n = rate as usize;
    let centre = (first + last) / 2;
    assert!(
        centre >= n / 2 && centre + n / 2 <= pos.len(),
        "window outside the run"
    );
    let w0 = centre - n / 2;
    assert!(last - first < n, "the change is longer than the window");
    let (lo, hi) = steps[w0..w0 + n - 1]
        .iter()
        .fold((f64::INFINITY, 0.0f64), |(a, b), &v| (a.min(v), b.max(v)));
    let beta = 38.0;
    let i0 = crate::window::bessel_i0(beta);
    let win: Vec<f64> = (0..n)
        .map(|i| {
            let u = 2.0 * i as f64 / (n - 1) as f64 - 1.0;
            crate::window::bessel_i0(beta * (1.0 - u * u).max(0.0).sqrt()) / i0
        })
        .collect();
    let norm = win.iter().sum::<f64>() / 2.0;
    let fft = RealFftPlanner::<f64>::new().plan_fft_forward(n);
    let mut worst = f64::NEG_INFINITY;
    for k in 0..16 {
        let ph = k as f64 * PI / 8.0;
        let mut x = fft.make_input_vec();
        for (i, v) in x.iter_mut().enumerate() {
            // p in source frames: half the ×2-stream position.
            let src_s = pos[w0 + i] / rate;
            *v = (2.0 * PI * f * src_s + ph).sin() * win[i];
        }
        let mut y = fft.make_output_vec();
        fft.process(&mut x, &mut y).unwrap();
        for (b, c) in y.iter().enumerate() {
            let fr = b as f64 * rate / n as f64;
            let outside = fr < f * lo - 500.0 || fr > f * hi + 500.0;
            if outside && fr > 20.0 && fr < f64::from(G) / 2.0 {
                worst = worst.max(db(c.norm() / norm));
            }
        }
    }
    worst
}

/// The intended modulation of the trajectories the planner
/// really produces (one cubic piece per 1024-frame call), for every kind of
/// command within the envelope: <= -120 dB re the tone at 1 and 10 kHz.
#[test]
fn intended_modulation_of_the_planned_trajectories() {
    let cases: Vec<&ChangeCase> = CHANGE_CASES
        .iter()
        .filter(|c| !c.events.iter().any(|(_, e)| matches!(e, Change::Step(_))))
        .collect();
    // A slider drag: a command every 50 ms, +0.01 each.
    const DRAG: &[(usize, Change)] = &[
        (START, Change::Speed(1.01, 200.0)),
        (START + 2_205, Change::Speed(1.02, 200.0)),
        (START + 4_410, Change::Speed(1.03, 200.0)),
        (START + 6_615, Change::Speed(1.04, 200.0)),
        (START + 8_820, Change::Speed(1.05, 200.0)),
        (START + 11_025, Change::Speed(1.06, 200.0)),
    ];
    let drag = ChangeCase {
        name: "slider drag 1 -> 1.06, +0.01 every 50 ms, 200 ms",
        s0: 1.0,
        events: DRAG,
        total: START + 30_000,
    };
    let fast = ChangeCase {
        name: "1 -> 1.08 at T_min (47 ms)",
        s0: 1.0,
        events: &[(START, Change::Speed(1.08, 0.0))],
        total: START + 6_000,
    };
    // Changes whose T_min is shorter than the 20 ms floor (0.6 ms and
    // 4.3 ms): ramp 0 gets the floor.
    let tiny = ChangeCase {
        name: "1 -> 1.000013, ramp 0 (the 20 ms floor)",
        s0: 1.0,
        events: &[(START, Change::Speed(1.000_013, 0.0))],
        total: START + 6_000,
    };
    let small = ChangeCase {
        name: "1 -> 1.001, ramp 0 (the 20 ms floor)",
        s0: 1.0,
        events: &[(START, Change::Speed(1.001, 0.0))],
        total: START + 6_000,
    };
    let mut worst_all = f64::NEG_INFINITY;
    for case in cases.into_iter().chain([&drag, &fast, &tiny, &small]) {
        // Room for the 1 s window on both sides.
        let pos = trajectory(case, 1024, 30_000);
        let r1 = splatter(&pos, 1_000.0);
        let r10 = splatter(&pos, 10_000.0);
        worst_all = worst_all.max(r1.max(r10));
        println!("{:<50} 1 kHz {r1:7.1} dB, 10 kHz {r10:7.1} dB", case.name);
        assert!(r1 <= -120.0 && r10 <= -120.0, "{}", case.name);
    }
    println!("worst: {worst_all:.1} dB re the tone");
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

/// Random command sequences (speeds, ramps, caps) at three rates and
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
    for case in 0..60 {
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

/// The case where the earlier peak-based bound fell one frame short
/// (24 kHz, 1 024 frames: 1 846 against 1 847).
#[test]
fn the_source_frames_given_before_the_plan_are_the_plans() {
    let mut vs = stage(24_000, 1, 0.5);
    vs.set_speed(1.8, 0.0).unwrap();
    let mut done = 0;
    while done < 4444 {
        let n = 1024.min(4444 - done);
        pull_silence(&mut vs, n);
        done += n;
    }
    vs.set_speed(1.7999, 42.65).unwrap();
    for _ in 0..20 {
        let preview = vs.max_source_frames(1024);
        assert_eq!(vs.plan(1024).unwrap(), preview);
        process_silence(&mut vs, 1024);
    }
}

/// The cap-drop rule: a cap of 1.1 at every phase of a 0.5 -> 2 ramp at
/// `T_min`. The speed never exceeds `peak_until_settled()` as given at the
/// drop, never exceeds the cap from the jump on (or at all, when the
/// envelope can stop below it), the output is silent from the jump until
/// the ÷2 has cleared it, and the stage settles at the cap.
#[test]
fn a_cap_drop_at_every_phase_of_a_ramp() {
    let cap = 1.1;
    let mut faded = 0;
    let mut worst_peak = 0.0f64;
    for phase in 0..=40 {
        let mut run = Run::new(stage(G, 1, 0.5), false);
        let mut tone = |j: u64| 0.5 * (0.05 * j as f64).sin();
        while run.produced < 4096 {
            run.pull(256, &mut tone);
        }
        run.vs.set_speed(2.0, 0.0).unwrap();
        let drop_at = 4096 + phase * 8820 / 40;
        while run.produced < drop_at {
            run.pull(256, &mut tone);
        }
        let before = run.vs.speed();
        run.vs.set_speed_limit(cap);
        let peak = run.vs.peak_until_settled();
        assert!(peak >= before);
        let mut seen_fade = false;
        let mut jumped = false;
        let mut top = 0.0f64;
        let mut silent = 0usize;
        while run.produced < drop_at + 30_000 {
            run.vs.plan(256).unwrap();
            let latch = run.vs.latched.unwrap();
            let (_, hi) = planned_range(&run.vs);
            top = top.max(hi);
            assert!(
                hi <= peak * (1.0 + 1e-9),
                "phase {phase}: {hi} above {peak}"
            );
            seen_fade |= latch.fade != Fade::Open;
            if !seen_fade || jumped || latch.fade == Fade::Open {
                assert!(
                    hi <= cap * (1.0 + 1e-9),
                    "phase {phase}: {hi} above the cap after the jump"
                );
            }
            let mut f = latch.fade;
            let x: Vec<f64> = (0..latch.n_src).map(|j| tone(run.fed + j as u64)).collect();
            let mut y = vec![0.0; 256];
            run.vs.process(&[&x], &mut [&mut y]);
            run.fed += latch.n_src as u64;
            run.produced += 256;
            for v in &y {
                if let Fade::Mute { .. } = f {
                    assert_eq!(*v, 0.0);
                    silent += 1;
                }
                if let Fade::Mute {
                    jumped: false,
                    left,
                    extend,
                } = f
                {
                    f = Fade::Mute {
                        left,
                        jumped: true,
                        extend,
                    };
                }
                f = run.vs.fade_step(f);
            }
            jumped |= matches!(
                run.vs.fade,
                Fade::Mute { jumped: true, .. } | Fade::In { .. }
            );
        }
        assert!(!run.vs.fading());
        assert_eq!(run.vs.speed(), cap, "phase {phase}");
        assert_eq!(run.vs.fallbacks(), 0);
        if phase == 20 {
            println!(
                "a retarget halfway (speed {before:.5}): peak_until_settled {peak:.5}, \
                 {}",
                if seen_fade { "faded" } else { "smooth" }
            );
        }
        if seen_fade {
            faded += 1;
            assert_eq!(silent, run.vs.mute_frames());
            worst_peak = worst_peak.max(peak);
        }
        assert!(top <= peak * (1.0 + 1e-9));
    }
    println!(
        "a cap of {cap} at 41 phases of 0.5 -> 2: {faded} faded, the highest peak_until_settled \
         {worst_peak:.4} (the smooth brake alone reached 1.414)"
    );
    assert!(faded > 0 && faded < 41);
}

/// The splatter of the cap drop's fades, on the stage's real output: hold
/// at 1.5, then a cap of 1.0 (above the cap, so the stage fades out,
/// jumps and fades in). The speed-change metric on the output, centred on the
/// jump, outside the band the two speeds span; for fades of 1 frame (a
/// hard mute), 2, 5 (the one used) and 10 ms.
#[test]
fn cap_drop_fade_splatter() {
    let n = G as usize; // a 1 s window
    let beta = 38.0;
    let i0 = crate::window::bessel_i0(beta);
    let win: Vec<f64> = (0..n)
        .map(|i| {
            let u = 2.0 * i as f64 / (n - 1) as f64 - 1.0;
            crate::window::bessel_i0(beta * (1.0 - u * u).max(0.0).sqrt()) / i0
        })
        .collect();
    let norm = win.iter().sum::<f64>() / 2.0;
    let fft = RealFftPlanner::<f64>::new().plan_fft_forward(n);
    let mut rows = Vec::new();
    for fade_ms in [0.0, 2.0, 5.0, 10.0] {
        let mut row = format!("fade {fade_ms:>4} ms:");
        for f in [1_000.0, 10_000.0] {
            let mut worst = f64::NEG_INFINITY;
            for k in 0..4 {
                let mut run = Run::new(stage(G, 1, 1.5), false);
                run.vs.fade_len = ((fade_ms * 1e-3 * f64::from(G)).round() as usize).max(1);
                let w = 2.0 * PI * f / f64::from(G);
                let ph = k as f64 * PI / 4.0;
                let mut x = |j: u64| 0.5 * (w * j as f64 + ph).sin();
                let mut out = Vec::new();
                let drop_at = 30_000;
                while run.produced < drop_at + n {
                    if run.produced == drop_at {
                        run.vs.set_speed_limit(1.0);
                    }
                    run.pull(500, &mut x);
                    out.extend_from_slice(&run.out[0][..500]);
                }
                assert_eq!(run.vs.speed(), 1.0);
                // Centre the window on the jump.
                let jump = drop_at + run.vs.fade_frames();
                let w0 = jump - n / 2;
                let mut input = fft.make_input_vec();
                for (i, v) in input.iter_mut().enumerate() {
                    *v = out[w0 + i] * win[i];
                }
                let mut spec = fft.make_output_vec();
                fft.process(&mut input, &mut spec).unwrap();
                for (b, c) in spec.iter().enumerate() {
                    let fr = b as f64 * f64::from(G) / n as f64;
                    let outside = fr < f * 1.0 - 500.0 || fr > f * 1.5 + 500.0;
                    if outside && fr > 20.0 && fr < f64::from(G) / 2.0 {
                        worst = worst.max(db(c.norm() / norm));
                    }
                }
            }
            row += &format!(" {:>5.0} Hz {worst:7.1} dB", f);
            if fade_ms == 5.0 {
                assert!(worst <= -80.0, "the 5 ms fade at {f} Hz: {worst:.1} dB");
            }
        }
        rows.push(row);
    }
    // The one case the silence does not cover: lower caps late in it,
    // until its deadline stands one span late, then one more at the last
    // silent frame (its kink plays under the fade-in).
    let mut row = "a lower cap at the deadline:".to_owned();
    for f in [1_000.0, 10_000.0] {
        let mut worst = f64::NEG_INFINITY;
        for k in 0..4 {
            let mut run = Run::new(stage(G, 1, 1.5), false);
            let w = 2.0 * PI * f / f64::from(G);
            let ph = k as f64 * PI / 4.0;
            let mut x = |j: u64| 0.5 * (w * j as f64 + ph).sin();
            let mut out = Vec::new();
            let drop_at = 30_000;
            let mut jump = None;
            while run.produced < drop_at + n {
                if run.produced == drop_at {
                    run.vs.set_speed_limit(1.0);
                }
                let span = run.vs.mute_frames();
                if let Fade::Mute {
                    left,
                    jumped: true,
                    extend,
                } = run.vs.fade
                {
                    if left == 1 && extend < span && jump.is_none() {
                        run.vs.set_speed_limit(0.98);
                        jump = Some(run.produced);
                    } else if left == 2 && extend == span {
                        run.vs.set_speed_limit(0.99);
                    }
                }
                let call = if (drop_at..drop_at + 2000).contains(&run.produced) {
                    1
                } else {
                    500
                };
                run.pull(call, &mut x);
                out.extend_from_slice(&run.out[0][..call]);
            }
            assert_eq!(run.vs.speed(), 0.98);
            let w0 = jump.expect("the late jump") - n / 2;
            let mut input = fft.make_input_vec();
            for (i, v) in input.iter_mut().enumerate() {
                *v = out[w0 + i] * win[i];
            }
            let mut spec = fft.make_output_vec();
            fft.process(&mut input, &mut spec).unwrap();
            for (b, c) in spec.iter().enumerate() {
                let fr = b as f64 * f64::from(G) / n as f64;
                let outside = fr < f * 0.98 - 500.0 || fr > f * 1.5 + 500.0;
                if outside && fr > 20.0 && fr < f64::from(G) / 2.0 {
                    worst = worst.max(db(c.norm() / norm));
                }
            }
        }
        row += &format!(" {:>5.0} Hz {worst:7.1} dB", f);
        assert!(worst <= -75.0, "the late jump at {f} Hz: {worst:.1} dB");
    }
    rows.push(row);
    for r in rows {
        println!("{r}");
    }
}

/// The same speed command again (as a slider or a repeated control call
/// sends it) does not restart the ramp; a different one does.
#[test]
fn a_repeated_command_does_not_restart_its_ramp() {
    let mut vs = stage(G, 1, 1.0);
    let mut produced = 0;
    while produced < 30 * 1024 {
        vs.set_speed(1.5, 400.0).unwrap();
        pull_silence(&mut vs, 1024);
        produced += 1024;
        if produced >= 19 * 1024 {
            // 400 ms is 17 640 frames: done within the next call.
            assert_eq!(vs.speed(), 1.5, "after {produced} frames");
            assert!(!vs.ramping());
        }
    }
    vs.set_speed(1.25, 400.0).unwrap();
    assert!(vs.ramping());
    // A cap change that moves the target is still a new command.
    vs.set_speed(1.5, 400.0).unwrap();
    vs.set_speed_limit(1.2);
    assert_eq!(vs.target(), 1.2);
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

/// A cap restored during a drop's fade-out, before the silence: the curve
/// fits again, so the fade turns back from the gain it reached, with no
/// jump and no silence.
#[test]
fn a_cap_restored_during_the_fade_out_turns_it_back() {
    let mut vs = stage(G, 1, 1.5);
    pull_silence(&mut vs, 512);
    vs.set_speed_limit(1.0);
    pull_silence(&mut vs, 100);
    assert_eq!(vs.fade, Fade::Out { done: 100 });
    vs.set_speed_limit(f64::INFINITY);
    vs.plan(1).unwrap();
    // The same gain, rising.
    let done = vs.fade_frames() - 1 - 100;
    assert_eq!(vs.latched.unwrap().fade, Fade::In { done });
    assert!(
        (vs.fade_gain(Fade::In { done }) - vs.fade_gain(Fade::Out { done: 100 })).abs() < 1e-12
    );
    process_silence(&mut vs, 1);
    // The step had followed the curve towards 1.0 on the fade-out; it
    // brakes and returns to 1.5 smoothly.
    let low = vs.speed() * 0.999;
    assert!(vs.speed() < 1.5);
    for _ in 0..40 {
        vs.plan(256).unwrap();
        assert!(
            frame_fades(&vs)
                .iter()
                .all(|f| !matches!(f, Fade::Mute { .. }))
        );
        let (lo, hi) = planned_range(&vs);
        assert!(lo >= low && hi <= 1.5, "{lo}..{hi}");
        process_silence(&mut vs, 256);
    }
    assert!(!vs.ramping());
    assert_eq!(vs.speed(), 1.5);
}

/// A reset holds at the latest request under the current cap: a cap
/// raised during the fade-in (the reset used to go back to the cap drop's
/// jump target, 1.0), or during the silence (where the rise waits).
#[test]
fn a_reset_holds_at_the_latest_capped_request() {
    let drop_until = |vs: &mut Varispeed, done: fn(Fade) -> bool| {
        pull_silence(vs, 512);
        vs.set_speed_limit(1.0);
        while !done(vs.fade) {
            pull_silence(vs, 64);
        }
    };
    let mut vs = stage(G, 1, 1.5);
    drop_until(&mut vs, |f| matches!(f, Fade::In { .. }));
    vs.set_speed_limit(1.2);
    pull_silence(&mut vs, 1);
    assert_eq!(vs.target(), 1.2);
    vs.reset();
    assert_eq!((vs.speed(), vs.target()), (1.2, 1.2));
    assert!(!vs.ramping());

    let mut vs = stage(G, 1, 1.5);
    drop_until(&mut vs, |f| matches!(f, Fade::Mute { jumped: true, .. }));
    vs.set_speed_limit(1.3);
    pull_silence(&mut vs, 64);
    assert_eq!(vs.speed(), 1.0, "the rise waits for the fade-in");
    assert_eq!(vs.target(), 1.3);
    vs.reset();
    assert_eq!(vs.speed(), 1.3);

    // Under the cap, the request itself.
    let mut vs = stage(G, 1, 1.5);
    drop_until(&mut vs, |f| matches!(f, Fade::In { .. }));
    vs.set_speed(0.8, 0.0).unwrap();
    vs.reset();
    assert_eq!(vs.speed(), 0.8);
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

/// No ramp is shorter than `MIN_RAMP_S`: from rest (small and large
/// changes, with no ramp or a shorter one asked) and mid-ramp (towards the
/// target and against it), counted in core outputs from the command's
/// first output to the first one at the target.
#[test]
fn every_ramp_lasts_at_least_the_shortest_ramp() {
    for rate in [G, 96_000] {
        let floor = (envelope::MIN_RAMP_S * 2.0 * f64::from(rate)).floor() as usize;
        let ramp_len = |vs: &mut Varispeed, target: f64| -> usize {
            let mut steps = Vec::new();
            while steps.len() < 1_000_000 && (steps.last() != Some(&target) || vs.ramping()) {
                vs.plan(64).unwrap();
                steps.extend(planned_steps(vs));
                process_silence(vs, 64);
            }
            steps.iter().position(|&s| s == target).unwrap()
        };
        for (from, to, ramp_ms) in [
            (1.0, 1.001, 0.0),
            (1.0, 1.0001, 5.0),
            (1.0, 0.9999, 0.0),
            (0.5, 2.0, 0.0),
            (1.0, 1.0123, 19.9),
        ] {
            let mut vs = stage(rate, 1, from);
            vs.set_speed(to, ramp_ms).unwrap();
            let len = ramp_len(&mut vs, to);
            assert!(len >= floor, "{rate} Hz, {from} -> {to}: {len} < {floor}");
        }
        for sign in [1.0, -1.0] {
            let mut vs = stage(rate, 1, 1.0);
            vs.set_speed(1.5, 0.0).unwrap();
            for _ in 0..20 {
                pull_silence(&mut vs, 64);
            }
            assert!(vs.ramping());
            let to = Step::from_f64(vs.speed() * (1.0 + sign * 5e-4))
                .unwrap()
                .to_f64();
            vs.set_speed(to, 0.0).unwrap();
            let len = ramp_len(&mut vs, to);
            assert!(len >= floor, "{rate} Hz, mid-ramp {sign}: {len} < {floor}");
        }
    }
}

#[test]
fn the_static_cost_matches_the_plan() {
    // 1 248 / 1 772 multiply-adds per frame at s = 1 / 2 up to
    // 192 kHz, 704 / 956 above.
    let per_frame = |rate: u32, s: f64| macs_per_second(rate, s).unwrap() / f64::from(rate);
    assert_eq!(per_frame(44_100, 1.0), 1248.0);
    assert_eq!(per_frame(48_000, 2.0), 1772.0);
    assert_eq!(per_frame(768_000, 1.0), 704.0);
    assert_eq!(per_frame(768_000, 2.0), 956.0);
    assert!(worst_case_macs_per_second(768_000).unwrap() < 740e6);
    let st = Varispeed::new(VarispeedSpec {
        sample_rate: 96_000,
        channels: 1,
        max_frames: 64,
        speed: 1.0,
    })
    .unwrap();
    assert_eq!(per_frame(96_000, 1.5), st.macs_per_frame(1.5));
    assert!(macs_per_second(0, 1.0).is_err());
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

/// Random commands (speeds, ramps, caps), random call sizes, and for the
/// follower random ε targets: `(stage, follower)` driven alike, call by
/// call, `check` after each plan.
fn follower_pair(seed: u64, eps: bool, mut check: impl FnMut(&Varispeed, &Varispeed, (f64, f64))) {
    let mut seed = seed;
    for case in 0..12 {
        let rate = [44_100u32, 48_000, 96_000][case % 3];
        let s0 = 0.5 + 1.5 * unit(&mut seed);
        let mut p = stage(rate, 1, s0);
        let mut f = stage(rate, 1, s0);
        f.set_follower(true);
        for i in 0..150 {
            if unit(&mut seed) < 0.2 {
                let s = 0.5 + 1.49 * unit(&mut seed);
                let r = [0.0, 20.0, 200.0, 3000.0][(unit(&mut seed) * 4.0) as usize];
                p.set_speed(s, r).unwrap();
                f.set_speed(s, r).unwrap();
            }
            if unit(&mut seed) < 0.06 {
                let cap = if unit(&mut seed) < 0.3 {
                    f64::INFINITY
                } else {
                    0.6 + 1.39 * unit(&mut seed)
                };
                p.set_speed_limit(cap);
                f.set_speed_limit(cap);
            }
            let to = if eps {
                (
                    2e-3 * (unit(&mut seed) - 0.5),
                    1e-4 * (unit(&mut seed) - 0.5),
                )
            } else {
                (0.0, 0.0)
            };
            f.set_epsilon_target(to.0, to.1);
            let n = if i % 10 == 9 {
                MAX_FRAMES
            } else {
                [1, 37, 256, 1024, 2048][(unit(&mut seed) * 5.0) as usize]
            };
            p.plan(n).unwrap();
            f.plan(n).unwrap();
            assert!(!f.latched.unwrap().fallback, "case {case} call {i}");
            check(&p, &f, to);
            process_silence(&mut p, n);
            process_silence(&mut f, n);
            assert_eq!(f.envelope_step(), p.step(), "case {case} call {i}");
            if eps {
                assert_eq!(f.epsilon().0, to.0, "case {case} call {i}");
            }
        }
        assert_eq!(f.fallbacks(), 0);
    }
}

/// Follower mode with ε held at zero runs as the stage: the same
/// positions at every core output, call for call, through ramps, caps and
/// their fades.
#[test]
fn follower_mode_at_zero_epsilon_runs_as_the_stage() {
    follower_pair(5, false, |p, f, _| {
        assert_eq!(planned_trajectory(p), planned_trajectory(f));
    });
}

/// Follower mode applies `1 + ε` to every step: at each core output the
/// step is the stage's times `1 + ε(t)`, ε running from the last call's
/// end to the target over the call. The product of the envelope's cubic
/// and ε's is a sextic, and each piece is one cubic with the product's
/// values and slopes at its ends, so they differ by at most the product
/// of their relative changes over the call (with ε held, as the loop holds
/// it through a ramp, exactly). The targets here are far steeper than any
/// loop's.
#[test]
fn follower_mode_applies_epsilon_to_every_step() {
    let mut worst: f64 = 0.0;
    follower_pair(9, true, |p, f, to| {
        let sp = planned_steps(p);
        let sf = planned_steps(f);
        assert_eq!(sp.len(), sf.len());
        let from = f.epsilon();
        let r = 2.0 * f64::from(p.sample_rate());
        let h = sp.len().max(1) as f64 / r;
        // A jump (a cap drop's silence) restarts ε's slope: skip calls
        // with one, the pieces around it are muted.
        let latch = f.latched.unwrap();
        let jumps = latch.parts[..latch.n_parts]
            .iter()
            .any(|x| matches!(x.seg, Segment::Jump(_)));
        let (vlo, vhi) = sp
            .iter()
            .fold((f64::INFINITY, 0.0f64), |(x, y), &v| (x.min(v), y.max(v)));
        let dv = (vhi - vlo) / vlo;
        let de = (to.0 - from.0).abs() + h * (to.1.abs() + from.1.abs());
        if !jumps {
            for (k, (&a, &b)) in sp.iter().zip(&sf).enumerate() {
                let e = crate::drift::hermite(from, to, h, k as f64 / r).0;
                let want = a * (1.0 + e);
                let err = ((b - want) / want).abs();
                assert!(
                    err <= 2.0 * dv * de + 1e-9,
                    "output {k}: {err:e}, dv {dv:e} de {de:e}"
                );
                worst = worst.max(err);
            }
        }
        let (lo, hi) = sf
            .iter()
            .fold((f64::INFINITY, 0.0f64), |(x, y), &v| (x.min(v), y.max(v)));
        assert!(lo >= Step::MIN.to_f64() && hi <= Step::MAX.to_f64());
    });
    println!("worst relative step error {worst:.2e}");
}

/// A follower's reset keeps ε (the rate state): the core restarts at the
/// target times `1 + ε`.
#[test]
fn a_follower_reset_keeps_epsilon() {
    let mut f = stage(G, 1, 1.0);
    f.set_follower(true);
    f.set_epsilon_target(1e-3, 0.0);
    pull_silence(&mut f, 1024);
    assert_eq!(f.epsilon().0, 1e-3);
    f.set_speed(1.5, 0.0).unwrap();
    f.reset();
    assert_eq!(f.envelope_step().to_f64(), 1.5);
    assert!((f.speed() - 1.5 * 1.001).abs() < 1e-12, "{}", f.speed());
    assert_eq!(f.epsilon(), (1e-3, 0.0));
    let (lo, hi) = pull_silence(&mut f, 512);
    assert!((lo - 1.5015).abs() < 1e-12 && (hi - 1.5015).abs() < 1e-12);
    // Off again: the next reset is at the target.
    f.set_follower(false);
    f.reset();
    assert_eq!(f.speed(), 1.5);
}

/// `position_after(n)` is where the core stands `n` frames on, cut into
/// other calls: exactly on holds, within a frame through ramps and cap
/// drops (0.44 at worst here, with 20 ms ramps across the whole range):
/// the envelope's pieces follow the calls' cuts.
#[test]
fn position_after_is_where_the_core_gets_to() {
    let mut seed = 0x005e_ed0f_u64;
    let mut worst = 0.0f64;
    for case in 0..24 {
        let rate = [44_100u32, 48_000, 192_000][case % 3];
        let mut run = Run::new(stage(rate, 1, 1.0), false);
        let mut x = |j: u64| (j as f64 * 0.01).sin();
        for _ in 0..40 {
            if unit(&mut seed) < 0.3 {
                let s = 0.5 + 1.49 * unit(&mut seed);
                let r = [0.0, 20.0, 200.0, 3000.0][(unit(&mut seed) * 4.0) as usize];
                run.vs.set_speed(s, r).unwrap();
            }
            if unit(&mut seed) < 0.1 {
                run.vs.set_speed_limit(0.6 + 1.39 * unit(&mut seed));
            }
            let k = 1 + (unit(&mut seed) * 6000.0) as usize;
            let was_ramping = run.vs.ramping();
            let want = run.vs.position_after(k).unwrap();
            let mut left = k;
            while left > 0 {
                let n = (1 + (unit(&mut seed) * 1500.0) as usize).min(left);
                run.pull(n, &mut x);
                left -= n;
            }
            let (ramping, fading) = (run.vs.ramping(), run.vs.fading());
            let got = run.vs.core().accumulator().position();
            // ×2-grid frames to source frames.
            let err = (got as i128 - want as i128).unsigned_abs() as f64 / ONE_F / 2.0;
            worst = worst.max(err);
            assert!(err < 1.0, "case {case}: {err} frames off after {k}");
            if !ramping && !fading && !was_ramping {
                assert_eq!(got, want, "case {case}: a hold");
            }
        }
    }
    eprintln!("position_after: worst {worst:.2e} source frames");
}
