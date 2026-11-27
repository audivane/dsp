//! Times the DPSS window at long lengths against a bisection reference.
//!
//! `cargo run --release -p audivane-dsp-core --example dpss_timing`
//! Optional args: `<taps> <nw>` for one row.
use audivane_dsp_core::window::dpss;
use std::time::Instant;

fn row(n: usize, nw: f64) {
    let t = Instant::now();
    let reference = dpss_old(n, nw);
    let before = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let w = dpss(n, nw);
    let after = t.elapsed().as_secs_f64();
    let diff = w
        .iter()
        .zip(&reference)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f64::max);
    println!("| {n} | {nw} | {before:.3} s | {after:.3} s | {diff:e} |");
}

fn main() {
    let args: Vec<f64> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    println!("| Taps | NW | Bisection | Now | Max abs difference |");
    println!("|---|---|---|---|---|");
    if args.len() == 2 {
        row(args[0] as usize, args[1]);
        return;
    }
    for n in [1 << 20, 8 << 20] {
        for nw in [1.0, 4.0, 5.0, 7.0, 8.0, 10.0] {
            row(n, nw);
        }
    }
}

// The bisection `dpss` before the bracketed search.
fn dpss_old(n: usize, nw: f64) -> Vec<f64> {
    if n <= 2 {
        return vec![1.0; n];
    }
    let s = 2.0 * (std::f64::consts::PI * nw / n as f64).sin().powi(2);
    let mu = old_mu(n, s, nw);
    let mut half = Vec::with_capacity(n.div_ceil(2));
    old_shoot(n, s, mu, Some(&mut half));
    let mut w = half.clone();
    let mirrored = n / 2;
    w.extend(half[..mirrored].iter().rev());
    let peak = w.iter().copied().fold(0.0, f64::max);
    for v in &mut w {
        *v /= peak;
    }
    w
}

/// The smallest `mu` of [`dpss`]'s Sturm-Liouville form (`s = 2
/// sin^2(pi nw / n)`): the least `mu` whose solution shot from the edge
/// turns over at the centre (or changes sign before it).
fn old_mu(n: usize, s: f64, nw: f64) -> f64 {
    let above = |mu: f64| {
        let (sign_change, r) = old_shoot(n, s, mu, None);
        sign_change || r <= 0.0
    };
    let mut lo = 0.0;
    let mut hi = (std::f64::consts::PI * nw).max(1.0);
    while !above(hi) {
        lo = hi;
        hi *= 2.0;
    }
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if mid <= lo || mid >= hi {
            break;
        }
        if above(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    0.5 * (lo + hi)
}

/// The solution of [`dpss`]'s recurrence for `mu`, shot from `v_0 = 1` to
/// the centre (`v_0 ..= v_{(n - 1) / 2}`, kept in `out`): whether it
/// changes sign on the way, and the residual of the symmetric condition at
/// the centre relative to the centre value (positive while the solution
/// still rises there, so `mu` is below the eigenvalue).
fn old_shoot(n: usize, s: f64, mu: f64, mut out: Option<&mut Vec<f64>>) -> (bool, f64) {
    let e = |i: usize| (i as f64) * ((n - i) as f64) * 0.5;
    let q = |i: usize| {
        let c = 0.5 * (n as f64 - 1.0 - 2.0 * i as f64);
        c * c * s
    };
    // The last index of the half: m for n = 2m + 1, m - 1 for n = 2m.
    let last = (n - 1) / 2;
    let mut v = 1.0;
    // flux_i = e_{i+1} (v_{i+1} - v_i).
    let mut flux = 0.0;
    let mut sign_change = false;
    for i in 0..last {
        if let Some(o) = &mut out {
            o.push(v);
        }
        flux += (q(i) - mu) * v;
        let next = v + flux / e(i + 1);
        sign_change |= next <= 0.0;
        v = next;
    }
    if let Some(o) = &mut out {
        o.push(v);
    }
    let r = if n % 2 == 1 {
        // v_{m+1} = v_{m-1}: e_{m+1} = e_m, so flux_m = -flux_{m-1}.
        2.0 * flux + (q(last) - mu) * v
    } else {
        // v_m = v_{m-1}: flux_{m-1} = 0.
        flux + (q(last) - mu) * v
    };
    (sign_change, r / v.abs().max(f64::MIN_POSITIVE))
}
