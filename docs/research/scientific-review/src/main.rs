use audivane_dsp_core::{
    Complex64 as C,
    fft::RealFftPair,
    resample::{UpsamplerDesign, design_kernel, dft_magnitudes, nyquist_stop_cutoff},
};
use audivane_protocol::FilterPhase;
use std::f64::consts::PI;
unsafe extern "C" {
    fn reference(h: *const f64, n: usize, cycles: f64, out: *mut f64);
    fn root(h: *const f64, n: usize, angle: f64, out: *mut f64);
}
fn db(x: f64) -> f64 {
    20. * x.max(1e-300).log10()
}
fn cepstral(h: &[f64], ratio: usize, floor: f64, pad: usize) -> Vec<f64> {
    let nfft = (h.len() * pad).next_power_of_two().max(64);
    let mut fft = RealFftPair::new(nfft);
    let mut spec = vec![C::default(); fft.spec_len()];
    fft.forward_padded_complex(h, &mut spec);
    let max = spec.iter().map(|c| c.norm()).fold(0., f64::max);
    for z in &mut spec {
        *z = C::new(z.norm().max(max * 10f64.powf(-floor / 20.)).ln(), 0.);
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
    let tail_e: f64 = cep[h.len()..].iter().map(|x| x * x).sum();
    let all_e: f64 = cep.iter().map(|x| x * x).sum();
    let tail_l1: f64 = cep[h.len()..].iter().map(|x| x.abs()).sum();
    eprintln!(
        "cep floor={floor:.4} pad={pad} tail_energy={:.2}dB tail_l1={:.3e}",
        db((tail_e / all_e).sqrt()),
        tail_l1 / ratio as f64
    );
    cep.truncate(h.len());
    let scale = ratio as f64 / cep.iter().sum::<f64>();
    for v in &mut cep {
        *v *= scale;
    }
    cep
}
fn metrics(h: &[f64], ratio: usize) -> (usize, usize, f64, f64) {
    let total: f64 = h.iter().map(|v| v * v).sum();
    let mut sum = 0.;
    let mut p50 = 0;
    let mut p99 = 0;
    for (i, x) in h.iter().enumerate() {
        sum += x * x;
        if sum < total * 0.5 {
            p50 = i + 1;
        }
        if sum < total * 0.99 {
            p99 = i + 1;
        }
    }
    let dc = h.iter().sum::<f64>();
    let gd = h.iter().enumerate().map(|(n, x)| n as f64 * x).sum::<f64>() / dc;
    let l1 = h.iter().map(|x| x.abs()).sum::<f64>() / ratio as f64;
    (p50, p99, gd, l1)
}
fn compare(lin: &[f64], h: &[f64], ratio: usize, cut: f64, beta: f64, name: &str) {
    let pad = std::env::var("REVIEW_FFT_PAD")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(8);
    let nfft = (lin.len() * pad).next_power_of_two();
    let mut fft = RealFftPair::new(nfft);
    let mut a = vec![C::default(); fft.spec_len()];
    let mut b = a.clone();
    fft.forward_padded_complex(lin, &mut a);
    fft.forward_padded_complex(h, &mut b);
    let fc = cut / (2. * ratio as f64);
    let aa = beta / 0.1102 + 8.7;
    let half_tw = (aa - 7.95) / (28.72 * (lin.len() - 1) as f64);
    let mut pb = 0f64;
    let mut tr = 0f64;
    let mut abs = 0f64;
    let mut stop_a = 0f64;
    let mut stop_b = 0f64;
    let mut ny_a = 0f64;
    let mut ny_b = 0f64;
    let mut gdlo = f64::INFINITY;
    let mut gdhi = f64::NEG_INFINITY;
    let mut mindiff = 0f64;
    let mut minat = 0f64;
    let mut winding = 0f64;
    let mut maxstep = 0f64;
    let mut logmean = 0f64;
    let mut far_pb = 0f64;
    for (k, (x, y)) in a.iter().zip(&b).enumerate() {
        if k > 0 {
            let dp = (b[k] * b[k - 1].conj()).arg();
            winding += dp;
            maxstep = maxstep.max(dp.abs());
        }
        let weight = if k == 0 || k == b.len() - 1 { 0.5 } else { 1. };
        logmean += weight * y.norm().max(1e-300).ln();
        let f = k as f64 / nfft as f64;
        let x = x.norm() / ratio as f64;
        let y = y.norm() / ratio as f64;
        if f <= fc - half_tw * 1.2 {
            pb = pb.max((db(x) - db(y)).abs());
        }
        if f <= fc * 0.9 {
            far_pb = far_pb.max((db(x) - db(y)).abs());
        }
        if f >= fc - half_tw * 1.2 && f <= fc + half_tw * 1.2 {
            tr = tr.max((x - y).abs());
        }
        abs = abs.max((x - y).abs());
        if f >= fc + half_tw * 1.2 {
            stop_a = stop_a.max(x);
            stop_b = stop_b.max(y);
        }
        if f >= 0.5 / ratio as f64 {
            ny_a = ny_a.max(x);
            ny_b = ny_b.max(y);
        }
        if f <= fc - half_tw * 1.2 && k > 0 && k + 1 < b.len() {
            let delta = (b[k + 1] * b[k - 1].conj()).arg();
            let gd = -delta * nfft as f64 / (4. * PI);
            gdlo = gdlo.min(gd);
            gdhi = gdhi.max(gd);
        }
        if f >= fc + half_tw * 1.2 && x > 1e-14 {
            let delta = (db(x) - db(y)).abs();
            if delta > mindiff {
                mindiff = delta;
                minat = f;
            }
        }
    }
    let energy = metrics(h, ratio);
    if std::env::var("REVIEW_ROOTS").is_ok() {
        let mut candidates: Vec<(usize, f64)> = (1..b.len())
            .map(|k| (k, (b[k] * b[k - 1].conj()).arg()))
            .filter(|(_, dp)| *dp < -0.03)
            .collect();
        candidates.sort_by(|a, b| a.1.total_cmp(&b.1));
        let mut roots: Vec<C> = vec![];
        for (k, _) in candidates.into_iter().take(80) {
            let mut out = [0.; 5];
            unsafe {
                root(
                    h.as_ptr(),
                    h.len(),
                    2. * PI * k as f64 / nfft as f64,
                    out.as_mut_ptr(),
                );
            }
            let z = C::new(out[0], out[1]);
            if out[2] > 1e-10 && out[3] < 1e-14 && !roots.iter().any(|v| (*v - z).norm() < 1e-7) {
                roots.push(z);
                println!(
                    "EXTERNAL_ROOT {name} beta={beta} z={z:?} radius_minus_1={:.12e} residual={:.3e} iters={}",
                    out[2],
                    out[3] / ratio as f64,
                    out[4]
                );
            }
        }
    }
    println!(
        "MINPHASE {name} winding_full={:.5} maxstep={maxstep:.5} Jensen_log_excess={:.9e} h0={:.9e} nyquist={:.4}",
        winding / PI,
        logmean / (b.len() - 1) as f64 - h[0].abs().ln(),
        h[0],
        db(b.last().unwrap().norm() / ratio as f64)
    );
    println!(
        "COMPARE N={} beta={beta} r={ratio} cut={cut:.9} {name} pb_diff_db={pb:.6e} far_pb_diff_db={far_pb:.6e} transition_abs={tr:.6e} all_abs={abs:.6e} stop_lin={:.4} stop_min={:.4} ny_lin={:.4} ny_min={:.4} sb_diff_max_db={mindiff:.2}@{minat:.8} E50={} E99={} gd_dc={:.5} gd_pb=[{gdlo:.4},{gdhi:.4}] l1={:.4}",
        h.len(),
        db(stop_a),
        db(stop_b),
        db(ny_a),
        db(ny_b),
        energy.0,
        energy.1,
        energy.2,
        energy.3
    );
}
fn dft_complex(h: &[f64], c: f64, corrected: bool) -> C {
    let mut tw = vec![C::default(); 256];
    for (m, z) in tw.iter_mut().enumerate() {
        let a = c * m as f64;
        let (s, co) = (2. * PI * (a - a.floor())).sin_cos();
        *z = C::new(co, -s);
    }
    let mut out = C::default();
    for (b, chunk) in h.chunks(256).enumerate() {
        let mut sum = C::default();
        for (x, z) in chunk.iter().zip(&tw) {
            sum += *x * *z;
        }
        let n0 = (b * 256) as f64;
        let hi = c * n0;
        let lo = if corrected { c.mul_add(n0, -hi) } else { 0. };
        let (s, co) = (2. * PI * ((hi - hi.floor()) + lo)).sin_cos();
        out += sum * C::new(co, -s);
    }
    out
}
fn dft_check(h: &[f64], r: usize, cut: f64, beta: f64) {
    let fc = cut / (2. * r as f64);
    let aa = beta / 0.1102 + 8.7;
    let tw = (aa - 7.95) / (14.36 * (h.len() - 1) as f64);
    let mut freqs = vec![
        0.,
        0.25 * fc,
        0.7 * fc,
        fc - tw,
        fc,
        fc + tw,
        fc + 2. * tw,
        fc + 3. * tw,
        fc + 4. * tw,
        0.123456789,
        0.253456789,
        0.453456789,
        0.5,
    ];
    for i in 0..32 {
        freqs.push(fc + tw * (0.7 + 3.4 * i as f64 / 31.));
    }
    if std::env::var("REVIEW_DFT_GRID").is_ok() {
        for i in 0..256 {
            freqs.push(fc + tw + (0.5 - fc - tw) * ((i as f64 + 0.314159) / 256.));
        }
    }
    let prod = dft_magnitudes(h, 1., &freqs);
    let mut worst_old = 0f64;
    let mut worst_new = 0f64;
    let mut worst_api = 0f64;
    let mut stop_new = 0f64;
    for (i, &c) in freqs.iter().enumerate() {
        let mut rr = [0.; 2];
        unsafe {
            reference(h.as_ptr(), h.len(), c, rr.as_mut_ptr());
        }
        let refz = C::new(rr[0], rr[1]);
        let old = dft_complex(h, c, false);
        let new = dft_complex(h, c, true);
        let eold = (old - refz).norm() / r as f64;
        let enew = (new - refz).norm() / r as f64;
        worst_old = worst_old.max(eold);
        worst_new = worst_new.max(enew);
        worst_api = worst_api.max((prod[i] - refz.norm()).abs() / r as f64);
        if c >= fc + tw {
            stop_new = stop_new.max(enew);
        }
        if i < 13 {
            println!(
                "DFT c={c:.17} true_db={:.4} old_db={:.4} new_db={:.4} old_err={:.2} new_err={:.2}",
                db(refz.norm() / r as f64),
                db(old.norm() / r as f64),
                db(new.norm() / r as f64),
                db(eold),
                db(enew)
            );
        }
    }
    println!(
        "DFT_WORST N={} beta={beta} old_err={:.2} new_err={:.2} stop_new_err={:.2} api_mag_err={:.2}",
        h.len(),
        db(worst_old),
        db(worst_new),
        db(stop_new),
        db(worst_api)
    );
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("small");
    let n = args
        .get(2)
        .map(|s| s.parse().unwrap())
        .unwrap_or(16384usize);
    let betas: Vec<f64> = args
        .get(3)
        .map(|s| vec![s.parse().unwrap()])
        .unwrap_or(vec![8., 14., 20.]);
    let ratio = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(16usize);
    for beta in betas {
        let cut = if let Ok(c) = std::env::var("REVIEW_CUTOFF") {
            c.parse().unwrap()
        } else if mode == "fraction" {
            0.95
        } else if let Ok(k) = std::env::var("REVIEW_K") {
            let k: f64 = k.parse().unwrap();
            (1. - k * ratio as f64 * (beta / 0.1102 + 8.7 - 7.95) / (14.36 * (n - 1) as f64))
                .clamp(0.5, 1.)
        } else {
            nyquist_stop_cutoff(n, ratio, beta)
        };
        let d = UpsamplerDesign {
            ratio,
            taps: n,
            phase: FilterPhase::Linear,
            kaiser_beta: beta,
            cutoff: cut,
        };
        let lin = design_kernel(&d);
        if mode == "measure" {
            let cfg = audivane_protocol::UpsamplerConfig {
                target: audivane_protocol::RateTarget::Ratio(ratio as u32),
                taps: n,
                phase: FilterPhase::Minimum,
                kaiser_beta: beta,
                cutoff: 1.,
                cutoff_mode: audivane_protocol::CutoffMode::NyquistStop,
            };
            let measured = audivane_dsp_core::resample::measure_upsampler(&cfg, 48000).unwrap();
            println!(
                "MEASURE reported_stopband_start={:.9} reported_stopband_atten={:.9}",
                measured.response.stopband_start_hz, measured.response.stopband_atten_db
            );
            let h = design_kernel(&UpsamplerDesign {
                phase: FilterPhase::Minimum,
                ..d
            });
            for crossing in &measured.response.crossings {
                let c = crossing.freq_hz / (48000. * ratio as f64);
                let mut rr = [0.; 2];
                unsafe {
                    reference(h.as_ptr(), h.len(), c, rr.as_mut_ptr());
                }
                println!(
                    "CROSSING target={} hz={:.9} direct_kernel_db={:.9}",
                    crossing.level_db,
                    crossing.freq_hz,
                    db(C::new(rr[0], rr[1]).norm() / ratio as f64)
                );
            }
            let aa = beta / 0.1102 + 8.7;
            let tw = (aa - 7.95) / (14.36 * (n - 1) as f64);
            let fs: Vec<f64> = (0..=256)
                .map(|i| 24000. + 0.1 * tw * 48000. * ratio as f64 * i as f64 / 256.)
                .collect();
            let dbs = measured.magnitude_db_at(&fs);
            let (i, worst) = dbs
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .unwrap();
            println!("MEASURE actual_peak_db={worst:.9} hz={:.9}", fs[i]);
            continue;
        }
        if mode == "dft" {
            dft_check(&lin, ratio, cut, beta);
            continue;
        }
        if mode == "phase" {
            let min = design_kernel(&UpsamplerDesign {
                phase: FilterPhase::Minimum,
                ..d
            });
            compare(&lin, &min, ratio, cut, beta, "new");
            continue;
        }
        if mode == "dense" {
            let min = design_kernel(&UpsamplerDesign {
                phase: FilterPhase::Minimum,
                ..d
            });
            let aa = beta / 0.1102 + 8.7;
            let tw = (aa - 7.95) / (14.36 * (n - 1) as f64);
            let ny = 0.5 / ratio as f64;
            let points = std::env::var("REVIEW_POINTS")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(4096);
            let span = std::env::var("REVIEW_SPAN")
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(4.);
            let fs: Vec<f64> = (0..=points)
                .map(|i| ny + span * tw * i as f64 / points as f64)
                .filter(|f| *f <= 0.5)
                .collect();
            for (name, h) in [("lin", &lin), ("min", &min)] {
                let mags = dft_magnitudes(h, 1., &fs);
                let (idx, max) = mags
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.total_cmp(b.1))
                    .unwrap();
                println!(
                    "DENSE N={n} beta={beta} ratio={ratio} {name} worst={:.6} at_offset_widths={:.9} target={:.6}",
                    db(*max / ratio as f64),
                    (fs[idx] - ny) / tw,
                    -aa
                );
            }
            continue;
        }
        for (name, floor, pad) in [
            ("new", (beta / 0.1102 + 8.7 + 20.).max(80.), 4),
            ("old", 180., 4),
        ] {
            let min = if name == "new" {
                design_kernel(&UpsamplerDesign {
                    phase: FilterPhase::Minimum,
                    ..d
                })
            } else {
                cepstral(&lin, ratio, floor, pad)
            };
            compare(&lin, &min, ratio, cut, beta, name);
            if mode == "big" {
                dft_check(&min, ratio, cut, beta);
            }
        }
        if mode == "small" || mode == "fraction" {
            for pad in [16, 64] {
                let min = cepstral(&lin, ratio, (beta / 0.1102 + 8.7 + 20.).max(80.), pad);
                compare(&lin, &min, ratio, cut, beta, &format!("new_pad{pad}"));
            }
        }
    }
}
