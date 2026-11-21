//! Real-time factor of the rational resampler (direct polyphase).
//!
//! ```text
//! cargo run --release -p audivane-dsp-core --example bench_rational -- \
//!     [in_rate] [out_rate] [taps_per_branch] [channels] [seconds]
//! ```
//!
//! Defaults: 44100 48000 4095 2 10. Prints the time per second of audio
//! (the real-time factor on one core) and the multiply-add rate.

use audivane_dsp_core::rational::RationalResampler;
use audivane_protocol::RateRatio;
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize, d: u64| a.get(i).map_or(d, |s| s.parse().expect("number"));
    let fin = arg(0, 44_100) as u32;
    let fout = arg(1, 48_000) as u32;
    let taps = arg(2, 4095) as usize;
    let channels = arg(3, 2) as usize;
    let secs = arg(4, 10) as usize;
    let ratio = RateRatio::from_rates(fin, fout);
    let l = ratio.up as usize;
    let n = l * taps;
    let kernel: Vec<f64> = (0..n)
        .map(|i| {
            let x = (i as f64 - n as f64 / 2.0) / l as f64;
            if x == 0.0 {
                1.0
            } else {
                (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
            }
        })
        .collect();
    let block = 1024;
    let t0 = Instant::now();
    let mut rs = RationalResampler::from_kernel(&kernel, ratio, n as f64 / 2.0, channels, block);
    let build_s = t0.elapsed().as_secs_f64();
    let input: Vec<Vec<f64>> = (0..channels)
        .map(|c| {
            (0..block)
                .map(|i| ((i * (c + 3)) as f64 * 0.01).sin() * 0.5)
                .collect()
        })
        .collect();
    let mut out = vec![vec![0.0; ratio.max_frames_out(block)]; channels];
    let total_in = fin as usize * secs;
    let mut done = 0;
    let mut produced = 0usize;
    let t = Instant::now();
    while done < total_in {
        let k = block.min(total_in - done);
        let n_out = rs.output_frames(k);
        let ins: Vec<&[f64]> = input.iter().map(|c| &c[..k]).collect();
        let mut outs: Vec<&mut [f64]> = out.iter_mut().map(|c| &mut c[..n_out]).collect();
        rs.process(&ins, &mut outs);
        produced += n_out;
        done += k;
    }
    let el = t.elapsed().as_secs_f64();
    let macs = produced as f64 * taps as f64 * channels as f64;
    println!(
        "{fin} -> {fout} ({ratio}), {taps} taps/branch ({n} taps, {:.1} MB), {channels} ch: \
         build {build_s:.3} s, RTF {:.4} ({:.2} ms per s of audio), {:.2} GMAC/s",
        (n * 8) as f64 / 1e6,
        el / secs as f64,
        1000.0 * el / secs as f64,
        macs / el / 1e9
    );
}
