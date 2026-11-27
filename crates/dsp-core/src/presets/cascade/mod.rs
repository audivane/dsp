//! x2/x3 stage cascades (the `cascade_*` presets).
//!
//! The ratio is split into stages from the input side: 3 while what is
//! left is divisible by 3, else 2 (x8 = 2·2·2, x12 = 3·2·2, x6 = 3·2).
//! Every stage zero-stuffs by its factor f (each sample followed by f - 1
//! zeros), filters at the stage's output rate and multiplies by f. All
//! stages of one preset with the same factor use the same filter.
//!
//! Stage filters:
//! - `cascade_fir`: a 255-tap windowed sinc, cutoff 0.476755 of the
//!   stage output Nyquist for x2 (0.95351 of its input Nyquist) and a
//!   third of 0.95351 for x3, Dolph-Chebyshev window with -144.5 dB
//!   sidelobes (r = 2^-24), unit DC gain (`design::fir_prototype`).
//! - `cascade_fir_mp`: that prototype made minimum phase by the real
//!   cepstrum on 4096 points, 255 taps kept (`design::cepstral`, p = 0).
//! - `cascade_fir_asym`: the same transform with the phase blended 80 %
//!   toward a pure delay of 819 samples, then cut around its peak (289
//!   taps for x2).
//! - `cascade_fft`: a 512-tap frequency-sampled kernel (an ideal brick
//!   wall at about 0.95 of the stage input Nyquist, Kaiser beta 7 pi;
//!   `design::fft_stage_kernel`).
//! - `cascade_elliptic`, `cascade_inverse_chebyshev`: one recursive filter
//!   per stage (`iir`).
//!
//! The FIR-type stages run as small polyphase FFT block convolutions
//! (`stage`), which give the stage-by-stage direct convolution to far
//! below one 32-bit LSB (see the tests). The recursive cascades run sample by sample.

mod design;
mod iir;
mod stage;

pub use iir::IirCascade;

use super::{FilterPreset, PresetError, PresetKernel, UpsamplerRuntime};
use audivane_protocol::RateRatio;

/// Stage factors from the input side (`ratio` already checked by
/// `super::check_ratio`).
pub fn stage_factors(ratio: usize) -> Vec<usize> {
    let mut r = ratio;
    let mut out = Vec::new();
    while r > 1 {
        let f = if r.is_multiple_of(3) { 3 } else { 2 };
        out.push(f);
        r /= f;
    }
    out
}

fn is_recursive(p: FilterPreset) -> bool {
    matches!(
        p,
        FilterPreset::CascadeElliptic | FilterPreset::CascadeInverseChebyshev
    )
}

/// The kernel of one stage of a FIR-type cascade at the stage output rate,
/// with unit DC gain (the stage gain f is not included).
pub fn stage_kernel(p: FilterPreset, f: usize) -> Result<Vec<f64>, PresetError> {
    use FilterPreset::*;
    if f != 2 && f != 3 {
        return Err(PresetError::Design(format!("no x{f} stage")));
    }
    Ok(match p {
        CascadeFir => design::fir_prototype(f),
        CascadeFirMp => design::cepstral(&design::fir_prototype(f), 0.0, design::FIR_LEN_SETTING),
        CascadeFirAsym => design::cepstral(
            &design::fir_prototype(f),
            design::ASYM_P,
            design::FIR_LEN_SETTING,
        ),
        CascadeFft => design::fft_stage_kernel(f),
        _ => {
            return Err(PresetError::Design(format!(
                "{} has no FIR stage kernel",
                p.name()
            )));
        }
    })
}

/// The whole FIR-type cascade as one kernel at the output rate: its
/// response to a unit impulse, stage by stage (zero-stuff, direct
/// convolution in double, times f), without the trailing zeros. `delay`
/// is the position of its largest value.
pub fn cascade_kernel(p: FilterPreset, ratio: usize) -> Result<PresetKernel, PresetError> {
    super::check_ratio(p, ratio)?;
    let factors = stage_factors(ratio);
    let mut x = vec![1.0];
    let mut kernels: Vec<(usize, Vec<f64>)> = Vec::new();
    for &f in &factors {
        if !kernels.iter().any(|(g, _)| *g == f) {
            kernels.push((f, stage_kernel(p, f)?));
        }
        let h = &kernels.iter().find(|(g, _)| *g == f).unwrap().1;
        x = stage_direct(&x, f, h);
    }
    while x.len() > 1 && x[x.len() - 1] == 0.0 {
        x.pop();
    }
    let delay = x
        .iter()
        .enumerate()
        .fold((0, 0.0f64), |(bi, bv), (i, &v)| {
            if v.abs() > bv { (i, v.abs()) } else { (bi, bv) }
        })
        .0;
    Ok(PresetKernel { kernel: x, delay })
}

/// One FIR stage on a whole signal: zero-stuff `x` by `f` (keeping the
/// trailing zero slots), full convolution with `h`, times `f`.
fn stage_direct(x: &[f64], f: usize, h: &[f64]) -> Vec<f64> {
    let n = x.len() * f;
    let mut y = vec![0.0; n + h.len() - 1];
    for (i, yi) in y.iter_mut().enumerate() {
        let mut acc = 0.0;
        // u[j] = x[j / f] where j % f == 0; y[i] = sum_j u[j] h[i - j]
        let j_lo = (i + 1).saturating_sub(h.len());
        let j_hi = i.min(n - 1);
        let mut j = j_lo.div_ceil(f) * f;
        while j <= j_hi {
            acc += x[j / f] * h[i - j];
            j += f;
        }
        *yi = acc * f as f64;
    }
    y
}

/// Build the runtime for `p`.
pub fn build(
    p: FilterPreset,
    _source_rate: u32,
    ratio: usize,
    channels: usize,
    max_block: usize,
) -> Result<Box<dyn UpsamplerRuntime>, PresetError> {
    super::check_ratio(p, ratio)?;
    if is_recursive(p) {
        return Ok(Box::new(IirCascade::new(p, ratio, channels, max_block)));
    }
    Ok(Box::new(FirCascade::new(p, ratio, channels, max_block)?))
}

/// A FIR-type cascade: one polyphase FFT stage engine per stage and
/// channel ([`stage::FirStage`]), each with a small block (the next power
/// of two of its branch length). The rounding of a block convolution
/// grows with the block, and these small exact-spectrum blocks come out
/// at or below the error of a direct convolution in double; folding the
/// stages into one long kernel on a large block costs several times
/// more.
struct FirCascade {
    ratio: usize,
    channels: usize,
    max_block: usize,
    /// Product of the factors up to and including each stage.
    mults: Vec<usize>,
    /// `[channel][stage]`.
    stages: Vec<Vec<stage::FirStage>>,
    /// `[channel][stage]`: each stage's output except the last one's.
    bufs: Vec<Vec<Vec<f64>>>,
    latency: usize,
    span: usize,
}

impl FirCascade {
    fn new(
        p: FilterPreset,
        ratio: usize,
        channels: usize,
        max_block: usize,
    ) -> Result<Self, PresetError> {
        let factors = stage_factors(ratio);
        let k = cascade_kernel(p, ratio)?;
        let kernels: Vec<Vec<f64>> = factors
            .iter()
            .map(|&f| stage_kernel(p, f))
            .collect::<Result<_, _>>()?;
        let mut mults = Vec::new();
        let mut mult = 1;
        for &f in &factors {
            mult *= f;
            mults.push(mult);
        }
        let make = || -> Vec<stage::FirStage> {
            factors
                .iter()
                .zip(&kernels)
                .map(|(&f, h)| stage::FirStage::new(h, f))
                .collect()
        };
        // Block delay at the output rate: each stage's at its own output
        // rate, stretched by the stages after it.
        let block_latency: usize = make()
            .iter()
            .zip(&mults)
            .map(|(s, &m)| s.block_latency() * (ratio / m))
            .sum();
        let stages = (0..channels).map(|_| make()).collect();
        let bufs = (0..channels)
            .map(|_| {
                mults[..mults.len() - 1]
                    .iter()
                    .map(|&m| vec![0.0; max_block * m])
                    .collect()
            })
            .collect();
        Ok(Self {
            ratio,
            channels,
            max_block,
            mults,
            stages,
            bufs,
            latency: block_latency + k.delay,
            span: block_latency + k.kernel.len(),
        })
    }
}

impl UpsamplerRuntime for FirCascade {
    fn ratio(&self) -> RateRatio {
        RateRatio::integer(self.ratio as u32)
    }

    fn process(&mut self, input: &[&[f64]], output: &mut [&mut [f64]]) {
        assert_eq!(input.len(), self.channels, "input channel count mismatch");
        assert_eq!(output.len(), self.channels, "output channel count mismatch");
        if self.channels == 0 {
            return;
        }
        let n_in = input[0].len();
        assert!(
            n_in <= self.max_block,
            "input frame count {n_in} exceeds max_block {}",
            self.max_block
        );
        let last = self.mults.len() - 1;
        for c in 0..self.channels {
            assert_eq!(input[c].len(), n_in, "inconsistent input slice length");
            assert_eq!(output[c].len(), n_in * self.ratio, "output length");
            let bufs = &mut self.bufs[c];
            for (s, st) in self.stages[c].iter_mut().enumerate() {
                let len_out = n_in * self.mults[s];
                let (done, rest) = bufs.split_at_mut(s.min(last));
                let src: &[f64] = if s == 0 {
                    input[c]
                } else {
                    &done[s - 1][..n_in * self.mults[s - 1]]
                };
                let dst: &mut [f64] = if s == last {
                    &mut *output[c]
                } else {
                    &mut rest[0][..len_out]
                };
                st.process(src, dst);
            }
        }
    }

    fn reset(&mut self) {
        for ch in &mut self.stages {
            for st in ch {
                st.reset();
            }
        }
    }

    fn latency_out_frames(&self) -> usize {
        self.latency
    }

    fn scale_output(&mut self, gain: f64) {
        // Folded into the last stage's kernel spectra; skipped at exactly 1
        // so the unity path stays untouched.
        if gain != 1.0 {
            for ch in &mut self.stages {
                ch.last_mut().unwrap().scale(gain);
            }
        }
    }

    fn impulse_span_out_frames(&self) -> usize {
        self.span
    }

    // Each channel has its own stages and buffers already.
    fn split_channels(&mut self) -> Option<Vec<Box<dyn UpsamplerRuntime>>> {
        let stages = std::mem::take(&mut self.stages);
        let bufs = std::mem::take(&mut self.bufs);
        self.channels = 0;
        Some(
            stages
                .into_iter()
                .zip(bufs)
                .map(|(st, b)| {
                    Box::new(FirCascade {
                        ratio: self.ratio,
                        channels: 1,
                        max_block: self.max_block,
                        mults: self.mults.clone(),
                        stages: vec![st],
                        bufs: vec![b],
                        latency: self.latency,
                        span: self.span,
                    }) as Box<dyn UpsamplerRuntime>
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FilterPreset::*;

    const ALL: [FilterPreset; 6] = [
        CascadeElliptic,
        CascadeInverseChebyshev,
        CascadeFir,
        CascadeFirAsym,
        CascadeFirMp,
        CascadeFft,
    ];

    fn lcg(seed: &mut u64) -> f64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((*seed >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }

    /// Run `rt` over `x` (plus enough zeros) in blocks of `block`.
    fn run(rt: &mut dyn UpsamplerRuntime, x: &[f64], extra: usize, block: usize) -> Vec<f64> {
        let r = rt.ratio().up as usize;
        let total = x.len() + extra;
        let mut out = vec![0.0; total * r];
        let mut buf = vec![0.0; block];
        let mut pos = 0;
        while pos < total {
            let n = block.min(total - pos);
            for (j, s) in buf[..n].iter_mut().enumerate() {
                *s = x.get(pos + j).copied().unwrap_or(0.0);
            }
            rt.process(&[&buf[..n]], &mut [&mut out[pos * r..(pos + n) * r]]);
            pos += n;
        }
        out
    }

    #[test]
    fn unity_dc_gain() {
        // Each output phase of the cascade kernel sums to about 1.
        for p in [CascadeFir, CascadeFirMp, CascadeFirAsym, CascadeFft] {
            for ratio in [2, 6, 8] {
                let k = cascade_kernel(p, ratio).unwrap().kernel;
                for r in 0..ratio {
                    let s: f64 = k.iter().skip(r).step_by(ratio).sum();
                    assert!(
                        (s - 1.0).abs() < 1e-3,
                        "{} x{ratio} phase {r}: {s}",
                        p.name()
                    );
                }
            }
        }
    }

    #[test]
    fn fft_engine_matches_direct_stages() {
        // The polyphase block runtime against stages run one by one
        // with direct convolution: far below 1 LSB.
        let mut seed = 7;
        let x: Vec<f64> = (0..3000).map(|_| 0.5 * lcg(&mut seed)).collect();
        for p in [CascadeFir, CascadeFirMp, CascadeFirAsym, CascadeFft] {
            for ratio in [8, 6, 16, 32] {
                let mut rt = build(p, 44100, ratio, 1, 256).unwrap();
                let k = cascade_kernel(p, ratio).unwrap();
                let lat_block = rt.impulse_span_out_frames() - k.kernel.len();
                assert_eq!(rt.latency_out_frames(), lat_block + k.delay);
                let y = run(rt.as_mut(), &x, 0, 256);
                let mut d = x.clone();
                for f in stage_factors(ratio) {
                    d = stage_direct(&d, f, &stage_kernel(p, f).unwrap());
                }
                let mut err = 0.0f64;
                for (i, &v) in y.iter().enumerate().skip(lat_block) {
                    err = err.max((v - d[i - lat_block]).abs());
                }
                assert!(err < 1e-14, "{} x{ratio}: {err}", p.name());
            }
        }
    }

    #[test]
    fn every_block_size_gives_the_same_output() {
        let mut seed = 11;
        let x: Vec<f64> = (0..2000).map(|_| 0.3 * lcg(&mut seed)).collect();
        for p in ALL {
            for ratio in [2, 8, 12] {
                let mut a = build(p, 44100, ratio, 1, 512).unwrap();
                let mut b = build(p, 44100, ratio, 1, 512).unwrap();
                let la = a.latency_out_frames();
                let ya = run(a.as_mut(), &x, 600, 512);
                let yb = run(b.as_mut(), &x, 600, 37);
                assert_eq!(la, b.latency_out_frames());
                if is_recursive(p) {
                    assert_eq!(ya, yb, "{} x{ratio}", p.name());
                } else {
                    // Same block engine whatever the call size.
                    let e = ya
                        .iter()
                        .zip(&yb)
                        .fold(0.0f64, |m, (u, v)| m.max((u - v).abs()));
                    assert!(e < 1e-15, "{} x{ratio}: {e}", p.name());
                }
            }
        }
    }
}
