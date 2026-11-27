use super::*;

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
