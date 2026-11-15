use audivane_dsp_core::{
    Complex64,
    fft::RealFftPair,
    minphase,
    resample::{
        UpsamplerDesign, design_kernel, dft_magnitudes, kaiser_attenuation_db, measure_upsampler,
        nyquist_stop_check, nyquist_stop_min_taps,
    },
};
use audivane_protocol::{CutoffMode, FilterPhase, RateTarget, UpsamplerConfig};
use std::time::Instant;
unsafe extern "C" {
    fn reference(h: *const f64, n: usize, cycles: f64, out: *mut f64);
}
fn db(x: f64) -> f64 {
    20. * x.max(1e-300).log10()
}
fn spectrum(h: &[f64], pad: usize) -> Vec<f64> {
    let mut fft = RealFftPair::new((h.len() * pad).next_power_of_two().max(1024));
    let mut s = vec![Complex64::default(); fft.spec_len()];
    fft.forward_padded_complex(h, &mut s);
    s.iter().map(|v| v.norm()).collect()
}
fn ld(h: &[f64], f: f64) -> f64 {
    let mut x = [0.; 2];
    unsafe { reference(h.as_ptr(), h.len(), f, x.as_mut_ptr()) };
    x[0].hypot(x[1])
}
fn design(n: usize, r: usize, b: f64, c: f64, p: FilterPhase) -> Vec<f64> {
    design_kernel(&UpsamplerDesign {
        taps: n,
        ratio: r,
        kaiser_beta: b,
        cutoff: c,
        phase: p,
    })
}
// Independent 32N grid, all sampled local maxima considered (no 24/64-peak cap).
// Refine the strongest 8 with golden section and the direct production DFT;
// validate the final winning point using long-double C arithmetic.
fn peak(h: &[f64], r: usize, start: f64) -> (f64, f64) {
    let m = spectrum(h, 32);
    let l = 2 * (m.len() - 1);
    let k0 = (start * l as f64).ceil() as usize;
    let mut ks: Vec<_> = (k0..m.len())
        .filter(|&k| (k == k0 || m[k] >= m[k - 1]) && (k + 1 == m.len() || m[k] >= m[k + 1]))
        .collect();
    ks.sort_unstable_by(|&a, &b| m[b].total_cmp(&m[a]));
    ks.truncate(8);
    let mut best = (start, ld(h, start));
    for k in ks {
        let mut lo = (k.saturating_sub(1) as f64 / l as f64).max(start);
        let mut hi = ((k + 1) as f64 / l as f64).min(0.5);
        for _ in 0..24 {
            let a = lo + (hi - lo) * 0.3819660112501051;
            let b = lo + (hi - lo) * 0.6180339887498949;
            let v = dft_magnitudes(h, 1., &[a, b]);
            if v[0] > v[1] { hi = b } else { lo = a }
        }
        let f = (lo + hi) * 0.5;
        let v = ld(h, f);
        if v > best.1 {
            best = (f, v)
        }
    }
    (best.0, db(best.1 / r as f64))
}
fn rng(s: &mut u64) -> f64 {
    *s = s
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    ((*s >> 11) as f64) / ((1u64 << 53) as f64)
}
fn minscan(count: usize) {
    let mut seed = 384293u64;
    let mut worst: Vec<_> = vec![];
    let mut fallbacks = 0;
    for i in 0..count {
        let n = 2 + (rng(&mut seed) * 1023.) as usize;
        let r = [2, 3, 4, 8, 16, 32, 64][(rng(&mut seed) * 7.) as usize];
        let b = 2. + 18. * rng(&mut seed);
        let c = if i % 3 == 0 {
            10f64.powf(-5. * rng(&mut seed))
        } else {
            0.5 + 0.5 * rng(&mut seed)
        };
        let lin = design(n, r, b, c, FilterPhase::Linear);
        let report = minphase::minimum_phase(&lin, b);
        if !report.exact {
            fallbacks += 1;
        }
        let mut min = report.kernel;
        let sum = min.iter().sum::<f64>();
        for x in &mut min {
            *x *= r as f64 / sum;
        }
        let a = spectrum(&lin, 32);
        let z = spectrum(&min, 32);
        let mut pb = 0f64;
        let mut upper = 0f64;
        let mut abs = 0f64;
        for (&x, &y) in a.iter().zip(&z) {
            let x = db(x / r as f64);
            let y = db(y / r as f64);
            if x >= -1. {
                pb = pb.max((y - x).abs());
            }
            if x >= -100. {
                upper = upper.max((y - x).abs());
            }
            abs = abs.max((10f64.powf(x / 20.) - 10f64.powf(y / 20.)).abs());
        }
        worst.push((
            pb,
            upper,
            db(abs),
            n,
            r,
            b,
            c,
            report.exact,
            report.max_phase_step,
        ));
    }
    worst.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!("MINSCAN count={count} fallback={fallbacks}");
    for x in worst.iter().take(20) {
        println!("{x:?}");
    }
    worst.sort_by(|a, b| b.1.total_cmp(&a.1));
    println!("UPPER worst:");
    for x in worst.iter().take(10) {
        println!("{x:?}");
    }
}
fn nscan(count: usize) {
    let mut seed = 92385u64;
    let mut worst = vec![];
    for i in 0..count {
        let r = if std::env::var_os("ALL_RATIOS").is_some() {
            2 + i % 31
        } else {
            [2, 3, 4, 8, 16, 32, 64][i % 7]
        };
        let b = match i % 5 {
            0 => 5. + 0.125 * rng(&mut seed),
            1 => 24. - 0.125 * rng(&mut seed),
            _ => 5. + 19. * rng(&mut seed),
        };
        let b = if std::env::var_os("MIN").is_some() {
            5. + (b - 5.) * 15. / 19.
        } else {
            b
        };
        let n0 = nyquist_stop_min_taps(r, b).unwrap();
        let n = match i % 4 {
            0 => n0,
            1 => n0 + 1 + ((rng(&mut seed) * 16.) as usize),
            _ => ((n0 as f64) * (1. + 40. * rng(&mut seed))).ceil() as usize,
        };
        let c = nyquist_stop_check(n, r, b).unwrap();
        let phase = if std::env::var_os("MIN").is_some() {
            FilterPhase::Minimum
        } else {
            FilterPhase::Linear
        };
        let h = design(n, r, b, c, phase);
        // A 16N grid preselects the cases closest to failure, then peak() refines.
        let m = spectrum(&h, 16);
        let l = 2 * (m.len() - 1);
        let start = (0.5 / r as f64 * l as f64).ceil() as usize;
        let pk = m[start..]
            .iter()
            .copied()
            .fold(ld(&h, 0.5 / r as f64), f64::max);
        worst.push((db(pk / r as f64) + kaiser_attenuation_db(b), n, r, b, c));
    }
    worst.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!("NSCAN count={count}, refined top 30:");
    for &(_, n, r, b, c) in worst.iter().take(30) {
        let phase = if std::env::var_os("MIN").is_some() {
            FilterPhase::Minimum
        } else {
            FilterPhase::Linear
        };
        let h = design(n, r, b, c, phase);
        let (f, pk) = peak(&h, r, 0.5 / r as f64);
        println!(
            "margin={:+.9} N={n} r={r} beta={b:.17} cutoff={c:.17} f={f:.17} peak={pk:.9}",
            pk + kaiser_attenuation_db(b)
        );
    }
}
fn case(n: usize, r: usize, b: f64, c: f64) {
    let t = Instant::now();
    let lin = design(n, r, b, c, FilterPhase::Linear);
    let tlin = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let report = minphase::minimum_phase(&lin, b);
    println!(
        "CASE N={n} r={r} b={b:.17} cut={c:.17} tlin={tlin:.3} tmin={:.3} L={} exact={} reflected={} step={} tail={} floored={}",
        t.elapsed().as_secs_f64(),
        report.fft_len,
        report.exact,
        report.reflected_zeros,
        report.max_phase_step,
        report.tail,
        report.floored_bins
    );
    let mut min = report.kernel;
    let scale = r as f64 / min.iter().sum::<f64>();
    for v in &mut min {
        *v *= scale;
    }
    if let Ok(dir) = std::env::var("DUMP_DIR") {
        std::fs::create_dir_all(&dir).unwrap();
        for (name, h) in [("linear", &lin), ("minimum", &min)] {
            std::fs::write(
                format!("{dir}/{name}.txt"),
                h.iter().map(|v| format!("{v:.17e}\n")).collect::<String>(),
            )
            .unwrap();
        }
    }
    let pad = std::env::var("PAD")
        .ok()
        .map(|v| v.parse().unwrap())
        .unwrap_or(16);
    let a = spectrum(&lin, pad);
    let z = spectrum(&min, pad);
    let l = 2 * (a.len() - 1);
    let edge =
        c / (2. * r as f64) + 0.6 * (kaiser_attenuation_db(b) - 7.95) / (14.36 * (n - 1) as f64);
    let (mut pb, mut upper, mut err, mut env, mut ea, mut eb) =
        (0f64, 0f64, 0f64, f64::NEG_INFINITY, 0f64, 0f64);
    let mut at = 0.;
    for k in (0..a.len()).rev() {
        let x = a[k] / r as f64;
        let y = z[k] / r as f64;
        let d = (db(x) - db(y)).abs();
        if db(x) >= -1. && d > pb {
            pb = d;
            at = k as f64 / l as f64;
        }
        if db(x) >= -100. {
            upper = upper.max(d);
        }
        if k as f64 / l as f64 >= edge {
            err = err.max((x - y).abs());
            ea = ea.max(x);
            eb = eb.max(y);
            if db(ea) >= -(kaiser_attenuation_db(b) + 5.) {
                env = env.max(db(eb) - db(ea));
            }
        }
    }
    let kny = (0.5 / r as f64 * l as f64).ceil() as usize;
    println!(
        "above-Nyquist grid peaks linear={:.9} minimum={:.9}",
        db(a[kny..].iter().copied().fold(0., f64::max) / r as f64),
        db(z[kny..].iter().copied().fold(0., f64::max) / r as f64)
    );
    println!(
        "grid L={l} passband={pb:.9e} at={at:.17} upper={upper:.9e} stop_error={:.6} bound={:.6} envelope={env:.6}",
        db(err),
        -kaiser_attenuation_db(b) - 25.
    );
    let fs: Vec<f64> = (0..=2048)
        .map(|i| (c / (2. * r as f64) + (i as f64 / 2048. - 0.5) * 12. / n as f64).clamp(0., 0.5))
        .collect();
    let aa = dft_magnitudes(&lin, 1., &fs);
    let bb = dft_magnitudes(&min, 1., &fs);
    let mut upperd = 0f64;
    let mut pbd = 0f64;
    let mut witness = 0;
    for (i, (&x, &y)) in aa.iter().zip(&bb).enumerate() {
        let d = (db(x) - db(y)).abs();
        if db(x / r as f64) >= -1. && d > pbd {
            pbd = d;
            witness = i;
        }
        if db(x / r as f64) >= -100. {
            upperd = upperd.max(d);
        }
    }
    let f = fs[witness];
    println!(
        "edge direct passband={pbd:.9e} upper={upperd:.9e}; long-double witness f={f:.17} lin={:.12} min={:.12} delta={:.9e}",
        db(ld(&lin, f) / r as f64),
        db(ld(&min, f) / r as f64),
        db(ld(&min, f) / ld(&lin, f))
    );
    if std::env::var_os("PEAK").is_some() {
        for (name, h) in [("linear", &lin), ("minimum", &min)] {
            let (f, p) = peak(h, r, 0.5 / r as f64);
            println!(
                "{name} peak={p:.9} f={f:.17} margin={:+.9}",
                p + kaiser_attenuation_db(b)
            );
        }
    }
}
fn summary(n: usize, r: usize, b: f64, c: f64) {
    for phase in [FilterPhase::Linear, FilterPhase::Minimum] {
        let cfg = UpsamplerConfig {
            target: RateTarget::Ratio(r as u32),
            taps: n,
            phase,
            kaiser_beta: b,
            cutoff: c,
            cutoff_mode: if c == 0. {
                CutoffMode::NyquistStop
            } else {
                CutoffMode::Fraction
            },
        };
        let m = measure_upsampler(&cfg, 48000).unwrap();
        let start = m.response.stopband_start_hz / m.output_rate() as f64;
        let h = design(n, r, b, m.response.effective_cutoff, phase);
        let (f, _) = peak(&h, r, start);
        let p = m.magnitude_db_at(&[f * m.output_rate() as f64])[0];
        println!(
            "SUMMARY N={n} r={r} beta={b} cutoff={c} {phase:?} start={:.12} reported={:.9} actual={p:.9} excess={:+.9} at_hz={:.12}",
            m.response.stopband_start_hz,
            -m.response.stopband_atten_db,
            p + m.response.stopband_atten_db,
            f * m.output_rate() as f64
        );
    }
}
fn summaryscan(count: usize) {
    let mut seed = 394453;
    let mut worst = vec![];
    for i in 0..count {
        let r = [2, 3, 4, 8, 16, 32][i % 6];
        let b = if i % 3 == 0 {
            24. + 16. * rng(&mut seed)
        } else {
            2. + 18. * rng(&mut seed)
        };
        let n = 32 + (rng(&mut seed) * 5000.) as usize;
        let c = if i % 4 == 0 {
            0.95
        } else {
            0.5 + 0.5 * rng(&mut seed)
        };
        let phase = if i % 2 == 0 || b > 20. {
            FilterPhase::Linear
        } else {
            FilterPhase::Minimum
        };
        let cfg = UpsamplerConfig {
            target: RateTarget::Ratio(r as u32),
            taps: n,
            phase,
            kaiser_beta: b,
            cutoff: c,
            cutoff_mode: CutoffMode::Fraction,
        };
        let m = measure_upsampler(&cfg, 48000).unwrap();
        let h = design(n, r, b, c, phase);
        let start = m.response.stopband_start_hz / m.output_rate() as f64;
        let (f, _) = peak(&h, r, start);
        let actual = m.magnitude_db_at(&[f * m.output_rate() as f64])[0];
        worst.push((
            actual + m.response.stopband_atten_db,
            n,
            r,
            b,
            c,
            phase,
            m.response.stopband_start_hz,
            -m.response.stopband_atten_db,
            actual,
            f,
        ));
    }
    worst.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!("SUMMARYSCAN count={count}");
    for x in worst.iter().take(25) {
        println!("{x:?}");
    }
}
fn nycase(n: usize, r: usize, b: f64) {
    let c = nyquist_stop_check(n, r, b).unwrap();
    let h = design(n, r, b, c, FilterPhase::Linear);
    let (f, p) = peak(&h, r, 0.5 / r as f64);
    println!(
        "NYCASE N={n} r={r} beta={b:.17} cutoff={c:.17} peak={p:.9} margin={:+.9} f={f:.17}",
        p + kaiser_attenuation_db(b)
    );
}
fn edge(count: usize, r: usize, beta: f64, width: f64) {
    let mut v = vec![];
    for i in 0..=count {
        let b = beta - width + 2. * width * i as f64 / count as f64;
        let n0 = nyquist_stop_min_taps(r, b).unwrap();
        for n in n0..=n0 + 32 {
            let c = nyquist_stop_check(n, r, b).unwrap();
            let h = design(n, r, b, c, FilterPhase::Linear);
            let f = 0.5 / r as f64;
            let p = db(dft_magnitudes(&h, 1., &[f])[0] / r as f64);
            v.push((p + kaiser_attenuation_db(b), n, b, c));
        }
    }
    v.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!("EDGE cases={} r={r}", v.len());
    for &(_, n, b, c) in v.iter().take(30) {
        let h = design(n, r, b, c, FilterPhase::Linear);
        let p = db(ld(&h, 0.5 / r as f64) / r as f64);
        println!(
            "margin={:+.9} N={n} r={r} beta={b:.17} cutoff={c:.17} nyquist={p:.9}",
            p + kaiser_attenuation_db(b)
        );
    }
}
fn outside(h: &[f64], radius: f64) -> (i64, f64) {
    let n = h.len();
    let g: Vec<_> = h
        .iter()
        .enumerate()
        .map(|(i, &x)| x * (-radius * i as f64 / n as f64).exp())
        .collect();
    let mut fft = RealFftPair::new((n * 256).next_power_of_two());
    let mut s = vec![Complex64::default(); fft.spec_len()];
    fft.forward_padded_complex(&g, &mut s);
    let mut phase = 0.;
    let mut step = 0f64;
    for k in 1..s.len() {
        let d = (s[k] * s[k - 1].conj()).arg();
        phase += d;
        step = step.max(d.abs());
    }
    ((-phase / std::f64::consts::PI).round() as i64, step)
}
fn shortscan() {
    let mut count = 0;
    let mut bad = vec![];
    let mut fall = vec![];
    for n in 3..=128 {
        for r in [2, 4, 8, 16, 32] {
            for b in [2., 8., 14., 18., 19., 20.] {
                for c in [0.95, 1.] {
                    count += 1;
                    let lin = design(n, r, b, c, FilterPhase::Linear);
                    let m = minphase::minimum_phase(&lin, b);
                    let z = outside(&m.kernel, 0.25);
                    if z.0 > 0 && z.1 < 1.5 {
                        bad.push((n, r, b, c, m.exact, z));
                    }
                    if !m.exact {
                        fall.push((n, r, b, c));
                    }
                }
            }
        }
    }
    println!(
        "SHORTSCAN {count} designs; outside e^(0.25/N): {} ; fallback: {}",
        bad.len(),
        fall.len()
    );
    for x in bad.iter() {
        println!("OUTSIDE {x:?}");
    }
    for x in fall.iter() {
        println!("FALLBACK {x:?}");
    }
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).map(|x| x.as_str()).unwrap_or("minscan");
    let n = args.get(2).map(|x| x.parse().unwrap()).unwrap_or(3000);
    let r = args.get(3).map(|x| x.parse().unwrap()).unwrap_or(32);
    let b = args.get(4).map(|x| x.parse().unwrap()).unwrap_or(20.);
    let c = args.get(5).map(|x| x.parse().unwrap()).unwrap_or_else(|| {
        if mode == "case" || mode == "summary" {
            nyquist_stop_check(n, r, b).unwrap()
        } else {
            0.95
        }
    });
    match mode {
        "minscan" => minscan(n),
        "nscan" => nscan(n),
        "case" => case(n, r, b, c),
        "summary" => summary(n, r, b, c),
        "summaryscan" => summaryscan(n),
        "nycase" => nycase(n, r, b),
        "edge" => edge(n, r, b, c),
        "shortscan" => shortscan(),
        "time" => {
            let t = Instant::now();
            let h = design(n, r, b, c, FilterPhase::Minimum);
            println!(
                "TIME N={n} r={r} beta={b} cutoff={c} seconds={:.6} sum={} {}",
                t.elapsed().as_secs_f64(),
                h.iter().sum::<f64>(),
                std::fs::read_to_string("/proc/self/status")
                    .unwrap()
                    .lines()
                    .find(|s| s.starts_with("VmHWM"))
                    .unwrap()
            );
        }
        _ => panic!("unknown mode"),
    }
}
