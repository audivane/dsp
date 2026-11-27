use super::*;
use std::f64::consts::PI;

#[test]
fn sideband_analyser_reads_a_known_modulation_and_ignores_wander() {
    use sim::sidebands;
    let fs = 1000.0;
    let n = 120_000;
    let (tone, rate) = (10_000.0, 48_000.0);
    // A 1e-6 frame sinusoid at 3.3 Hz on 2 frames of wander at 2 mHz and
    // 0.5 frames at 50 mHz.
    let a = 1e-6;
    let err: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / fs;
            2.0 * (2.0 * PI * 0.002 * t).sin()
                + 0.5 * (2.0 * PI * 0.05 * t + 1.0).sin()
                + a * (2.0 * PI * 3.3 * t).sin()
        })
        .collect();
    let beta = 2.0 * PI * tone * a / rate;
    let want = 20.0 * (beta / 2.0).log10();
    let (db, hz) = sidebands(&err, fs, tone, rate, (1.0, 100.0));
    assert!((db - want).abs() < 0.2, "{db} dBc at {hz} Hz, want {want}");
    assert!((hz - 3.3).abs() < 0.1, "{hz}");
    // Without the line, the wander leaves the band clean.
    let quiet: Vec<f64> = err
        .iter()
        .enumerate()
        .map(|(i, e)| e - a * (2.0 * PI * 3.3 * i as f64 / fs).sin())
        .collect();
    let (db, _) = sidebands(&quiet, fs, tone, rate, (1.0, 100.0));
    assert!(db < want - 40.0, "{db}");
}

/// The continuous loop model integrated by fine RK4 steps, for checking
/// the exact propagation.
fn rk4(lp: &DriftLoop, u: f64, h: f64) -> (Vec<f64>, f64) {
    let m = lp.cfg.poles;
    let (a, ki) = (lp.a, lp.ki);
    let f = |x: &[f64]| -> Vec<f64> {
        let mut d = vec![0.0; m + 1];
        for i in 0..m {
            let prev = if i == 0 { u } else { x[i - 1] };
            d[i] = a * (prev - x[i]);
        }
        d[m] = ki * x[m - 1];
        d
    };
    let mut x: Vec<f64> = lp.x[..m].to_vec();
    x.push(lp.integ);
    let steps = 20_000;
    let dt = h / steps as f64;
    for _ in 0..steps {
        let k1 = f(&x);
        let x2: Vec<f64> = x.iter().zip(&k1).map(|(x, k)| x + 0.5 * dt * k).collect();
        let k2 = f(&x2);
        let x3: Vec<f64> = x.iter().zip(&k2).map(|(x, k)| x + 0.5 * dt * k).collect();
        let k3 = f(&x3);
        let x4: Vec<f64> = x.iter().zip(&k3).map(|(x, k)| x + dt * k).collect();
        let k4 = f(&x4);
        for i in 0..=m {
            x[i] += dt / 6.0 * (k1[i] + 2.0 * k2[i] + 2.0 * k3[i] + k4[i]);
        }
    }
    let integ = x.pop().unwrap();
    (x, integ)
}

#[test]
fn propagation_is_exact() {
    for poles in [1usize, 2, 5, 12, MAX_POLES] {
        let cfg = LoopConfig {
            poles,
            ..LoopConfig::default()
        };
        for (bw, h) in [(0.3, 0.021), (0.02, 0.186), (0.3, 0.186), (0.05, 0.005)] {
            let mut lp = DriftLoop::new(cfg, 48_000.0, 0.0).unwrap();
            lp.set_gains(bw);
            for (i, x) in lp.x[..poles].iter_mut().enumerate() {
                *x = 3.0 * ((i * 7 + 1) as f64).sin();
            }
            lp.integ = 1e-5;
            let (want_x, want_i) = rk4(&lp, 1.7, h);
            lp.propagate(1.7, h);
            for (got, want) in lp.x[..poles].iter().zip(&want_x) {
                assert!((got - want).abs() < 1e-9, "{poles} poles: {got} vs {want}");
            }
            assert!(
                (lp.integ - want_i).abs() < 1e-14,
                "{} vs {want_i}",
                lp.integ
            );
        }
    }
}

#[test]
fn lower_gamma_matches_its_sum() {
    for n in 1..=MAX_POLES {
        for x in [1e-6, 0.01, 0.3, 1.0, 5.0, 17.0, 40.0] {
            // P(n, x) = 1 − e^{−x} Σ_{k<n} x^k/k!, and the complement as the
            // tail series, both in long sums.
            let mut head = 0.0;
            let mut term = 1.0;
            for k in 0..n {
                head += term;
                term *= x / (k + 1) as f64;
            }
            let mut tail = 0.0;
            let mut k = n;
            let mut t = term;
            while k < n + 400 {
                tail += t;
                k += 1;
                t *= x / k as f64;
            }
            let want = (-x).exp() * tail;
            let alt = 1.0 - (-x).exp() * head;
            let got = lower_gamma_p(n, x);
            assert!(
                (got - want).abs() <= 1e-15 * want.max(1e-300) + 1e-16,
                "P({n}, {x}) = {got}, want {want} (complement {alt})"
            );
        }
    }
    assert_eq!(lower_gamma_p(3, 0.0), 0.0);
}

#[test]
fn position_map_interpolates_and_keeps_jumps_out_of_spans() {
    let mut map = PositionMap::new();
    assert_eq!(map.source_at(0), None);
    // Period 0: frames 0..=99 carry source 1000..=1099.5 (step 1.005).
    map.push(0, 1000 * FRAME);
    map.push(99, 1000 * FRAME + (99.5 * FRAME_F) as i128);
    // Period 1 starts 10 frames further on (a skip).
    map.push(100, 1110 * FRAME);
    map.push(199, 1209 * FRAME);
    assert_eq!(map.source_at(0), Some(1000 * FRAME));
    let mid = map.source_at(50).unwrap() as f64 / FRAME_F;
    assert!((mid - (1000.0 + 99.5 * 50.0 / 99.0)).abs() < 1e-12, "{mid}");
    assert_eq!(map.source_at(100), Some(1110 * FRAME));
    assert_eq!(map.source_at(150), Some(1160 * FRAME));
    assert_eq!(map.source_at(199), Some(1209 * FRAME));
    assert_eq!(map.source_at(200), None);
    // The oldest entries fall out.
    for k in 2..(MAP_LEN as u64 / 2 + 2) {
        map.push(100 * k, 0);
        map.push(100 * k + 99, 0);
    }
    assert_eq!(map.source_at(50), None);
    assert!(map.source_at(250).is_some());
    // An entry that does not move forward restarts the map.
    map.push(10, 5 * FRAME);
    assert_eq!(map.len(), 1);
    assert_eq!(map.source_at(10), Some(5 * FRAME));
}

#[test]
fn phase_detector_moves_the_master_to_the_follower_instant() {
    let mut mm = PositionMap::new();
    mm.push(0, 0);
    mm.push(1023, 1023 * FRAME);
    mm.push(1024, 1024 * FRAME);
    mm.push(2047, 2047 * FRAME);
    let mut fm = PositionMap::new();
    fm.push(0, 500 * FRAME);
    fm.push(1023, 1523 * FRAME);
    let mut det = PhaseDetector::new(48_000.0);
    let f = Reading {
        written: 1024,
        delay: 1000,
        time_ns: 1_010_000_000,
    };
    assert_eq!(det.follower(f, &fm), None, "no master yet");
    // The master is at frame 1048 at t = 1 s, so at 1.01 s at 1528; the
    // follower plays frame 24, source 524: the follower is 1004 behind.
    let m = Reading {
        written: 2048,
        delay: 1000,
        time_ns: 1_000_000_000,
    };
    assert!(det.master(m, &mm));
    let e = det.follower(f, &fm).unwrap();
    assert!((e - 1004.0).abs() < 1e-9, "{e}");
    det.set_offset(4.0);
    assert!((det.follower(f, &fm).unwrap() - 1000.0).abs() < 1e-9);
    // An xrun's delay, or a reading too old, is not used.
    let bad = Reading { delay: -5, ..f };
    assert_eq!(det.follower(bad, &fm), None);
    let late = Reading {
        time_ns: 3_000_000_000,
        ..f
    };
    assert_eq!(det.follower(late, &fm), None);
    assert!(!det.master(Reading { delay: 5000, ..m }, &mm));
    // A position published by the master's thread does the same.
    det.set_offset(0.0);
    det.master_position(1048 * FRAME, 1_000_000_000);
    assert!((det.follower(f, &fm).unwrap() - 1004.0).abs() < 1e-9);
}

#[test]
fn hermite_matches_its_ends() {
    let (from, to, h) = ((1e-4, 2e-6), (1.2e-4, -1e-6), 0.02);
    let (v0, d0) = hermite(from, to, h, 0.0);
    let (v1, d1) = hermite(from, to, h, h);
    assert!((v0 - from.0).abs() < 1e-18 && (d0 - from.1).abs() < 1e-15);
    assert!((v1 - to.0).abs() < 1e-18 && (d1 - to.1).abs() < 1e-15);
    // The slope is the value's derivative.
    let t = 0.37 * h;
    let dt = 1e-7;
    let num = (hermite(from, to, h, t + dt).0 - hermite(from, to, h, t - dt).0) / (2.0 * dt);
    assert!((num - hermite(from, to, h, t).1).abs() < 1e-9, "{num}");
}

/// Pieces from [`EpsilonTrack`] through the real accumulator: each call
/// ends on `s (1 + ε)` exactly, slopes join, and the input consumed is the
/// integral of the step.
#[test]
fn epsilon_pieces_drive_the_accumulator() {
    use crate::adaptive::Accumulator;
    let rate = 96_000.0; // core outputs per second
    let n = 2048usize;
    let h = n as f64 / rate;
    let s = 1.25;
    let mut acc = Accumulator::new(Step::from_f64(s).unwrap());
    let mut track = EpsilonTrack::new(rate);
    let mut ideal = 0.0;
    let mut prev_slope1 = 0.0;
    let mut state = (0.0, 0.0);
    for k in 0..200 {
        let t = (k + 1) as f64 * h;
        let to = (
            8e-5 * (2.0 * PI * 0.3 * t).sin() + 5e-5,
            8e-5 * 2.0 * PI * 0.3 * (2.0 * PI * 0.3 * t).cos(),
        );
        let seg = track
            .piece(Segment::Hold, Step::from_f64(s).unwrap(), to)
            .unwrap();
        let Segment::Piece {
            slope0,
            end,
            slope1,
        } = seg
        else {
            panic!("{seg:?}");
        };
        assert_eq!(slope0, prev_slope1, "slopes join");
        assert_eq!(end, Step::from_f64(s * (1.0 + to.0)).unwrap());
        prev_slope1 = slope1;
        // The ideal input: ∫ s (1 + ε) over the call's outputs (Hermite
        // integral in output units).
        let (e0, d0) = state;
        ideal += s * n as f64 + s * (h * (e0 + to.0) / 2.0 + h * h * (d0 - to.1) / 12.0) * rate;
        state = to;
        let need = acc.plan(n, seg).unwrap();
        acc.commit(need);
        assert_eq!(acc.step(), end);
    }
    // The accumulator sums the step at each output (a left sum): the
    // integral plus half the first step less half the last.
    let ideal = ideal + s * (0.0 - state.0) / 2.0;
    let got = acc.position() as f64 / FRAME_F;
    assert!((got - ideal).abs() < 1e-7, "{got} vs {ideal}");
    // A master ramp combines with ε by the product rule.
    let seg = Segment::Piece {
        slope0: 0.0,
        end: Step::from_f64(1.5).unwrap(),
        slope1: 1e-5,
    };
    let mut tr = EpsilonTrack::new(rate);
    tr.reset(1e-4);
    let out = tr.piece(seg, Step::ONE, (2e-4, 9.6)).unwrap();
    let Segment::Piece {
        slope0,
        end,
        slope1,
    } = out
    else {
        panic!()
    };
    assert_eq!(slope0, 0.0);
    assert_eq!(end, Step::from_f64(1.5 * (1.0 + 2e-4)).unwrap());
    assert!((slope1 - (1e-5 * (1.0 + 2e-4) + 1.5 * 9.6 / rate)).abs() < 1e-18);
    // Out of the core's range: refused, nothing kept.
    let mut tr = EpsilonTrack::new(rate);
    assert_eq!(tr.piece(Segment::Hold, Step::MAX, (1e-3, 0.0)), None);
    assert_eq!(tr.state(), (0.0, 0.0));
}

/// A noise-free closed loop: the plant integrates `g (δ + ε)`; the loop
/// takes ε to −δ and the error to zero, through the gear.
fn closed_loop(cfg: LoopConfig, delta: f64, e0: f64, secs: f64) -> (DriftLoop, f64, usize) {
    let g = 48_000.0;
    let h = 1024.0 / g;
    let mut lp = DriftLoop::new(cfg, g, 0.0).unwrap();
    let mut e = e0;
    let mut from = (0.0, 0.0);
    let mut skips = 0;
    for _ in 0..(secs / h) as usize {
        let c = lp.update(e, h);
        if c.skip != 0 {
            skips += 1;
            e -= c.skip as f64;
        }
        let to = (c.eps, c.eps_slope);
        // de/dt = −g (δ + ε), ε the Hermite: its exact integral.
        let int = h * (from.0 + to.0) / 2.0 + h * h * (from.1 - to.1) / 12.0;
        e -= g * (delta * h + int);
        from = to;
    }
    (lp, e, skips)
}

#[test]
fn loop_locks_to_the_clock_offset() {
    for delta in [-1e-4, -3e-5, 0.0, 2e-5, 1e-4] {
        // The first readings align by a skip (3 frames), the loop does the
        // rest.
        let (lp, e, skips) = closed_loop(LoopConfig::default(), delta, 3.0, 400.0);
        assert_eq!(skips, 1);
        assert!(lp.locked(), "δ {delta}");
        assert!((lp.eps() + delta).abs() < 1e-9, "δ {delta}: ε {}", lp.eps());
        assert!(e.abs() < 1e-3, "δ {delta}: error {e}");
        assert!((lp.ppm() - delta * 1e6).abs() < 1e-3);
        assert_eq!(lp.bandwidth(), LoopConfig::default().track_hz);
    }
}

#[test]
fn loop_skips_large_errors_and_does_not_wind_up() {
    // 300 frames behind: one skip after `skip_readings` readings, then
    // the rest is pulled in.
    let cfg = LoopConfig::default();
    let (lp, e, skips) = closed_loop(cfg, 5e-5, 300.0, 300.0);
    assert_eq!(skips, 1);
    assert!(e.abs() < 1e-2, "{e}");
    assert!(lp.locked());
    // With skips out of reach and a huge offset, ε clamps; the integral
    // does not wind up, so the loop recovers once the error is gone.
    let cfg2 = LoopConfig {
        skip_s: 1e9,
        acquire_skip_s: 1e9,
        ..cfg
    };
    let mut lp = DriftLoop::new(cfg2, 48_000.0, 0.0).unwrap();
    for _ in 0..200 {
        let c = lp.update(5000.0, 0.02);
        assert!(c.eps <= EPS_MAX && c.eps >= -EPS_MAX);
    }
    assert!(lp.integ.abs() <= EPS_MAX, "{}", lp.integ);
    // Not a number, or no time, holds.
    let before = lp.eps().clamp(-EPS_MAX, EPS_MAX);
    let c = lp.update(f64::NAN, 0.02);
    assert_eq!((c.eps, c.eps_slope, c.skip), (before, 0.0, 0));
    let c = lp.update(1.0, 0.0);
    assert_eq!(c.skip, 0);
}

#[test]
fn loop_config_is_checked() {
    let ok = LoopConfig::default();
    assert!(DriftLoop::new(ok, 48_000.0, 0.05).is_ok());
    let bad = [
        LoopConfig {
            track_hz: 1.0,
            ..ok
        },
        LoopConfig { poles: 0, ..ok },
        LoopConfig {
            poles: MAX_POLES + 1,
            ..ok
        },
        LoopConfig { eps_max: 0.1, ..ok },
        LoopConfig {
            skip_readings: 0,
            ..ok
        },
        LoopConfig {
            zero_ratio: f64::NAN,
            ..ok
        },
        LoopConfig {
            unlock_error_s: 0.1 / 48_000.0,
            ..ok
        },
    ];
    for cfg in bad {
        assert!(DriftLoop::new(cfg, 48_000.0, 0.05).is_err(), "{cfg:?}");
    }
    assert!(DriftLoop::new(ok, 0.0, 0.05).is_err());
    assert!(DriftLoop::new(ok, 48_000.0, f64::INFINITY).is_err());
    // The loop delay caps the acquisition bandwidth.
    let lp = DriftLoop::new(ok, 48_000.0, 0.5).unwrap();
    assert!((lp.bandwidth() - 0.2 / (2.0 * PI * 0.5)).abs() < 1e-12);
}

#[test]
fn rate_estimator_sees_the_raw_clock_offset() {
    // The loop holds ε at −δ; the raw phase still has slope −g δ.
    let g = 48_000.0;
    let delta = 3.5e-5;
    let h = 0.02;
    let mut est = RateEstimator::new();
    assert_eq!(est.ppm(g), None);
    let mut e = 0.0;
    let eps = -delta * 0.6; // a loop not yet converged
    for _ in 0..400 {
        est.push(e, g);
        est.advance(h, (eps, 0.0), (eps, 0.0));
        e -= g * (delta + eps) * h;
    }
    let (ppm, se) = est.ppm(g).unwrap();
    assert!((ppm - delta * 1e6).abs() < 1e-6, "{ppm}");
    assert!(se < 1e-6, "{se}");
    est.skipped(10.0);
    let (ppm2, _) = est.ppm(g).unwrap();
    assert!((ppm2 - ppm).abs() < 1e-9);
}

/// One update of a loop on a plant with reading noise (uniform within
/// ±`noise` frames) and an outlier of `outlier` frames when set: the true
/// error `e` after the update's skip, and the loop's lock.
struct Plant {
    g: f64,
    h: f64,
    delta: f64,
    e: f64,
    from: (f64, f64),
    rng: u64,
    noise: f64,
    skips: usize,
}

impl Plant {
    fn new(delta: f64, e: f64, noise: f64) -> Self {
        Self {
            g: 48_000.0,
            h: 1024.0 / 48_000.0,
            delta,
            e,
            from: (0.0, 0.0),
            rng: 0x2545_f491_4f6c_dd1d,
            noise,
            skips: 0,
        }
    }

    fn unit(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 11) as f64 / (1u64 << 53) as f64
    }

    fn step(&mut self, lp: &mut DriftLoop, outlier: f64) -> (f64, bool) {
        let reading = self.e + self.noise * (2.0 * self.unit() - 1.0) + outlier;
        let c = lp.update(reading, self.h);
        if c.skip != 0 {
            self.skips += 1;
            self.e -= c.skip as f64;
        }
        let to = (c.eps, c.eps_slope);
        let h = self.h;
        let int = h * (self.from.0 + to.0) / 2.0 + h * h * (self.from.1 - to.1) / 12.0;
        let at = self.e;
        self.e -= self.g * (self.delta * h + int);
        self.from = to;
        (at, c.locked)
    }
}

/// The placement floor at 48 kHz, frames.
const FLOOR: f64 = 0.05e-3 * 48_000.0;

/// A locked loop whose phase jumps (the master's stream restarted after a
/// pause): placed again within a fraction of a second, within the floor
/// at its first locked update and from there on, its rate state and
/// bandwidth kept, one skip.
#[test]
fn a_locked_loop_is_placed_again_after_a_phase_jump() {
    let delta = 3e-5;
    let (mut lp, e, _) = closed_loop(LoopConfig::default(), delta, 3.0, 400.0);
    assert!(lp.locked() && !lp.placed());
    let (bw, ppm) = (lp.bandwidth(), lp.ppm());
    for jump in [240_000.37, -31_999.8, 0.4] {
        let mut p = Plant::new(delta, e + jump, 3.0);
        p.from = (lp.eps(), 0.0);
        assert!(lp.realign());
        assert!(!lp.locked() && lp.trusted());
        let mut first = None;
        for i in 0..(10.0 / p.h) as usize {
            let (at, locked) = p.step(&mut lp, 0.0);
            if locked {
                first.get_or_insert((i as f64 * p.h, at));
                assert!(at.abs() < FLOOR, "jump {jump}: {at} frames at update {i}");
            } else {
                assert!(first.is_none(), "jump {jump}: muted again at update {i}");
            }
        }
        let (t, at) = first.expect("never placed");
        println!("jump {jump}: heard after {t:.3} s at {at:.2} frames");
        assert!(t < 0.6, "jump {jump}: {t} s");
        // (A jump under a frame may round to a frame on noisy readings.)
        assert!(p.skips <= 1, "jump {jump}: {} skips", p.skips);
        assert!(lp.bandwidth() <= bw, "{}", lp.bandwidth());
        // (Correcting a placement's residual moves the rate state a little.)
        assert!((lp.ppm() - ppm).abs() < 1.5, "{} vs {ppm}", lp.ppm());
    }
}

/// A fresh loop seeded with a remembered rate (a reopen of the same
/// device) places the follower from its first readings, without an
/// acquisition: heard within a second, within the floor; the same loop
/// unseeded acquires as before (seconds, muted).
#[test]
fn a_seeded_loop_places_without_acquiring() {
    let delta = -4.2e-5;
    let cfg = LoopConfig::default();
    let mut lp = DriftLoop::new(cfg, 48_000.0, 0.03).unwrap();
    lp.seed(-delta);
    assert!(lp.trusted() && !lp.aligned());
    assert!(lp.bandwidth() <= cfg.lock_hz);
    let mut p = Plant::new(delta, 5000.6, 3.0);
    p.from = (-delta, 0.0);
    let mut first = None;
    for i in 0..(20.0 / p.h) as usize {
        let (at, locked) = p.step(&mut lp, 0.0);
        if locked {
            first.get_or_insert((i as f64 * p.h, at));
            assert!(at.abs() < FLOOR, "{at} frames at update {i}");
        }
        assert!(lp.bandwidth() <= cfg.lock_hz);
    }
    let (t, at) = first.expect("never placed");
    println!("seeded: heard after {t:.3} s at {at:.2} frames");
    assert!(t < 0.6, "{t}");
    assert_eq!(p.skips, 1);
    assert!((lp.ppm() - delta * 1e6).abs() < 1.0, "{}", lp.ppm());
    // Unseeded: the acquisition, as before.
    let mut lp = DriftLoop::new(cfg, 48_000.0, 0.03).unwrap();
    let mut p = Plant::new(delta, 5000.6, 3.0);
    let mut lock_at = None;
    for i in 0..(60.0 / p.h) as usize {
        if p.step(&mut lp, 0.0).1 {
            lock_at.get_or_insert(i as f64 * p.h);
        }
    }
    let t = lock_at.expect("cold start never locked");
    println!("cold: locked after {t:.1} s");
    assert!(t > 5.0 && t < 30.0, "{t}");
    assert!(!lp.placed() && lp.trusted());
}

/// A seeded rate that is wrong (the remembered ε 60 ppm off): the follower
/// is placed, drifts, and is muted once it leaves the floor (not far
/// beyond it); the loop pulls the rate in and it is heard again, in place.
#[test]
fn a_wrong_seed_mutes_once_out_of_the_floor() {
    let delta = 2e-5;
    let mut lp = DriftLoop::new(LoopConfig::default(), 48_000.0, 0.03).unwrap();
    lp.seed(-delta + 60e-6);
    let mut p = Plant::new(delta, -700.2, 2.0);
    p.from = (lp.eps(), 0.0);
    let mut worst: f64 = 0.0;
    let mut mutes = 0;
    let mut was = false;
    for _ in 0..(120.0 / p.h) as usize {
        let (at, locked) = p.step(&mut lp, 0.0);
        if locked {
            worst = worst.max(at.abs());
        }
        if was && !locked {
            mutes += 1;
        }
        was = locked;
    }
    println!("wrong seed: worst heard {worst:.2} frames, {mutes} mutes");
    assert!(worst < 2.0 * FLOOR, "{worst}");
    assert!(lp.locked(), "never heard again");
    assert!((lp.ppm() - delta * 1e6).abs() < 1.0, "{}", lp.ppm());
}

/// The outlier gate still holds a locked loop (trusted: a phase jump would
/// place it again): single outliers are replaced, nothing skips or mutes.
#[test]
fn the_gate_holds_a_locked_loop_through_outliers() {
    let delta = 3e-5;
    let (mut lp, e, _) = closed_loop(LoopConfig::default(), delta, 3.0, 400.0);
    let mut p = Plant::new(delta, e, 1.0);
    p.from = (lp.eps(), 0.0);
    let gated = lp.gated();
    for i in 0..3000 {
        let outlier = if i % 50 == 7 { 300.0 } else { 0.0 };
        let (at, locked) = p.step(&mut lp, outlier);
        assert!(locked, "muted at update {i}");
        assert!(at.abs() < 1.0, "{at} at update {i}");
    }
    assert_eq!(p.skips, 0);
    assert!(lp.aligned());
    assert_eq!(lp.gated() - gated, 60);
}

/// A device read on 1 ms packets at a phase sweeping against its 10 ms
/// wakeups (a reading bias of 0 to 48 frames, round every 5.5 s): the
/// tracker reads it as coarse, takes nothing out (and claims no RMS) until
/// its phase has wrapped a whole sweep apart, then leaves the readings at
/// their long-run mean (half a packet behind) to a fraction of a frame;
/// over a new stream it keeps the line's slope and corrects again from
/// the next wrap.
#[test]
fn a_bias_tracker_takes_a_sweeping_grid_out() {
    let rate = 48_000.0;
    let clock = 1.0 + 40e-6;
    let packet = 48.0;
    let read = |t: f64, origin: f64| {
        // The pointer at its last packet; the wakeups slide a seventh of
        // a packet a second against the grid, and the clock's offset too.
        let true_pos = (t * clock * rate) + origin;
        let phase = ((t / 7.0).fract() * packet + true_pos).rem_euclid(packet);
        (true_pos - phase, true_pos)
    };
    let mut b = BiasTracker::new();
    let mut worst = 0.0f64;
    let mut t = 0.0;
    let mut first = None;
    while t < 60.0 {
        let (pos, true_pos) = read(t, 0.0);
        match b.push(pos, t, rate) {
            Some(dev) => {
                first.get_or_insert(t);
                // The reading less its deviation: the long-run mean of the
                // bias is what stays (-24 frames).
                let corrected = pos - dev - true_pos;
                worst = worst.max((corrected + 24.0).abs());
            }
            None => assert!(
                first.is_none() || b.coarse_grid_s().is_none(),
                "lost at {t}"
            ),
        }
        if first.is_none() {
            assert!(!b.rms_known(), "an RMS claimed at {t}");
        }
        t += 0.010;
    }
    assert!(b.coarse_grid_s().is_some());
    let first = first.unwrap();
    println!("first corrected at {first:.2} s, at most {worst:.3} frames from the mean");
    assert!(first < 14.0, "{first}");
    assert!(worst < 0.5, "{worst}");
    // A new stream: positions from elsewhere, the same clock.
    b.rebase();
    assert!(!b.established());
    let mut again = None;
    let mut worst = 0.0f64;
    let t0 = t;
    while t < t0 + 20.0 {
        let (pos, true_pos) = read(t, 1.0e6);
        if let Some(dev) = b.push(pos, t, rate) {
            again.get_or_insert(t - t0);
            worst = worst.max((pos - dev - true_pos + 24.0).abs());
        }
        t += 0.010;
    }
    let again = again.unwrap();
    println!("after a rebase corrected after {again:.2} s, at most {worst:.3} frames");
    assert!(again < 8.0, "{again}");
    assert!(worst < 0.5, "{worst}");
}
