use audivane_dsp_core::{
    Complex64 as C,
    fft::RealFftPair,
    resample::{UpsamplerDesign, design_kernel, nyquist_stop_cutoff},
};
use audivane_protocol::FilterPhase;
use std::time::Instant;

fn db(x: f64) -> f64 {
    20. * x.max(1e-300).log10()
}

/// Soft-floor homomorphic factorisation. Returns (b, tail_l1/r, tail_l2/l2)
fn minphase(h: &[f64], r: f64, floor_db: f64, nfft: usize, hard: bool) -> (Vec<f64>, f64, f64) {
    let mut fft = RealFftPair::new(nfft);
    let mut spec = vec![C::default(); fft.spec_len()];
    fft.forward_padded_complex(h, &mut spec);
    let eps = r * 10f64.powf(-floor_db / 20.);
    let e2 = eps * eps;
    for z in &mut spec {
        let m2 = z.norm_sqr();
        let v = if hard { m2.max(e2) } else { m2 + e2 };
        *z = C::new(0.5 * v.ln(), 0.);
    }
    let mut cep = vec![0.; nfft];
    fft.inverse_complex_normalized(&mut spec, &mut cep);
    for v in &mut cep[1..nfft / 2] {
        *v *= 2.;
    }
    cep[nfft / 2 + 1..].fill(0.);
    fft.forward_padded_complex(&cep, &mut spec);
    for z in &mut spec {
        *z = C::from_polar(z.re.exp(), z.im);
    }
    fft.inverse_complex_normalized(&mut spec, &mut cep);
    let n = h.len();
    let tail_l1: f64 = cep[n..].iter().map(|x| x.abs()).sum();
    let tail_e: f64 = cep[n..].iter().map(|x| x * x).sum();
    let all_e: f64 = cep.iter().map(|x| x * x).sum();
    cep.truncate(n);
    (cep, tail_l1 / r, (tail_e / all_e).sqrt())
}

/// Radially scaled complex cepstrum. a0: rho = exp(a0/N). Returns (b, tail_l1/r, max_step, resid_pi)
fn minphase_rad(h: &[f64], r: f64, a0: f64, nfft: usize) -> (Vec<f64>, f64, f64, f64, i64) {
    let n = h.len();
    let alpha = a0 / n as f64;
    let g: Vec<f64> = h.iter().enumerate().map(|(i, &x)| x * (-alpha * i as f64).exp()).collect();
    let mut fft = RealFftPair::new(nfft);
    let mut spec = vec![C::default(); fft.spec_len()];
    fft.forward_padded_complex(&g, &mut spec);
    // unwrap
    let mut phase = 0.0f64;
    let mut prev = spec[0];
    assert!(prev.re > 0.0);
    let mut max_step = 0f64;
    let mut logs = vec![C::default(); spec.len()];
    for k in 0..spec.len() {
        if k > 0 {
            let d = (spec[k] * prev.conj()).arg();
            max_step = max_step.max(d.abs());
            phase += d;
            prev = spec[k];
        }
        logs[k] = C::new(spec[k].norm().ln(), phase);
    }
    let half = spec.len() - 1;
    let lag = (-phase / std::f64::consts::PI).round();
    let resid = phase + lag * std::f64::consts::PI;
    for k in 0..spec.len() {
        let w = std::f64::consts::PI * k as f64 / half as f64;
        logs[k].im += lag * w;
    }
    spec.copy_from_slice(&logs);
    drop(logs);
    let mut cep = vec![0.; nfft];
    fft.inverse_complex_normalized(&mut spec, &mut cep);
    for m in 1..nfft / 2 {
        cep[m] += cep[nfft - m] * (-2.0 * alpha * m as f64).exp();
    }
    cep[nfft / 2 + 1..].fill(0.);
    cep[0] += lag * alpha;
    fft.forward_padded_complex(&cep, &mut spec);
    for z in &mut spec {
        *z = C::from_polar(z.re.exp(), z.im);
    }
    fft.inverse_complex_normalized(&mut spec, &mut cep);
    let tail_l1: f64 = cep[n..].iter().enumerate().map(|(i, x)| x.abs() * (alpha * (n + i) as f64).min(700.).exp()).sum::<f64>();
    let tail_raw: f64 = cep[n..].iter().map(|x| x.abs()).sum::<f64>();
    cep.truncate(n);
    for (i, v) in cep.iter_mut().enumerate() {
        *v *= (alpha * i as f64).exp();
    }
    let _ = tail_l1;
    (cep, tail_raw / r, max_step, resid, lag as i64)
}

struct M {
    pb: f64,
    mid: f64,
    env: f64,
    env_at: f64,
    ny_lin: f64,
    ny_min: f64,
    peak_lin: f64,
    peak_min: f64,
    energy_ok: bool,
}

fn metrics(lin: &[f64], min: &[f64], r: usize, cut: f64, beta: f64, grid_mult: usize) -> M {
    let nfft = (lin.len() * grid_mult).next_power_of_two();
    let mut fft = RealFftPair::new(nfft);
    let mut a = vec![C::default(); fft.spec_len()];
    let mut b = a.clone();
    fft.forward_padded_complex(lin, &mut a);
    fft.forward_padded_complex(min, &mut b);
    let rr = r as f64;
    let la: Vec<f64> = a.iter().map(|z| db(z.norm() / rr)).collect();
    let lb: Vec<f64> = b.iter().map(|z| db(z.norm() / rr)).collect();
    let fc = cut / (2. * rr);
    let aa = beta / 0.1102 + 8.7;
    let tw = (aa - 7.95) / (14.36 * (lin.len() - 1) as f64);
    let edge = fc + 0.6 * tw;
    let ny = 0.5 / rr;
    let mut pb = 0f64;
    let mut mid = 0f64;
    for k in 0..la.len() {
        let d = (la[k] - lb[k]).abs();
        if la[k] >= -1. {
            pb = pb.max(d);
        }
        if la[k] >= -100. {
            mid = mid.max(d);
        }
    }
    // suffix envelopes
    let kedge = (edge * nfft as f64).ceil() as usize;
    let kny = (ny * nfft as f64).ceil() as usize;
    let mut env = f64::NEG_INFINITY;
    let mut env_at = 0.;
    let (mut ea, mut eb) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    let mut ny_lin = 0.;
    let mut ny_min = 0.;
    let mut peak_lin = 0.;
    let mut peak_min = 0.;
    let probes = [0.0315f64, 0.032, 0.033, 0.035, 0.04, 0.05, 0.1, 0.2, 0.3, 0.4, 0.49];
    let mut prof = vec![];
    let mut env_lvl = (0f64, 0f64);
    let mut err = 0f64;
    for k in (kedge.min(la.len() - 1)..la.len()).rev() {
        let f = k as f64 / nfft as f64;
        if prof.len() < probes.len() && f <= probes[probes.len() - 1 - prof.len()] { prof.push((probes[probes.len() - 1 - prof.len()], ea, eb)); }
        ea = ea.max(la[k]);
        eb = eb.max(lb[k]);
        let lim: f64 = std::env::var("ENVLIM").ok().map(|s| s.parse().unwrap()).unwrap_or(-265.);
        if ea > lim && eb - ea > env {
            env = eb - ea;
            env_at = k as f64 / nfft as f64;
            env_lvl = (ea, eb);
        }
        err = err.max((a[k].norm() - b[k].norm()).abs() / rr);
        if k == kny {
            ny_lin = ea;
            ny_min = eb;
        }
        if k == kedge {
            peak_lin = ea;
            peak_min = eb;
        }
    }
    if std::env::var("PROFILE").is_ok() { for (f, a, b) in prof.iter().rev() { print!("[{f}: {a:.1}/{b:.1}] "); } println!(); println!("ENV levels lin {:.1} min {:.1}; max ||H_min|-|H_lin|| above the edge {:.1} dB", env_lvl.0, env_lvl.1, db(err)); }
    // energy partial sums
    let mut el = 0.;
    let mut em = 0.;
    let mut energy_ok = true;
    let tl: f64 = lin.iter().map(|x| x * x).sum();
    let tm: f64 = min.iter().map(|x| x * x).sum();
    for (x, y) in lin.iter().zip(min) {
        el += x * x;
        em += y * y;
        if em / tm < el / tl - 1e-12 {
            energy_ok = false;
        }
    }
    M { pb, mid, env, env_at, ny_lin, ny_min, peak_lin, peak_min, energy_ok }
}

/// Schur-Cohn step-down: true iff all zeros strictly inside the unit circle.
fn schur_cohn(b: &[f64]) -> (bool, f64) {
    let mut a: Vec<f64> = b.to_vec();
    let mut kmax = 0f64;
    while a.len() > 1 {
        let m = a.len() - 1;
        let k = a[m] / a[0];
        kmax = kmax.max(k.abs());
        if k.abs() >= 1. {
            return (false, kmax);
        }
        let d = 1. - k * k;
        let mut nxt = vec![0.; m];
        for i in 0..m {
            nxt[i] = (a[i] - k * a[m - i]) / d;
        }
        a = nxt;
    }
    (true, kmax)
}

/// Number of zeros of h outside radius exp(a/N) (winding count) and max phase step.
fn lag_at(h: &[f64], a: f64, nfft: usize) -> (i64, f64) {
    let n = h.len();
    let alpha = a / n as f64;
    let g: Vec<f64> = h.iter().enumerate().map(|(i, &x)| x * (-alpha * i as f64).exp()).collect();
    let mut fft = RealFftPair::new(nfft);
    let mut spec = vec![C::default(); fft.spec_len()];
    fft.forward_padded_complex(&g, &mut spec);
    let mut phase = 0.0f64;
    let mut ms = 0f64;
    for k in 1..spec.len() {
        let d = (spec[k] * spec[k - 1].conj()).arg();
        ms = ms.max(d.abs());
        phase += d;
    }
    ((-phase / std::f64::consts::PI).round() as i64, ms)
}

fn hwm() -> String { std::fs::read_to_string("/proc/self/status").unwrap().lines().find(|l| l.starts_with("VmHWM")).unwrap().to_string() }
fn main() {
    if std::env::args().nth(1).as_deref() == Some("sweepns") {
        let ph = if std::env::var("MIN").is_ok() { FilterPhase::Minimum } else { FilterPhase::Linear };
        sweep_ns(ph, std::env::var("MAXT").ok().map(|s| s.parse().unwrap()).unwrap_or(65536));
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("diag") {
        let a: Vec<String> = std::env::args().collect();
        diag(a[2].parse().unwrap(), a[3].parse().unwrap(), a[4].parse().unwrap(), a[5].parse().unwrap());
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("calib") {
        let ns: Vec<usize> = std::env::var("NS").unwrap().split(',').map(|x| x.parse().unwrap()).collect();
        let bs: Vec<f64> = std::env::var("BS").unwrap_or("4,8,14,20,24".into()).split(',').map(|x| x.parse().unwrap()).collect();
        let rs: Vec<usize> = std::env::var("RS").unwrap_or("2,4,8,16,32".into()).split(',').map(|x| x.parse().unwrap()).collect();
        let phase = if std::env::var("MIN").is_ok() { FilterPhase::Minimum } else { FilterPhase::Linear };
        for &n in &ns { for &b in &bs { for &r in &rs {
            let aa = b / 0.1102 + 8.7;
            let c12 = 1. - 1.2 * r as f64 * (aa - 7.95) / (14.36 * (n - 1) as f64);
            if c12 < 0.2 { println!("CALIB N={n} b={b} r={r} skip"); continue; }
            let (k, m12) = calib(n, r, b, phase);
            let ck = 1. - k * r as f64 * (aa - 7.95) / (14.36 * (n - 1) as f64); println!("CALIB N={n} b={b} r={r} kmin={k:.4} cut_at_kmin={ck:.4} excess_at_k1.2={m12:+.3}");
        }}}
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("plan") {
        let l: usize = 1 << 26;
        let mut p = realfft::RealFftPlanner::<f64>::new();
        let f = p.plan_fft_forward(l);
        println!("after fwd plan {} scratch {}", hwm(), f.get_scratch_len());
        let i = p.plan_fft_inverse(l);
        println!("after inv plan {} scratch {}", hwm(), i.get_scratch_len());
        let mut cp = rustfft::FftPlanner::<f64>::new();
        let c = cp.plan_fft_forward(l / 2);
        println!("after complex plan {} inplace scratch {}", hwm(), c.get_inplace_scratch_len());
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("time") {
        let n: usize = std::env::args().nth(2).unwrap().parse().unwrap();
        let phase = if std::env::args().nth(3).as_deref() == Some("min") { FilterPhase::Minimum } else { FilterPhase::Linear };
        let beta = 20.0;
        let t = Instant::now();
        let h = design_kernel(&UpsamplerDesign { ratio: 16, taps: n, phase, kaiser_beta: beta, cutoff: nyquist_stop_cutoff(n, 16, beta) });
        println!("TIME N={n} {phase:?} {:.2}s {} sum={}", t.elapsed().as_secs_f64(), hwm(), h.iter().sum::<f64>());
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("zeros") {
        let n: usize = std::env::args().nth(2).unwrap().parse().unwrap();
        let mult: usize = std::env::var("GRID").ok().map(|s| s.parse().unwrap()).unwrap_or(64);
        let nfft = (n * mult).next_power_of_two();
        let as_: Vec<f64> = std::env::var("AS").ok().map(|s| s.split(',').map(|x| x.parse().unwrap()).collect()).unwrap_or(vec![0.25, 0.5, 1., 1.5, 2., 3., 4., 6., 8., 12., 16.]);
        let bs: Vec<f64> = std::env::var("BS").ok().map(|s| s.split(',').map(|x| x.parse().unwrap()).collect()).unwrap_or(vec![0.0f64, 1., 2., 4., 6., 8., 10., 14., 20., 30., 40.]);
        let rs: Vec<usize> = std::env::var("RS").ok().map(|s| s.split(',').map(|x| x.parse().unwrap()).collect()).unwrap_or(vec![2, 4, 16, 64]);
        let cs: Vec<f64> = std::env::var("CS").ok().map(|s| s.split(',').map(|x| x.parse().unwrap()).collect()).unwrap_or(vec![0.05f64, 0.5, 0.9, 0.95, 1.0, -1.0]);
        for &r in &rs {
            for &beta in &bs {
                for &cut in &cs {
                    let c = if cut < 0. { nyquist_stop_cutoff(n, r, beta) } else { cut };
                    let h = design_kernel(&UpsamplerDesign { ratio: r, taps: n, phase: FilterPhase::Linear, kaiser_beta: beta, cutoff: c });
                    let lags: Vec<(i64, f64)> = as_.iter().map(|&a| lag_at(&h, a, nfft)).collect();
                    let base = lags[0].0;
                    let first_drop = as_.iter().zip(&lags).find(|(_, l)| l.0 != base).map(|(a, _)| *a).unwrap_or(f64::INFINITY);
                    let ms = lags.iter().map(|l| l.1).fold(0., f64::max);
                    println!("ZEROS N={n} r={r} b={beta} cut={c:.4} lag0={base} first_drop_a={first_drop} maxstep={ms:.3} lags={:?}", lags.iter().map(|l| l.0).collect::<Vec<_>>());
                }
            }
        }
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("sweep");
    let n: usize = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(16384);
    let r: usize = std::env::var("R").ok().map(|s| s.parse().unwrap()).unwrap_or(16);
    let betas: Vec<f64> = std::env::var("BETAS")
        .ok()
        .map(|s| s.split(',').map(|x| x.parse().unwrap()).collect())
        .unwrap_or(vec![8., 14., 20.]);
    let pads: Vec<usize> = std::env::var("PADS")
        .ok()
        .map(|s| s.split(',').map(|x| x.parse().unwrap()).collect())
        .unwrap_or(vec![4, 8, 16, 32]);
    let floors: Vec<String> = std::env::var("FLOORS")
        .ok()
        .map(|s| s.split(',').map(|x| x.to_string()).collect())
        .unwrap_or(vec!["A+20".into(), "A+40".into(), "A+60".into(), "300".into()]);
    let hard = std::env::var("HARD").is_ok();
    let grid: usize = std::env::var("GRID").ok().map(|s| s.parse().unwrap()).unwrap_or(16);
    for &beta in &betas {
        let aa = beta / 0.1102 + 8.7;
        let cut = match std::env::var("CUT") {
            Ok(c) => c.parse().unwrap(),
            Err(_) => nyquist_stop_cutoff(n, r, beta),
        };
        let d = UpsamplerDesign { ratio: r, taps: n, phase: FilterPhase::Linear, kaiser_beta: beta, cutoff: cut };
        let lin = design_kernel(&d);
        if mode == "with" {
            let a0: f64 = std::env::var("A0").unwrap().parse().unwrap();
            let mult: usize = std::env::var("MULT").unwrap().parse().unwrap();
            let t = Instant::now();
            let mp = audivane_dsp_core::minphase::minimum_phase_with(&lin, (n * mult).next_power_of_two(), a0);
            let el = t.elapsed().as_secs_f64();
            let mut min = mp.kernel.clone();
            let s: f64 = min.iter().sum(); for v in &mut min { *v *= r as f64 / s; }
            let m = metrics(&lin, &min, r, cut, beta, grid);
            println!("WITH N={n} b={beta} cut={cut:.6} a0={a0} L={mult}N t={el:.2}s exact={} floored={} step={:.3} tail={:.1e} pb={:.3e} mid={:.3e} env={:+.3}@{:.5} ny {:.2}/{:.2} peak {:.3}/{:.3}",
                mp.exact, mp.floored_bins, mp.max_phase_step, mp.tail, m.pb, m.mid, m.env, m.env_at, m.ny_lin, m.ny_min, m.peak_lin, m.peak_min);
            continue;
        }
        if mode == "prod" {
            let t = Instant::now();
            let min = design_kernel(&UpsamplerDesign { phase: FilterPhase::Minimum, ..d });
            let el = t.elapsed().as_secs_f64();
            let m = metrics(&lin, &min, r, cut, beta, grid);
            println!(
                "PROD N={n} b={beta} cut={cut:.6} t={el:.2}s pb={:.3e} mid={:.3e} env={:+.3}@{:.5} ny {:.2}/{:.2} peak {:.3}/{:.3} E={} {}",
                m.pb, m.mid, m.env, m.env_at, m.ny_lin, m.ny_min, m.peak_lin, m.peak_min, m.energy_ok,
                if n <= 8192 { let c=0.1; let sc: Vec<f64> = min.iter().enumerate().map(|(i,&x)| x*(-c*i as f64/n as f64).exp()).collect(); let sl: Vec<f64> = lin.iter().enumerate().map(|(i,&x)| x*(-c*i as f64/n as f64).exp()).collect(); format!("SC(min)={:?} SC(lin)={:?} wind_out(min,c=0.25)={:?} wind_out(lin)={:?}", schur_cohn(&sc), schur_cohn(&sl).0, lag_at(&min, 0.25, 64*n), lag_at(&lin, 0.25, 64*n).0) } else { format!("wind_out(min,c=0.25)={:?}", lag_at(&min, 0.25, 64*n)) }
            );
            if std::env::var("DUMP").is_ok() { let s: Vec<String> = min.iter().map(|x| format!("{x:e}")).collect(); std::fs::write(format!("min_{n}_{beta}.txt"), s.join("\n")).unwrap(); }
            continue;
        }
        if mode == "rad" {
            let a0s: Vec<f64> = std::env::var("A0S").ok().map(|s| s.split(',').map(|x| x.parse().unwrap()).collect()).unwrap_or(vec![3., 4., 6., 8.]);
            for &a0 in &a0s {
                for &p in &pads {
                    let nfft = (n * p).next_power_of_two();
                    let t = Instant::now();
                    let (min, tl1, ms, resid, lag) = minphase_rad(&lin, r as f64, a0, nfft);
                    let el = t.elapsed().as_secs_f64();
                    let m = metrics(&lin, &min, r, cut, beta, grid);
                    let sc = if n <= 8192 { format!("{:?}", schur_cohn(&min)) } else { String::new() };
                    println!(
                        "RAD N={n} b={beta} cut={cut:.6} a0={a0} L={}N t={el:.2}s tail={tl1:.2e} step={ms:.3} resid={resid:.1e} lag={lag} pb={:.3e} mid={:.3e} env={:+.3}@{:.5} ny {:.2}/{:.2} peak {:.3}/{:.3} E={} {sc}",
                        nfft / n, m.pb, m.mid, m.env, m.env_at, m.ny_lin, m.ny_min, m.peak_lin, m.peak_min, m.energy_ok
                    );
                }
            }
            continue;
        }
        for f in &floors {
            let fdb = if let Some(x) = f.strip_prefix("A+") { aa + x.parse::<f64>().unwrap() } else { f.parse().unwrap() };
            for &p in &pads {
                let nfft = (n * p).next_power_of_two();
                let t = Instant::now();
                let (min, tl1, tl2) = minphase(&lin, r as f64, fdb, nfft, hard);
                let el = t.elapsed().as_secs_f64();
                let m = metrics(&lin, &min, r, cut, beta, grid);
                let sc = if n <= 8192 { format!("{:?}", schur_cohn(&min)) } else { String::new() };
                println!(
                    "N={n} b={beta} cut={cut:.6} floor={fdb:.1} L={}N t={el:.2}s tail_l1={tl1:.2e} tail_l2={tl2:.2e} pb={:.3e} mid={:.3e} env={:+.3}@{:.5} ny {:.2}/{:.2} peak {:.3}/{:.3} E={} {sc}",
                    nfft / n, m.pb, m.mid, m.env, m.env_at, m.ny_lin, m.ny_min, m.peak_lin, m.peak_min, m.energy_ok
                );
            }
        }
    }
}

/// Peak magnitude (dB re ratio) of h over [f_lo, 0.5] cycles/sample: FFT grid
/// (>= 8 points per 1/N) plus direct refinement of every grid peak within
/// 0.5 dB of the grid maximum, and the endpoint f_lo itself.
pub fn peak_above(h: &[f64], r: f64, f_lo: f64) -> (f64, f64) {
    use audivane_dsp_core::resample::dft_magnitudes;
    let n = h.len();
    let nfft = (n * 8).next_power_of_two().max(1024);
    let mut fft = RealFftPair::new(nfft);
    let mut spec = vec![C::default(); fft.spec_len()];
    fft.forward_padded_complex(h, &mut spec);
    let mag: Vec<f64> = spec.iter().map(|c| c.norm()).collect();
    drop(spec);
    let k0 = (f_lo * nfft as f64).ceil() as usize;
    let gmax = mag[k0..].iter().copied().fold(0., f64::max);
    let thr = gmax * 10f64.powf(-0.5 / 20.);
    let df = 1.0 / nfft as f64;
    let mut cands = vec![];
    for k in k0..mag.len() {
        let l = if k > 0 { mag[k - 1] } else { 0. };
        let rr = if k + 1 < mag.len() { mag[k + 1] } else { 0. };
        if mag[k] >= thr && mag[k] >= l && mag[k] >= rr {
            cands.push(k as f64 * df);
        }
    }
    let mut best = dft_magnitudes(h, 1.0, &[f_lo])[0];
    let mut at = f_lo;
    // golden-section-like refinement: 3 rounds of 17-point local grids
    for c in cands {
        let mut lo = (c - df).max(f_lo);
        let mut hi = (c + df).min(0.5);
        for _ in 0..4 {
            let fs: Vec<f64> = (0..=16).map(|i| lo + (hi - lo) * i as f64 / 16.).collect();
            let m = dft_magnitudes(h, 1.0, &fs);
            let (i, v) = m.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
            if *v > best {
                best = *v;
                at = fs[i];
            }
            let w = (hi - lo) / 16.;
            lo = (fs[i] - w).max(f_lo);
            hi = (fs[i] + w).min(0.5);
        }
    }
    (db(best / r), at)
}

pub fn calib(n: usize, r: usize, beta: f64, phase: FilterPhase) -> (f64, f64) {
    let aa = beta / 0.1102 + 8.7;
    let ny = 0.5 / r as f64;
    let peak_k = |k: f64| {
        let cut = 1. - k * r as f64 * (aa - 7.95) / (14.36 * (n - 1) as f64);
        let h = design_kernel(&UpsamplerDesign { ratio: r, taps: n, phase, kaiser_beta: beta, cutoff: cut });
        peak_above(&h, r as f64, ny).0
    };
    // find smallest k with peak <= -A (assume monotone-ish), bisection on [0.6, 2.0]
    let (mut lo, mut hi) = (0.6, 2.0);
    for _ in 0..14 {
        let mid = 0.5 * (lo + hi);
        if peak_k(mid) <= -aa { hi = mid } else { lo = mid }
    }
    (hi, peak_k(1.2) + aa)
}

pub fn diag(n: usize, beta: f64, a0: f64, mult: usize) {
    let r = 16usize;
    let cut = nyquist_stop_cutoff(n, r, beta);
    let h = design_kernel(&UpsamplerDesign { ratio: r, taps: n, phase: FilterPhase::Linear, kaiser_beta: beta, cutoff: cut });
    let nfft = (n * mult).next_power_of_two();
    let alpha = a0 / n as f64;
    let g: Vec<f64> = h.iter().enumerate().map(|(i, &x)| x * (-alpha * i as f64).exp()).collect();
    let mut fft = RealFftPair::new(nfft);
    let mut spec = vec![C::default(); fft.spec_len()];
    fft.forward_padded_complex(&g, &mut spec);
    let mx = spec.iter().map(|c| c.norm()).fold(0., f64::max);
    let mut hist = [0usize; 8];
    let mut bad = 0usize;
    let mut first_bad = None;
    let mut minmag = f64::INFINITY;
    for k in 1..spec.len() {
        let d = (spec[k] * spec[k - 1].conj()).arg().abs();
        let m = db(spec[k].norm() / mx);
        minmag = minmag.min(m);
        let b = ((-m - 180.) / 20.).clamp(0., 7.) as usize;
        hist[b] += 1;
        if d > std::f64::consts::FRAC_PI_2 { bad += 1; if first_bad.is_none() { first_bad = Some((k as f64 / nfft as f64, m)); } }
    }
    println!("DIAG N={n} b={beta} a0={a0} L={}N bad={bad} first_bad={first_bad:?} min_db={minmag:.1} hist(180..,200..,..)={hist:?}", nfft / n);
}

/// Broad Nyquist-stop sweep with the production formula, kernel level.
pub fn sweep_ns(phase: FilterPhase, max_taps: usize) {
    use audivane_dsp_core::resample::{kaiser_attenuation_db, nyquist_stop_check, nyquist_stop_min_taps};
    let mut jobs = vec![];
    // The calibrated range, beta 5..=24 in steps of 1/8.
    for bi in 0..=152 {
        let beta = 5.0 + bi as f64 * 0.125;
        for &r in &[2usize, 3, 4, 8, 16, 32] {
            let m = nyquist_stop_min_taps(r, beta).unwrap();
            for mult in [1.0, 1.0001, 1.5, 2.0, 3.0, 5.0, 10.0, 30.0, 100.0] {
                let taps = ((m as f64 * mult).ceil() as usize).max(m + (mult > 1.0) as usize);
                if taps <= max_taps { jobs.push((beta, r, taps)); }
            }
        }
    }
    let jobs = std::sync::Mutex::new(jobs);
    let out = std::sync::Mutex::new(vec![]);
    std::thread::scope(|s| {
        for _ in 0..40 {
            s.spawn(|| loop {
                let job = jobs.lock().unwrap().pop();
                let Some((beta, r, taps)) = job else { break };
                let cut = nyquist_stop_check(taps, r, beta).unwrap();
                let h = design_kernel(&UpsamplerDesign { ratio: r, taps, phase, kaiser_beta: beta, cutoff: cut });
                let (pk, at) = peak_above(&h, r as f64, 0.5 / r as f64);
                let margin = pk + kaiser_attenuation_db(beta);
                out.lock().unwrap().push((margin, beta, r, taps, cut, at));
            });
        }
    });
    let mut v = out.into_inner().unwrap();
    v.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!("SWEEP {phase:?} cases={} worst:", v.len());
    for x in v.iter().filter(|x| x.1 >= std::env::var("BMIN").ok().map(|s| s.parse().unwrap()).unwrap_or(0.0)).take(25) { println!("  margin={:+.4} beta={} r={} taps={} cutoff={:.5} at={:.6}", x.0, x.1, x.2, x.3, x.4, x.5); }
}
