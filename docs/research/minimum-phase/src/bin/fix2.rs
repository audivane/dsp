//! Experiments for the second scientific review fix (`docs/research/minimum-phase/README.md`, "Second review"). Modes: case, roots, psweep, wsweep, csweep, long, radius, summ, mtime, dtime, dump.
use audivane_dsp_core::{minphase, resample::*};
use audivane_protocol::FilterPhase;
use std::time::Instant;
fn lin(n: usize, r: usize, b: f64, c: f64) -> (Vec<f64>, UpsamplerDesign) {
    let d = UpsamplerDesign { ratio: r, taps: n, phase: FilterPhase::Linear, kaiser_beta: b, cutoff: c };
    (design_kernel(&d), d)
}
fn outside(h: &[f64], radius: f64, pad: usize) -> (i64, f64) {
    use audivane_dsp_core::{Complex64, fft::RealFftPair};
    let n = h.len();
    let g: Vec<_> = h.iter().enumerate().map(|(i, &x)| x * (-radius * i as f64 / n as f64).exp()).collect();
    let mut fft = RealFftPair::new((n * pad).next_power_of_two());
    let mut s = vec![Complex64::default(); fft.spec_len()];
    fft.forward_padded_complex(&g, &mut s);
    let mut phase = 0.; let mut step = 0f64;
    for k in 1..s.len() { let d = (s[k] * s[k - 1].conj()).arg(); phase += d; step = step.max(d.abs()); }
    ((-phase / std::f64::consts::PI).round() as i64, step)
}
/// largest |z|-1 among zeros where |B| on the circle at their angle is above -250 / -200 dB re DC
fn level_radius(b: &[f64]) -> (f64, f64) {
    let z = minphase::zeros(b).unwrap();
    let dc: f64 = b.iter().sum::<f64>().abs();
    let (mut a, mut c) = (0f64, 0f64);
    for zi in z { let rr = zi.norm() - 1.0; if rr <= 0.0 { continue; }
        let f = zi.arg() / (2.0 * std::f64::consts::PI);
        let m = dft_magnitudes(b, 1.0, &[f.abs()])[0] / dc;
        let db = 20.0 * m.max(1e-300).log10();
        if db > -250.0 { a = a.max(rr); } if db > -200.0 { c = c.max(rr); } }
    (a, c)
}
fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "case" => {
            let n = a[2].parse().unwrap(); let r = a[3].parse().unwrap(); let b = a[4].parse().unwrap(); let c = a[5].parse().unwrap();
            let (h, d) = lin(n, r, b, c);
            let t = Instant::now();
            let m = minphase::minimum_phase(&h, &minimum_phase_contract(&d));
            let el = t.elapsed().as_secs_f64();
            match m {
                Ok(m) => println!("N={n} r={r} b={b} c={c} t={el:.3}s {:?} L={} refl={} tail={:.2e} {:?}", m.method, m.fft_len, m.reflected_zeros, m.tail, m.check),
                Err(e) => println!("N={n} r={r} b={b} c={c} t={el:.3}s ERR {e}"),
            }
        }
        "sweep" => {
            let lo: usize = a[2].parse().unwrap(); let hi: usize = a[3].parse().unwrap();
            let mut worst_excess = 0f64; let mut worst_rad = 0f64; let mut fails = 0; let mut count = 0;
            let t = Instant::now();
            for n in lo..=hi { for r in [2,3,4,8,16,32] { for b in [2.0, 5.0, 8.0, 11.0, 14.0, 17.0, 18.0, 19.0, 20.0] { for c in [1.0, 0.95, 0.5] {
                count += 1;
                let (h, d) = lin(n, r, b, c);
                match minphase::minimum_phase(&h, &minimum_phase_contract(&d)) {
                    Ok(m) => { let ch = m.check.unwrap(); if let Some(x) = ch.max_zero_excess { if x > worst_excess { worst_excess = x; println!("  excess {x:.3} rad {:.3e} at N={n} r={r} b={b} c={c}", ch.max_zero_radius.unwrap()-1.0);} } if let Some(x)=ch.max_zero_radius { worst_rad = worst_rad.max(x); } }
                    Err(e) => { fails += 1; if fails < 30 { println!("FAIL N={n} r={r} b={b} c={c}: {e}"); } }
                }
            }}}}
            println!("SWEEP {lo}..{hi}: {count} designs, {fails} failures, worst excess {worst_excess:.3}, worst radius-1 {:.3e}, {:.1}s", worst_rad-1.0, t.elapsed().as_secs_f64());
        }
        "psweep" => {
            // all N in lo..=hi; full grid of (r, b, c) when `full`, else one combo per N cycling
            let lo: usize = a[2].parse().unwrap(); let hi: usize = a[3].parse().unwrap(); let full = a.get(4).map(|s| s == "full").unwrap_or(false);
            let rs = [2usize,3,4,8,16,32]; let bs = [2.0, 3.5, 5.0, 8.0, 11.0, 14.0, 17.0, 18.5, 19.0, 19.817, 20.0]; let cs = [1.0, 0.95, 0.5, 0.333, 0.9883];
            let mut jobs = vec![];
            let mut i = 0usize;
            for n in lo..=hi { if full { for &r in &rs { for &b in &bs { for &c in &cs { jobs.push((n,r,b,c)); }}}} else { for _ in 0..6 { let r = rs[i % rs.len()]; let b = bs[(i / 6) % bs.len()]; let c = cs[(i / 66) % cs.len()]; jobs.push((n,r,b,c)); i += 1; } } }
            let t = Instant::now();
            let jobs = std::sync::Mutex::new(jobs.into_iter());
            let out = std::sync::Mutex::new(vec![]);
            std::thread::scope(|sc| { for _ in 0..44 { sc.spawn(|| loop {
                let job = jobs.lock().unwrap().next(); let Some((n,r,b,c)) = job else { break };
                let (h, d) = lin(n, r, b, c);
                let res = minphase::minimum_phase(&h, &minimum_phase_contract(&d));
                let lv = res.as_ref().ok().map(|m| level_radius(&m.kernel)).unwrap_or((0.0, 0.0));
                out.lock().unwrap().push((n,r,b,c, res.map(|m| (m.method, m.check.unwrap(), m.tail, lv)).map_err(|e| e.to_string())));
            }); } });
            let out = out.into_inner().unwrap();
            let mut fails = 0; let (mut wex, mut wrad, mut wpb, mut wup, mut wst, mut wtail) = (0f64, 0f64, 0f64, 0f64, f64::NEG_INFINITY, 0f64); let (mut wl250, mut wl200) = (0f64, 0f64);
            for (n,r,b,c,res) in &out { match res {
                Ok((_, ch, tail, lv)) => { if lv.0 > wl250 { wl250 = lv.0; println!("  worst radius-1 above -250 dB {:.3e} N={n} r={r} b={b} c={c}", lv.0); } if lv.1 > wl200 { wl200 = lv.1; } if let Some(x)=ch.max_zero_excess { if x > wex { wex = x; println!("  worst excess {x:.3e} radius-1 {:.3e} N={n} r={r} b={b} c={c}", ch.max_zero_radius.unwrap()-1.0); } }
                    if let Some(x)=ch.max_zero_radius { if x-1.0 > wrad { wrad = x-1.0; println!("  worst radius-1 {:.3e} N={n} r={r} b={b} c={c}", x-1.0); } }
                    wpb = wpb.max(ch.passband_db); wup = wup.max(ch.upper_db); wtail = wtail.max(*tail);
                    let margin = ch.stopband_error_db + (b/0.1102+8.7); if margin > wst { wst = margin; }
                }
                Err(e) => { fails += 1; if fails < 40 { println!("FAIL N={n} r={r} b={b} c={c}: {e}"); } } } }
            println!("PSWEEP {lo}..{hi} full={full}: {} designs, {fails} failures; worst excess {wex:.3e}, radius-1 {wrad:.3e} (above -250 dB {wl250:.3e}, above -200 dB {wl200:.3e}), passband {wpb:.2e} dB, upper {wup:.2e} dB, stopband error re -A {wst:+.1} dB, tail {wtail:.1e}; {:.1}s", out.len(), t.elapsed().as_secs_f64());
        }
        "wsweep" => {
            // cepstral range: winding count of the result on e^{c/N}
            let lo: usize = a[2].parse().unwrap(); let hi: usize = a[3].parse().unwrap(); let stride: usize = a[4].parse().unwrap(); let c0: f64 = a[5].parse().unwrap();
            let rs = [2usize,3,4,8,16,32]; let cs = [1.0, 0.95, 0.5, 0.333, 0.9883];
            let mut jobs = vec![]; let mut seed = 12345u64;
            let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 11) as f64 / (1u64 << 53) as f64 };
            let mut n = lo; while n <= hi { for _ in 0..2 { let r = rs[(rnd()*6.0) as usize]; let b = 2.0 + 18.0*rnd(); let c = if rnd() < 0.5 { cs[(rnd()*5.0) as usize] } else { 0.3 + 0.7*rnd() }; jobs.push((n,r,b,c)); } n += stride; }
            let t = Instant::now();
            let jobs = std::sync::Mutex::new(jobs.into_iter());
            let out = std::sync::Mutex::new(vec![]);
            std::thread::scope(|sc| { for _ in 0..44 { sc.spawn(|| loop {
                let job = jobs.lock().unwrap().next(); let Some((n,r,b,c)) = job else { break };
                let (h, d) = lin(n, r, b, c);
                let res = minphase::minimum_phase(&h, &minimum_phase_contract(&d));
                let w = res.as_ref().ok().map(|m| outside(&m.kernel, c0, 256));
                out.lock().unwrap().push((n,r,b,c, res.map(|m| (m.method, m.check.unwrap())).map_err(|e| e.to_string()), w));
            }); } });
            let out = out.into_inner().unwrap();
            let (mut fails, mut bad, mut untrusted) = (0, 0, 0);
            for (n,r,b,c,res,w) in &out { match res { Ok(_) => { let (o, st) = w.unwrap(); if st >= 1.5 { untrusted += 1; if untrusted < 10 { println!("untrusted step {st:.2} N={n} r={r} b={b} c={c}"); } } else if o != 0 { bad += 1; println!("OUTSIDE {o} N={n} r={r} b={b} c={c}"); } }
                Err(e) => { fails += 1; if fails < 40 { println!("FAIL N={n} r={r} b={b} c={c}: {e}"); } } } }
            println!("WSWEEP {lo}..{hi} step {stride} c={c0}: {} designs, {fails} failures, {bad} with zeros outside e^(c/N), {untrusted} untrusted counts; {:.1}s", out.len(), t.elapsed().as_secs_f64());
        }
        "roots" => {
            let n = a[2].parse().unwrap(); let r = a[3].parse().unwrap(); let b = a[4].parse().unwrap(); let c = a[5].parse().unwrap();
            let (h, d) = lin(n, r, b, c);
            let t = Instant::now();
            let m = minphase::minimum_phase_by_roots(&h, &minimum_phase_contract(&d));
            let el = t.elapsed().as_secs_f64();
            match m { Ok(m) => println!("ROOTS N={n} t={el:.2}s refl={} {:?} w(0.05)={:?} w(0.25)={:?}", m.reflected_zeros, m.check, outside(&m.kernel, 0.05, 256), outside(&m.kernel, 0.25, 256)), Err(e) => println!("ROOTS ERR {e}") }
            let m = minphase::minimum_phase(&h, &minimum_phase_contract(&d)).unwrap();
            println!("PROD {:?} refl={} w(0.05)={:?} w(0.25)={:?}", m.method, m.reflected_zeros, outside(&m.kernel, 0.05, 256), outside(&m.kernel, 0.25, 256));
            println!("LIN w(2.0)={:?} w(0.05)={:?}", outside(&h, 2.0, 256), outside(&h, 0.05, 256));
        }
        "csweep" => {
            // cepstrum (alpha = 2/N, L = 8N) on short lengths: failures and zeros outside e^{c/N}
            let lo: usize = a[2].parse().unwrap(); let hi: usize = a[3].parse().unwrap(); let c0: f64 = a[4].parse().unwrap();
            let rs = [2usize,3,4,8,16,32]; let bs = [2.0, 3.5, 5.0, 8.0, 11.0, 14.0, 17.0, 18.5, 19.0, 19.817, 20.0]; let cs = [1.0, 0.95, 0.5, 0.333, 0.9883];
            let mut jobs = vec![]; for n in lo..=hi { for &r in &rs { for &b in &bs { for &c in &cs { jobs.push((n,r,b,c)); }}}}
            let jobs = std::sync::Mutex::new(jobs.into_iter());
            let out = std::sync::Mutex::new(vec![]);
            std::thread::scope(|sc| { for _ in 0..44 { sc.spawn(|| loop {
                let job = jobs.lock().unwrap().next(); let Some((n,r,b,c)) = job else { break };
                let (h, _) = lin(n, r, b, c);
                let m = minphase::minimum_phase_with(&h, minphase::fft_len_for(n), 2.0);
                let w = outside(&m.kernel, c0, 256);
                out.lock().unwrap().push((n, m.exact, w));
            }); } });
            let out = out.into_inner().unwrap();
            let mut maxbad = 0; let (mut f, mut o) = (0, 0);
            for (n, exact, (cnt, st)) in &out { if !exact { f += 1; maxbad = maxbad.max(*n); } else if *cnt > 0 && *st < 1.5 { o += 1; maxbad = maxbad.max(*n); } }
            println!("CSWEEP {lo}..{hi}: {} designs, {f} unwrap failures, {o} with zeros outside e^({c0}/N); largest bad N {maxbad}", out.len());
        }
        "summ" => {
            use audivane_protocol::{CutoffMode, RateTarget, UpsamplerConfig};
            let n = a[2].parse().unwrap(); let r: u32 = a[3].parse().unwrap(); let b = a[4].parse().unwrap(); let c = a[5].parse().unwrap();
            let phase = if a.get(6).map(|s| s == "min").unwrap_or(false) { FilterPhase::Minimum } else { FilterPhase::Linear };
            let cfg = UpsamplerConfig { target: RateTarget::Ratio(r), taps: n, phase, kaiser_beta: b, cutoff: c, cutoff_mode: CutoffMode::Fraction };
            let t = Instant::now();
            let m = measure_upsampler(&cfg, 48000).unwrap();
            let el = t.elapsed().as_secs_f64();
            let start = m.response.stopband_start_hz; let nyq = m.output_rate() as f64 / 2.0;
            // dense brute force: 64 points per 1/N over [start, nyq] (capped)
            let span = nyq - start; let pts = ((span / (m.output_rate() as f64 / n as f64)) * 64.0).min(4.0e6) as usize;
            let fs: Vec<f64> = (0..=pts).map(|i| start + span * i as f64 / pts as f64).collect();
            let mut best = f64::NEG_INFINITY; let mut at = 0.0;
            for chunk in fs.chunks(65536) { let v = m.magnitude_db_at(chunk); for (f, d) in chunk.iter().zip(v) { if d > best { best = d; at = *f; } } }
            // golden refine around `at`
            let step = span / pts as f64; let (mut lo, mut hi) = ((at - step).max(start), (at + step).min(nyq));
            for _ in 0..60 { let x1 = lo + (hi-lo)*0.381966; let x2 = lo + (hi-lo)*0.618034; let v = m.magnitude_db_at(&[x1, x2]); if v[0] > v[1] { hi = x2 } else { lo = x1 } }
            let g = m.magnitude_db_at(&[0.5*(lo+hi)])[0]; if g > best { best = g; at = 0.5*(lo+hi); }
            println!("SUMM N={n} r={r} b={b} c={c} {phase:?} t={el:.2}s start={start:.6} reported={:.6} dense_peak={best:.6} at {at:.4} Hz; reported-actual={:+.6} dB ({pts} pts)", -m.response.stopband_atten_db, -m.response.stopband_atten_db - best);
        }
        "mtime" => {
            use audivane_protocol::{CutoffMode, RateTarget, UpsamplerConfig};
            let n = a[2].parse().unwrap(); let r: u32 = a[3].parse().unwrap(); let b = a[4].parse().unwrap(); let c = a[5].parse().unwrap();
            let phase = if a.get(6).map(|s| s == "min").unwrap_or(false) { FilterPhase::Minimum } else { FilterPhase::Linear };
            let cfg = UpsamplerConfig { target: RateTarget::Ratio(r), taps: n, phase, kaiser_beta: b, cutoff: c, cutoff_mode: if c == 0.0 { CutoffMode::NyquistStop } else { CutoffMode::Fraction } };
            let t = Instant::now();
            let m = measure_upsampler(&cfg, 48000).unwrap();
            println!("MTIME N={n} r={r} b={b} c={c} {phase:?} t={:.2}s start={:.4} atten={:.6} {}", t.elapsed().as_secs_f64(), m.response.stopband_start_hz, m.response.stopband_atten_db, std::fs::read_to_string("/proc/self/status").unwrap().lines().find(|s| s.starts_with("VmHWM")).unwrap());
        }
        "dtime" => {
            let n = a[2].parse().unwrap(); let r = a[3].parse().unwrap(); let b = a[4].parse().unwrap(); let c = a[5].parse().unwrap();
            let t = Instant::now();
            let h = design_kernel(&UpsamplerDesign { ratio: r, taps: n, phase: FilterPhase::Minimum, kaiser_beta: b, cutoff: c });
            println!("DTIME N={n} t={:.2}s sum={} {}", t.elapsed().as_secs_f64(), h.iter().sum::<f64>(), std::fs::read_to_string("/proc/self/status").unwrap().lines().find(|s| s.starts_with("VmHWM")).unwrap());
        }
        "long" => {
            // long-length matrix, `threads` designs at a time
            let threads: usize = a[2].parse().unwrap();
            let mut jobs = vec![];
            let ns: Vec<usize> = std::env::var("NS").map(|v| v.split(',').map(|x| x.parse().unwrap()).collect()).unwrap_or(vec![65_535usize, 65_536, 131_071, 262_144, 524_287, 524_288, 1_000_001, 1_048_576, 2_000_001, 4_194_303, 4_194_304]);
            for &n in &ns { for &r in &[2usize, 3, 4, 8, 16, 32] { for &b in &[2.0, 5.0, 8.0, 11.0, 14.0, 17.0, 20.0] {
                let mut cs = vec![1.0, 0.95, 0.5]; if let Ok(c) = nyquist_stop_check(n, r, b) { cs.push(c); }
                for c in cs { jobs.push((n, r, b, c)); } } } }
            let total = jobs.len();
            let jobs = std::sync::Mutex::new(jobs.into_iter());
            let t = Instant::now();
            std::thread::scope(|sc| { for _ in 0..threads { sc.spawn(|| loop {
                let job = jobs.lock().unwrap().next(); let Some((n,r,b,c)) = job else { break };
                let (h, d) = lin(n, r, b, c);
                let t1 = Instant::now();
                let res = minphase::minimum_phase(&h, &minimum_phase_contract(&d));
                let el = t1.elapsed().as_secs_f64();
                match res { Ok(m) => { let ch = m.check.unwrap(); println!("OK N={n} r={r} b={b} c={c:.6} R={} {el:.1}s pb={:.2e} up={:.2e} stop={:.1} (bound {:.1}) tail={:.1e}", m.radius_taps, ch.passband_db, ch.upper_db, ch.stopband_error_db, -(b/0.1102+8.7+25.0), m.tail); }
                    Err(e) => println!("FAIL N={n} r={r} b={b} c={c:.6} {el:.1}s: {e}") }
            }); } });
            println!("LONG {total} designs in {:.0}s", t.elapsed().as_secs_f64());
        }
        "radius" => {
            use audivane_dsp_core::{Complex64, fft::RealFftPair};
            let n = a[2].parse().unwrap(); let r = a[3].parse().unwrap(); let b: f64 = a[4].parse().unwrap(); let c = if a[5] == "ns" { nyquist_stop_check(n, r, b).unwrap() } else { a[5].parse().unwrap() };
            let radii: Vec<f64> = a[6].split(',').map(|x| x.parse().unwrap()).collect();
            let mult: usize = a.get(7).map(|x| x.parse().unwrap()).unwrap_or(8);
            let (h, d) = lin(n, r, b, c);
            let edge = minimum_phase_contract(&d).stopband_edge;
            let l = (n * 8).next_power_of_two();
            let mut fft = RealFftPair::new(l);
            let mut spec = vec![Complex64::default(); fft.spec_len()];
            fft.forward_padded_complex(&h, &mut spec);
            let ma: Vec<f64> = spec.iter().map(|x| x.norm()).collect();
            for &rad in &radii {
                let t = Instant::now();
                let m = minphase::minimum_phase_with(&h, (n * mult).next_power_of_two(), rad);
                let el = t.elapsed().as_secs_f64();
                if !m.exact { println!("R={rad} unwrap failed"); continue; }
                fft.forward_padded_complex(&m.kernel, &mut spec);
                let dc = ma[0];
                let (mut pb, mut up, mut se) = (0f64, 0f64, 0f64);
                let mut se_at = 0usize;
                for (k, (x, y)) in ma.iter().zip(&spec).enumerate() { let y = y.norm();
                    let ld = 20.0*(x/dc).log10(); let dd = (20.0*(y/x).log10()).abs();
                    if ld >= -1.0 { pb = pb.max(dd); } if ld >= -100.0 { up = up.max(dd); }
                    if k as f64 / l as f64 >= edge && (x-y).abs()/dc > se { se = (x-y).abs()/dc; se_at = k; } }
                // error profile: worst per 1/64 of the band above the edge
                if std::env::var_os("PROFILE").is_some() {
                    let first = (edge * l as f64).ceil() as usize; let seg = (ma.len() - first) / 64;
                    let prof: Vec<String> = (0..64).map(|i| { let r0 = first + i*seg; let r1 = (r0+seg).min(ma.len()); let e = (r0..r1).map(|k| (ma[k]-spec[k].norm()).abs()/dc).fold(0f64, f64::max); let lv = (r0..r1).map(|k| ma[k]/dc).fold(0f64, f64::max); format!("{:.3}:{:.0}/{:.0}", r0 as f64 / l as f64, 20.0*e.log10(), 20.0*lv.log10()) }).collect();
                    println!("  profile (f: err/level dB) {}", prof.join(" "));
                }
                if std::env::var_os("AROUND").is_some() {
                    let lo = se_at.saturating_sub(400);
                    for k in (lo..(se_at + 40).min(ma.len())).step_by(10) { println!("    k={k} f={:.8} lin={:.2} min={:.2} err={:.1}", k as f64 / l as f64, 20.0*(ma[k]/dc).log10(), 20.0*(spec[k].norm()/dc).log10(), 20.0*((ma[k]-spec[k].norm()).abs()/dc).log10()); }
                }
                println!("  worst stopband error at f={:.6} (lin level {:.1} dB, min level {:.1} dB)", se_at as f64 / l as f64, 20.0*(ma[se_at]/dc).log10(), 20.0*(spec[se_at].norm()/dc).log10());
                println!("N={n} r={r} b={b} c={c} R={rad} L={mult}N t={el:.1}s pb={pb:.2e} up={up:.2e} stop_err={:.1} dB (bound {:.1}) step={:.3} tail={:.1e}", 20.0*se.log10(), -(b/0.1102+8.7+25.0), m.max_phase_step, m.tail);
            }
        }
        "dump" => {
            let n = a[2].parse().unwrap(); let r = a[3].parse().unwrap(); let b = a[4].parse().unwrap(); let c = a[5].parse().unwrap();
            let (h, _) = lin(n, r, b, c);
            for v in &h { println!("{v:.17e}"); }
        }
        _ => panic!(),
    }
}
