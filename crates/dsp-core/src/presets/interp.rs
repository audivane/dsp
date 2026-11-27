//! Short interpolators (`nos`, `cosine`, `catmull_rom`, `table_sinc`).

use audivane_protocol::RateRatio;
use std::f64::consts::PI;

use super::{FilterPreset, PresetError, UpsamplerRuntime, check_ratio};
use crate::design::paths::fir::windows;

const TABLE_LEN: usize = 128_000;
const TABLE_CENTRE: usize = 64_000;
const TABLE_PER_SAMPLE: usize = 1_000;
const KAISER_BETA: f64 = 14.964_542_65;
const TAPS: usize = 127;

/// Build the runtime for `p`.
pub fn build(
    p: FilterPreset,
    _source_rate: u32,
    ratio: usize,
    channels: usize,
    _max_block: usize,
) -> Result<Box<dyn UpsamplerRuntime>, PresetError> {
    check_ratio(p, ratio)?;
    match p {
        FilterPreset::Nos => Ok(Box::new(Nos::new(ratio, channels))),
        FilterPreset::Cosine => Ok(Box::new(PolyInterp::new(PolyMode::Cosine, ratio, channels))),
        FilterPreset::CatmullRom => Ok(Box::new(PolyInterp::new(
            PolyMode::CatmullRom,
            ratio,
            channels,
        ))),
        FilterPreset::TableSinc => Ok(Box::new(TableSinc::new(ratio, channels))),
        _ => Err(PresetError::NotImplemented(p)),
    }
}

/// Zero-order hold (non-oversampling sample hold): repeats each input sample
/// `ratio` times with no reconstruction filter.
///
/// The causal boxcar impulse `[1; ratio]` is symmetric (linear phase) with
/// exact group delay `(ratio - 1) / 2` output frames (approaching half an
/// input sample as `ratio` grows).
#[derive(Clone)]
struct Nos {
    ratio: usize,
    channels: usize,
    gain: f64,
}

impl Nos {
    fn new(ratio: usize, channels: usize) -> Self {
        crate::test_support::record("nos", || crate::test_support::words(&[ratio as u64], &[]));
        Self {
            ratio,
            channels,
            gain: 1.0,
        }
    }
}

impl UpsamplerRuntime for Nos {
    fn ratio(&self) -> RateRatio {
        RateRatio::integer(self.ratio as u32)
    }

    fn process(&mut self, input: &[&[f64]], output: &mut [&mut [f64]]) {
        assert_eq!(input.len(), self.channels, "input channel mismatch");
        assert_eq!(output.len(), self.channels, "output channel mismatch");
        let r = self.ratio;
        let gain = self.gain;

        for (&inp, out) in input.iter().zip(output.iter_mut()) {
            assert_eq!(out.len(), inp.len() * r, "output length mismatch");
            if gain == 1.0 {
                for (i, &x) in inp.iter().enumerate() {
                    out[i * r..(i + 1) * r].fill(x);
                }
            } else {
                for (i, &x) in inp.iter().enumerate() {
                    out[i * r..(i + 1) * r].fill(x * gain);
                }
            }
        }
    }

    fn reset(&mut self) {}

    fn latency_out_frames(&self) -> usize {
        self.ratio / 2
    }

    fn latency_out_exact(&self) -> f64 {
        (self.ratio - 1) as f64 * 0.5
    }

    fn scale_output(&mut self, gain: f64) {
        self.gain *= gain;
    }

    fn split_channels(&mut self) -> Option<Vec<Box<dyn UpsamplerRuntime>>> {
        let channels = std::mem::take(&mut self.channels);
        Some(
            (0..channels)
                .map(|_| {
                    Box::new(Nos {
                        ratio: self.ratio,
                        channels: 1,
                        gain: self.gain,
                    }) as Box<dyn UpsamplerRuntime>
                })
                .collect(),
        )
    }

    fn impulse_span_out_frames(&self) -> usize {
        self.ratio
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PolyMode {
    Cosine,
    CatmullRom,
}

/// 4-point history interpolator (raised-cosine or Catmull-Rom cubic Hermite).
///
/// Design choice: emits exactly `ratio` outputs per input sample at exact
/// phases `mu = k / ratio` (`k = 1..=ratio`), rather than accumulating `1 / ratio`
/// in `f64` (which can drift and emit an extra sample at non-power-of-two
/// ratios). Identical at power-of-two ratios.
#[derive(Clone)]
struct PolyInterp {
    mode: PolyMode,
    ratio: usize,
    gain: f64,
    /// Per-phase precomputed weights:
    /// - Cosine: `[w1, w2, 0.0, 0.0]`
    /// - CatmullRom: `[w1, w2, wm0, wm1]`
    phases: Vec<[f64; 4]>,
    history: Vec<[f64; 4]>,
}

impl PolyInterp {
    fn new(mode: PolyMode, ratio: usize, channels: usize) -> Self {
        let phases: Vec<[f64; 4]> = (1..=ratio)
            .map(|k| {
                let mu = k as f64 / ratio as f64;
                match mode {
                    PolyMode::Cosine => {
                        let c = (PI * mu).cos();
                        [0.5 * (1.0 + c), 0.5 * (1.0 - c), 0.0, 0.0]
                    }
                    PolyMode::CatmullRom => {
                        let mu2 = mu * mu;
                        let mu3 = mu2 * mu;
                        [
                            1.0 - 3.0 * mu2 + 2.0 * mu3,
                            3.0 * mu2 - 2.0 * mu3,
                            mu3 - 2.0 * mu2 + mu,
                            mu3 - mu2,
                        ]
                    }
                }
            })
            .collect();
        crate::test_support::record("poly_interp", || {
            crate::test_support::words(&[mode as u64, ratio as u64], &phases.concat())
        });
        Self {
            mode,
            ratio,
            gain: 1.0,
            phases,
            history: vec![[0.0; 4]; channels],
        }
    }
}

impl UpsamplerRuntime for PolyInterp {
    fn ratio(&self) -> RateRatio {
        RateRatio::integer(self.ratio as u32)
    }

    fn process(&mut self, input: &[&[f64]], output: &mut [&mut [f64]]) {
        assert_eq!(input.len(), self.history.len(), "input channel mismatch");
        assert_eq!(output.len(), self.history.len(), "output channel mismatch");
        let r = self.ratio;
        let gain = self.gain;

        for (c, (&inp, out)) in input.iter().zip(output.iter_mut()).enumerate() {
            assert_eq!(out.len(), inp.len() * r, "output length mismatch");
            let h = &mut self.history[c];
            for (i, &x) in inp.iter().enumerate() {
                h[0] = h[1];
                h[1] = h[2];
                h[2] = h[3];
                h[3] = x;

                let y0 = h[0];
                let y1 = h[1];
                let y2 = h[2];
                let y3 = h[3];
                let out_slot = &mut out[i * r..(i + 1) * r];

                match self.mode {
                    PolyMode::Cosine => {
                        for (dst, w) in out_slot.iter_mut().zip(&self.phases) {
                            *dst = (w[0] * y1 + w[1] * y2) * gain;
                        }
                    }
                    PolyMode::CatmullRom => {
                        let m0 = (y1 - y0) + (y2 - y1);
                        let m1 = (y2 - y1) + (y3 - y2);
                        for (dst, w) in out_slot.iter_mut().zip(&self.phases) {
                            let y = w[0] * y1 + w[1] * y2 + 0.5 * (w[2] * m0 + w[3] * m1);
                            *dst = y * gain;
                        }
                    }
                }
            }
        }
    }

    fn reset(&mut self) {
        self.history.fill([0.0; 4]);
    }

    fn latency_out_frames(&self) -> usize {
        2 * self.ratio - 1
    }

    fn scale_output(&mut self, gain: f64) {
        self.gain *= gain;
    }

    fn split_channels(&mut self) -> Option<Vec<Box<dyn UpsamplerRuntime>>> {
        let history = std::mem::take(&mut self.history);
        Some(
            history
                .into_iter()
                .map(|h| {
                    Box::new(PolyInterp {
                        history: vec![h],
                        ..self.clone()
                    }) as Box<dyn UpsamplerRuntime>
                })
                .collect(),
        )
    }

    fn impulse_span_out_frames(&self) -> usize {
        match self.mode {
            PolyMode::Cosine => 3 * self.ratio - 1,
            PolyMode::CatmullRom => 4 * self.ratio - 1,
        }
    }
}

/// 127-tap windowed-sinc interpolator backed by a 128,000-entry periodic-Kaiser
/// table (1000 entries per input sample, beta = 14.96454265) with linear
/// interpolation between table entries and per-phase unity-DC normalisation.
///
/// Design choice: emits exactly `ratio` outputs per input sample at exact
/// phases `p = k / ratio` (`k = 0..ratio`), rather than accumulating `1 / ratio`
/// in `f64`. Identical at power-of-two ratios.
#[derive(Clone)]
struct TableSinc {
    ratio: usize,
    /// Precomputed per-phase 127-tap coefficients, shared across all channels.
    phases: Vec<[f64; TAPS]>,
    /// Mirrored ring buffer of length `2 * TAPS` per channel so `history[pos + 1 .. pos + 1 + TAPS]`
    /// is always a contiguous slice in oldest-to-newest order.
    history: Vec<[f64; TAPS * 2]>,
    pos: Vec<usize>,
}

impl TableSinc {
    fn new(ratio: usize, channels: usize) -> Self {
        let win = windows::kaiser_periodic(TABLE_LEN, KAISER_BETA);
        let step = PI / TABLE_PER_SAMPLE as f64;
        let table: Vec<f64> = (0..TABLE_LEN)
            .zip(win)
            .map(|(i, w)| {
                let s = if i == TABLE_CENTRE {
                    1.0
                } else {
                    let x = (i as f64 - TABLE_CENTRE as f64) * step;
                    x.sin() / x
                };
                s * w
            })
            .collect();

        let phases: Vec<[f64; TAPS]> = (0..ratio)
            .map(|k| {
                let p = k as f64 / ratio as f64;
                let mut c = [0.0; TAPS];

                let a = TABLE_CENTRE as f64 - TABLE_PER_SAMPLE as f64 * p;
                let ia = a.floor() as usize;
                let fa = a - a.floor();
                for (j, slot) in c[..64].iter_mut().enumerate() {
                    let idx = ia - TABLE_PER_SAMPLE * (63 - j);
                    *slot = table[idx] + fa * (table[idx + 1] - table[idx]);
                }

                let b = (TABLE_CENTRE + TABLE_PER_SAMPLE) as f64 - TABLE_PER_SAMPLE as f64 * p;
                let ib = b.floor() as usize;
                let fb = b - b.floor();
                for (j, slot) in c[64..TAPS].iter_mut().enumerate() {
                    let idx = ib + TABLE_PER_SAMPLE * j;
                    *slot = table[idx] + fb * (table[idx + 1] - table[idx]);
                }

                let inv_sum = 1.0 / c.iter().sum::<f64>();
                for v in &mut c {
                    *v *= inv_sum;
                }
                c
            })
            .collect();
        crate::test_support::record("table_sinc", || {
            crate::test_support::words(&[ratio as u64], &phases.concat())
        });

        Self {
            ratio,
            phases,
            history: vec![[0.0; TAPS * 2]; channels],
            pos: vec![0; channels],
        }
    }
}

#[inline]
fn dot_fma16(x: &[f64], c: &[f64; TAPS]) -> f64 {
    let mut b = x[0] * c[0];
    let mut k = 1;
    while k + 16 <= TAPS {
        let mut p = [[0.0_f64; 4]; 4];
        p[0][0] = b;
        for (g, row) in p.iter_mut().enumerate() {
            for (j, slot) in row.iter_mut().enumerate() {
                let idx = k + 4 * g + j;
                *slot = x[idx].mul_add(c[idx], *slot);
            }
        }
        let s0 = (p[3][0] + p[2][0]) + (p[1][0] + p[0][0]);
        let s1 = (p[3][1] + p[2][1]) + (p[1][1] + p[0][1]);
        let s2 = (p[3][2] + p[2][2]) + (p[1][2] + p[0][2]);
        let s3 = (p[3][3] + p[2][3]) + (p[1][3] + p[0][3]);
        b = (s0 + s2) + (s1 + s3);
        k += 16;
    }
    while k < TAPS {
        b = x[k].mul_add(c[k], b);
        k += 1;
    }
    b
}

impl UpsamplerRuntime for TableSinc {
    fn ratio(&self) -> RateRatio {
        RateRatio::integer(self.ratio as u32)
    }

    fn process(&mut self, input: &[&[f64]], output: &mut [&mut [f64]]) {
        assert_eq!(input.len(), self.history.len(), "input channel mismatch");
        assert_eq!(output.len(), self.history.len(), "output channel mismatch");
        let r = self.ratio;

        for (c, (&inp, out)) in input.iter().zip(output.iter_mut()).enumerate() {
            assert_eq!(out.len(), inp.len() * r, "output length mismatch");
            let hist = &mut self.history[c];
            let mut pos = self.pos[c];

            for (i, &x) in inp.iter().enumerate() {
                hist[pos] = x;
                hist[pos + TAPS] = x;
                let window = &hist[pos + 1..pos + 1 + TAPS];
                pos += 1;
                if pos == TAPS {
                    pos = 0;
                }

                let out_slot = &mut out[i * r..(i + 1) * r];
                for (dst, coef) in out_slot.iter_mut().zip(&self.phases) {
                    *dst = dot_fma16(window, coef);
                }
            }

            self.pos[c] = pos;
        }
    }

    fn reset(&mut self) {
        self.history.fill([0.0; TAPS * 2]);
        self.pos.fill(0);
    }

    fn latency_out_frames(&self) -> usize {
        63 * self.ratio
    }

    fn split_channels(&mut self) -> Option<Vec<Box<dyn UpsamplerRuntime>>> {
        let history = std::mem::take(&mut self.history);
        let pos = std::mem::take(&mut self.pos);
        Some(
            history
                .into_iter()
                .zip(pos)
                .map(|(h, p)| {
                    Box::new(TableSinc {
                        history: vec![h],
                        pos: vec![p],
                        ..self.clone()
                    }) as Box<dyn UpsamplerRuntime>
                })
                .collect(),
        )
    }

    fn scale_output(&mut self, gain: f64) {
        for phase in &mut self.phases {
            for v in phase.iter_mut() {
                *v *= gain;
            }
        }
    }

    fn impulse_span_out_frames(&self) -> usize {
        TAPS * self.ratio
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERP_PRESETS: [FilterPreset; 4] = [
        FilterPreset::Nos,
        FilterPreset::Cosine,
        FilterPreset::CatmullRom,
        FilterPreset::TableSinc,
    ];

    #[test]
    fn impulse_peak_and_span() {
        for preset in INTERP_PRESETS {
            for &ratio in &[2usize, 3, 6, 8] {
                let mut rt = super::build(preset, 44_100, ratio, 1, 64).unwrap();
                let total_in = rt.impulse_span_out_frames().div_ceil(ratio) + 4;
                let mut inp = vec![0.0; total_in];
                inp[0] = 1.0;
                let mut out = vec![0.0; total_in * ratio];
                rt.process(&[&inp], &mut [&mut out]);

                if preset == FilterPreset::Nos {
                    assert!(
                        out[..ratio].iter().all(|&v| v == 1.0),
                        "nos x{ratio} impulse {out:?}"
                    );
                    assert_eq!(rt.latency_out_frames(), ratio / 2);
                    assert_eq!(rt.latency_out_exact(), (ratio - 1) as f64 * 0.5);
                } else {
                    let peak_idx = out
                        .iter()
                        .enumerate()
                        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                        .unwrap()
                        .0;
                    assert_eq!(
                        peak_idx,
                        rt.latency_out_frames(),
                        "{} x{ratio} peak mismatch",
                        preset.name()
                    );
                    assert!(
                        (out[peak_idx] - 1.0).abs() < 1e-12,
                        "{} x{ratio} peak value {}",
                        preset.name(),
                        out[peak_idx]
                    );
                }

                let span = rt.impulse_span_out_frames();
                assert!(
                    out[span..].iter().all(|&v| v == 0.0),
                    "{} x{ratio} non-zero past span {span}",
                    preset.name()
                );
            }
        }
    }

    /// At ratios that are not powers of two the phases fall between table
    /// entries; the linear interpolation must track the continuous
    /// windowed sinc (truncating to the lower entry would be ~1e-3 off).
    #[test]
    fn table_sinc_interpolates_between_entries() {
        let continuous = |u: f64| {
            let s = if u == 0.0 {
                1.0
            } else {
                (PI * u).sin() / (PI * u)
            };
            let i = TABLE_CENTRE as f64 + TABLE_PER_SAMPLE as f64 * u;
            let t = 2.0 * i / TABLE_LEN as f64 - 1.0;
            let w = windows::bessel_i0_series(KAISER_BETA * (1.0 - t * t).max(0.0).sqrt())
                / windows::bessel_i0_series(KAISER_BETA);
            s * w
        };
        for &ratio in &[3usize, 6, 12] {
            let ts = TableSinc::new(ratio, 1);
            for (k, c) in ts.phases.iter().enumerate() {
                let p = k as f64 / ratio as f64;
                let raw: Vec<f64> = (0..TAPS).map(|j| continuous(j as f64 - 63.0 - p)).collect();
                let sum: f64 = raw.iter().sum();
                for (j, (&got, &want)) in c.iter().zip(&raw).enumerate() {
                    let want = want / sum;
                    assert!(
                        (got - want).abs() < 1e-5,
                        "x{ratio} phase {k} tap {j}: {got} vs {want}"
                    );
                }
            }
        }
    }

    #[test]
    fn unity_dc_gain_and_scale_output() {
        for preset in INTERP_PRESETS {
            for &ratio in &[3usize, 8] {
                let mut rt = super::build(preset, 44_100, ratio, 2, 128).unwrap();
                let mut scaled = super::build(preset, 44_100, ratio, 2, 128).unwrap();
                scaled.scale_output(0.5);

                let inp = vec![0.75; 256];
                let mut out_a0 = vec![0.0; 256 * ratio];
                let mut out_a1 = vec![0.0; 256 * ratio];
                let mut out_b0 = vec![0.0; 256 * ratio];
                let mut out_b1 = vec![0.0; 256 * ratio];

                rt.process(&[&inp, &inp], &mut [&mut out_a0, &mut out_a1]);
                scaled.process(&[&inp, &inp], &mut [&mut out_b0, &mut out_b1]);

                assert_eq!(out_a0, out_a1);
                let tail = out_a0.len() - 16 * ratio;
                for &v in &out_a0[tail..] {
                    assert!(
                        (v - 0.75).abs() < 1e-12,
                        "{} x{ratio} DC={v}",
                        preset.name()
                    );
                }
                for (a, b) in out_a0.iter().zip(out_b0.iter()) {
                    assert!((a * 0.5 - b).abs() < 1e-15);
                }
            }
        }
    }

    #[test]
    fn reset_and_zero_length_block() {
        for preset in INTERP_PRESETS {
            let mut rt = super::build(preset, 44_100, 4, 1, 32).unwrap();
            let inp = [0.25; 16];
            let mut out = [0.0; 64];
            rt.process(&[&inp], &mut [&mut out]);

            let empty: [f64; 0] = [];
            let mut empty_out: [f64; 0] = [];
            rt.process(&[&empty], &mut [&mut empty_out]);

            rt.reset();
            let zeros = [0.0; 32];
            let mut zout = [1.0; 128];
            rt.process(&[&zeros], &mut [&mut zout]);
            assert!(zout.iter().all(|&v| v == 0.0));
        }
    }

    #[test]
    fn cosine_and_catmull_rom_exact_values() {
        // Cosine x2 on [1.0, 0.0, 0.0]:
        // n=0: y1=0, y2=0 -> [0, 0]
        // n=1: y1=0, y2=1 -> [0.5*(1 - cos(pi/2)), 0.5*(1 - cos(pi))] = [0.5, 1.0]
        // n=2: y1=1, y2=0 -> [0.5*(1 + cos(pi/2)), 0.5*(1 + cos(pi))] = [0.5, 0.0]
        let mut cos_rt = super::build(FilterPreset::Cosine, 44_100, 2, 1, 8).unwrap();
        let mut out = [0.0; 6];
        cos_rt.process(&[&[1.0, 0.0, 0.0]], &mut [&mut out]);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);
        assert!((out[2] - 0.5).abs() < 1e-15);
        assert_eq!(out[3], 1.0);
        assert!((out[4] - 0.5).abs() < 1e-15);
        assert_eq!(out[5], 0.0);

        // Catmull-Rom x2 on [1.0, 0.0, 0.0, 0.0]:
        // Keys cubic with a = -0.5: at n=0, h=[0,0,0,1], m0=0, m1=1, mu=0.5 -> -0.0625.
        let mut cr_rt = super::build(FilterPreset::CatmullRom, 44_100, 2, 1, 8).unwrap();
        let mut cr_out = [0.0; 8];
        cr_rt.process(&[&[1.0, 0.0, 0.0, 0.0]], &mut [&mut cr_out]);
        let expected = [-0.0625, 0.0, 0.5625, 1.0, 0.5625, 0.0, -0.0625, 0.0];
        for (a, b) in cr_out.iter().zip(expected.iter()) {
            assert!((a - b).abs() < 1e-15, "got {cr_out:?}");
        }
    }

    #[test]
    fn nos_repeats_samples_bit_for_bit() {
        let samples = [
            0.0_f64,
            -0.0_f64,
            1.0,
            -1.0,
            0.123_456_789_012_345_67,
            -0.987_654_321_098_765_4,
            f64::MIN_POSITIVE,
            -1e-320,
        ];
        for &ratio in &[2usize, 3, 4, 8, 64] {
            let mut rt = super::build(FilterPreset::Nos, 44_100, ratio, 2, 64).unwrap();
            let ch1: Vec<f64> = samples.iter().rev().copied().collect();
            let mut out0 = vec![f64::NAN; samples.len() * ratio];
            let mut out1 = vec![f64::NAN; samples.len() * ratio];
            rt.process(&[&samples, &ch1], &mut [&mut out0, &mut out1]);

            for (i, &x) in samples.iter().enumerate() {
                for k in 0..ratio {
                    assert_eq!(
                        out0[i * ratio + k].to_bits(),
                        x.to_bits(),
                        "x{ratio} ch0 sample {i} copy {k}"
                    );
                }
            }
            for (i, &x) in ch1.iter().enumerate() {
                for k in 0..ratio {
                    assert_eq!(
                        out1[i * ratio + k].to_bits(),
                        x.to_bits(),
                        "x{ratio} ch1 sample {i} copy {k}"
                    );
                }
            }
        }
    }

    #[test]
    fn nos_sinc_droop_and_images() {
        use audivane_protocol::{RateTarget, UpsamplerConfig};

        let cfg_x4 = UpsamplerConfig {
            target: RateTarget::Ratio(4),
            preset: Some("nos".into()),
            design: None,
        };
        let discrete_zoh_db = |f: f64, fs: f64, r: usize| -> f64 {
            let num = (PI * f / fs).sin().abs();
            let den = r as f64 * (PI * f / (r as f64 * fs)).sin().abs();
            20.0 * (num / den).log10()
        };

        // 44.1 kHz x4: impulse is [1, 1, 1, 1], droop at 20 kHz matches discrete ZOH sinc,
        // and imaging lobes remain above source Nyquist.
        let m44 = crate::resample::measure_upsampler(&cfg_x4, 44_100).unwrap();
        assert_eq!(m44.response.impulse, vec![1.0; 4]);
        assert_eq!(m44.response.taps, 4);
        let [mag_20k_44, img_34k, null_44k] =
            m44.magnitude_db_at(&[20_000.0, 34_100.0, 44_100.0])[..]
        else {
            unreachable!()
        };
        let want_20k_44 = discrete_zoh_db(20_000.0, 44_100.0, 4);
        assert!(
            (mag_20k_44 - want_20k_44).abs() < 0.05,
            "44.1k x4 at 20 kHz: got {mag_20k_44:.4} dB, want {want_20k_44:.4} dB"
        );
        // First image of 10 kHz sits at 44.1 - 10 = 34.1 kHz and must not be filtered out.
        let want_34k = discrete_zoh_db(34_100.0, 44_100.0, 4);
        assert!(
            (img_34k - want_34k).abs() < 0.05 && img_34k > -15.0,
            "44.1k x4 image at 34.1 kHz: got {img_34k:.4} dB, want {want_34k:.4} dB"
        );
        // Null at fs = 44.1 kHz.
        assert!(null_44k < -100.0);

        // 96 kHz x4: droop at 20 kHz is about -0.6 dB.
        let m96 = crate::resample::measure_upsampler(&cfg_x4, 96_000).unwrap();
        let mag_20k_96 = m96.magnitude_db_at(&[20_000.0])[0];
        let want_20k_96 = discrete_zoh_db(20_000.0, 96_000.0, 4);
        assert!(
            (mag_20k_96 - want_20k_96).abs() < 0.05,
            "96k x4 at 20 kHz: got {mag_20k_96:.4} dB, want {want_20k_96:.4} dB"
        );

        // At high ratio (x16), discrete ZOH converges to continuous sinc(f / fs).
        let cfg_x16 = UpsamplerConfig {
            target: RateTarget::Ratio(16),
            preset: Some("nos".into()),
            design: None,
        };
        let m44_x16 = crate::resample::measure_upsampler(&cfg_x16, 44_100).unwrap();
        let x = PI * 20_000.0 / 44_100.0;
        let cont_sinc_db = 20.0 * (x.sin() / x).abs().log10();
        let mag_20k_x16 = m44_x16.magnitude_db_at(&[20_000.0])[0];
        assert!(
            (mag_20k_x16 - cont_sinc_db).abs() < 0.05,
            "44.1k x16 at 20 kHz: got {mag_20k_x16:.4} dB, continuous sinc {cont_sinc_db:.4} dB"
        );
    }
}
