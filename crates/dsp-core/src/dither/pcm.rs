//! Fixed-coefficient PCM dither and feedback loops, in output LSB units.
use audivane_protocol::DitherKind;

#[derive(Clone, Debug)]
struct Kiss {
    x: u32,
    y: u32,
    z: u32,
    c: u32,
}

impl Kiss {
    fn new(seed: u64, channel: usize) -> Self {
        let k = (seed as u32).wrapping_add(channel as u32);
        let (x, y, z, c) = if k == 0 {
            // The engine seed replaces a time-dependent seed. Folding its
            // upper word also makes the zero-index case deterministic.
            let t = (seed >> 32) as u32;
            (
                2390734783 ^ t,
                3082164705 ^ t,
                1257101940 ^ t,
                3546005401 ^ t,
            )
        } else {
            (
                k ^ 2485140220,
                k ^ 1538284562,
                k ^ 1817825840,
                k ^ 1451305570,
            )
        };
        Self {
            x,
            y,
            z,
            c: c % 698769069,
        }
    }

    fn next(&mut self) -> u32 {
        self.x = self.x.wrapping_mul(69069).wrapping_add(12345);
        self.y ^= self.y << 13;
        self.y ^= self.y >> 17;
        self.y ^= self.y << 5;
        let t = 698769069u64 * u64::from(self.z) + u64::from(self.c);
        self.z = t as u32;
        self.c = (t >> 32) as u32;
        self.x.wrapping_add(self.y).wrapping_add(self.z)
    }

    fn uniform(&mut self) -> f64 {
        f64::from(self.next()) * (1.0 / 4294967296.0)
    }

    fn tpdf(&mut self) -> f64 {
        // Convert both draws before addition: no integer wraparound.
        self.uniform() + self.uniform() - 1.0
    }

    fn gaussian(&mut self) -> f64 {
        loop {
            let a = self.uniform() * 2.0 - 1.0;
            let b = self.uniform() * 2.0 - 1.0;
            let s = a * a + b * b;
            if s == 0.0 || s >= 1.0 {
                continue;
            }
            let g = (a / 3.0) * (-2.0 * s.ln() / s).sqrt();
            if g.abs() <= 1.0 {
                return g;
            }
        }
    }
}

pub(super) fn headroom(kind: DitherKind) -> f64 {
    match kind {
        DitherKind::Feedback1 | DitherKind::Resonant5 | DitherKind::Resonant9 => 4.0,
        DitherKind::Feedback5 => 2.0,
        DitherKind::Integrator15 => 16.0,
        _ => 1.0,
    }
}

fn order(kind: DitherKind) -> usize {
    match kind {
        DitherKind::Resonant5 => 5,
        DitherKind::Resonant9 => 9,
        DitherKind::Integrator15 => 15,
        _ => 0,
    }
}

const GAINS: [f64; 15] = [
    1.0,
    1.0 / 2.0,
    1.0 / 6.0,
    1.0 / 24.0,
    1.0 / 120.0,
    1.0 / 720.0,
    1.0 / 5040.0,
    1.0 / 40320.0,
    1.0 / 362880.0,
    1.0 / 3628800.0,
    1.0 / 39916800.0,
    1.0 / 479001600.0,
    1.0 / 6227020800.0,
    1.0 / 87178291200.0,
    1.0 / 1307674368000.0,
];
const HP_B: [f64; 7] = [1.0, -6.0, 15.0, -20.0, 15.0, -6.0, 1.0];
const HP_A: [f64; 7] = [
    1.0,
    2.6863369274708733,
    3.502210408461253,
    2.6178638043550397,
    1.1676663366598323,
    0.2900643727181562,
    0.031151133266637432,
];

pub(super) struct Channel {
    rng: Kiss,
    feedback: Feedback,
    hp_x: [f64; 6],
    hp_y: [f64; 6],
}

/// Same state transition is used by the nonlinear quantizer and linear
/// impulse-response tests. All right-hand sides use the previous states.
#[derive(Default)]
struct Feedback {
    s: [f64; 15],
    w: [f64; 5],
}

impl Feedback {
    fn advance(&mut self, kind: DitherKind) -> f64 {
        match kind {
            DitherKind::Feedback1 => 0.9 * self.w[0],
            DitherKind::Feedback5 => self
                .w
                .iter()
                .zip([1.0, 0.5, 0.25, 0.125, 0.0625])
                .map(|(w, c)| w * c)
                .sum(),
            _ => {
                let k = order(kind);
                if k == 0 {
                    return 0.0;
                }
                let old = self.s;
                let mut g = if kind == DitherKind::Integrator15 {
                    2.0
                } else {
                    1.0
                };
                self.s[0] += g * self.w[0];
                for i in 1..k {
                    g = if kind == DitherKind::Resonant5 {
                        g * 0.5
                    } else {
                        GAINS[i]
                            * if kind == DitherKind::Integrator15 {
                                2.0
                            } else {
                                1.0
                            }
                    };
                    self.s[i] += g * old[i - 1];
                }
                match kind {
                    DitherKind::Resonant5 => {
                        self.s[1] -= old[2] / 16.0;
                        self.s[2] -= old[3] / 8.0;
                    }
                    DitherKind::Resonant9 => {
                        self.s[1] -= old[2] / 2.0;
                        self.s[3] -= old[4] / 16.0;
                    }
                    _ => {}
                }
                self.s[..k].iter().sum()
            }
        }
    }

    fn store(&mut self, w: f64) {
        self.w.copy_within(..4, 1);
        self.w[0] = w;
    }
}

impl Channel {
    pub(super) fn new(seed: u64, ch: usize) -> Self {
        Self {
            rng: Kiss::new(seed, ch),
            feedback: Feedback::default(),
            hp_x: [0.0; 6],
            hp_y: [0.0; 6],
        }
    }

    pub(super) fn clear_filter(&mut self) {
        self.feedback = Feedback::default();
        self.hp_x = [0.0; 6];
        self.hp_y = [0.0; 6];
    }

    fn noise(&mut self, kind: DitherKind) -> f64 {
        match kind {
            DitherKind::Rectangular => self.rng.uniform() * 2.0 - 1.0,
            DitherKind::Gaussian => self.rng.gaussian(),
            DitherKind::Feedback1 => self.rng.tpdf() * 0.5,
            DitherKind::HighpassTpdf => {
                let x = self.rng.tpdf();
                let mut y = HP_B[0] * x * 0.0016681683413070879;
                for i in 0..6 {
                    y += HP_B[i + 1] * 0.0016681683413070879 * self.hp_x[i];
                    y -= HP_A[i + 1] * self.hp_y[i];
                }
                self.hp_x.copy_within(..5, 1);
                self.hp_y.copy_within(..5, 1);
                self.hp_x[0] = x;
                self.hp_y[0] = y;
                y
            }
            _ => self.rng.tpdf(),
        }
    }

    pub(super) fn sample(&mut self, kind: DitherKind, v: f64) -> f64 {
        let d = self.noise(kind);
        let q = (v + d + self.feedback.advance(kind)).round_ties_even();
        self.feedback.store(v - q);
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dither::Quantizer;
    use audivane_protocol::OutputConfig;

    const KINDS: [DitherKind; 9] = [
        DitherKind::HeadroomTpdf,
        DitherKind::Rectangular,
        DitherKind::Gaussian,
        DitherKind::HighpassTpdf,
        DitherKind::Feedback1,
        DitherKind::Feedback5,
        DitherKind::Resonant5,
        DitherKind::Resonant9,
        DitherKind::Integrator15,
    ];

    #[test]
    fn kiss_matches_independent_integer_vectors() {
        // Computed separately with arbitrary-precision integer arithmetic,
        // explicitly masking each 32-bit operation.
        for (seed, expected) in [
            (
                0,
                [
                    1835769068, 440524962, 3112462261, 3185440595, 3524634109, 1078739667,
                ],
            ),
            (
                1,
                [
                    773280066, 117534705, 59361158, 1814423701, 3602451877, 1254053667,
                ],
            ),
            (
                42,
                [
                    1885671514, 946138085, 436012277, 1554101179, 924557978, 3051785347,
                ],
            ),
        ] {
            let mut rng = Kiss::new(seed, 0);
            assert_eq!(std::array::from_fn::<_, 6, _>(|_| rng.next()), expected);
        }
        let mut a = Kiss::new(41, 1);
        let mut b = Kiss::new(42, 0);
        for _ in 0..100 {
            assert_eq!(a.next(), b.next());
        }
    }

    #[test]
    fn feedback_uses_previous_states_and_clean_input_error() {
        let mut f = Feedback {
            s: std::array::from_fn(|i| (i + 1) as f64),
            w: [2.0; 5],
        };
        f.advance(DitherKind::Resonant5);
        assert_eq!(&f.s[..5], &[3.0, 2.3125, 3.0, 4.375, 5.25]);
        let mut f = Feedback {
            s: std::array::from_fn(|i| (i + 1) as f64),
            w: [2.0; 5],
        };
        f.advance(DitherKind::Resonant9);
        assert_eq!(
            &f.s[..4],
            &[3.0, 1.0, 3.0 + 2.0 / 6.0, 4.0 + 3.0 / 24.0 - 5.0 / 16.0]
        );
        f.store(0.75 - 2.0);
        assert_eq!(f.w, [-1.25, 2.0, 2.0, 2.0, 2.0]);
        assert_eq!(f.advance(DitherKind::Feedback1), -1.125);
        assert_eq!(f.advance(DitherKind::Feedback5), 0.625);
    }

    #[test]
    fn distributions_and_highpass_variance() {
        for (kind, variance) in [
            (DitherKind::HeadroomTpdf, 1.0 / 6.0),
            (DitherKind::Rectangular, 1.0 / 3.0),
            (DitherKind::Gaussian, 0.10815),
            (DitherKind::HighpassTpdf, 0.0457),
        ] {
            let mut c = Channel::new(42, 0);
            let (mut sum, mut sq) = (0.0, 0.0);
            let n = 1 << 18;
            for _ in 0..n {
                let d = c.noise(kind);
                if kind != DitherKind::HighpassTpdf {
                    assert!(d.abs() <= 1.0);
                }
                sum += d;
                sq += d * d;
            }
            assert!((sum / n as f64).abs() < 0.004);
            assert!(
                (sq / n as f64 - variance).abs() < 0.002,
                "{kind:?}: {}",
                sq / n as f64
            );
        }
    }

    #[test]
    fn arbitrary_blocks_reset_and_channels_are_independent() {
        let x: Vec<f64> = (0..4096).map(|i| 0.01 * (i as f64 * 0.17).sin()).collect();
        for kind in KINDS {
            for bits in [16, 24, 32] {
                let cfg = OutputConfig {
                    bits,
                    dither: kind,
                    headroom_db: 0.0,
                };
                let mut a = Quantizer::new(&cfg, 2, 41);
                let mut b = Quantizer::new(&cfg, 2, 41);
                let mut all = vec![0; x.len() * 2];
                let mut split = vec![0; x.len() * 2];
                a.process(&[&x, &x], &mut all);
                let mut i = 0;
                for n in [0, 1, 13, 0, 127, 997, 2958] {
                    b.process(
                        &[&x[i..i + n], &x[i..i + n]],
                        &mut split[i * 2..(i + n) * 2],
                    );
                    i += n;
                }
                assert_eq!(all, split, "{kind:?}/{bits}");
                b.reset();
                b.process(&[&x, &x], &mut split);
                assert_eq!(all, split);
                // Channel 1 uses offset seed 42, independent of channel 0.
                let mut mono = Quantizer::new(&cfg, 1, 42);
                let mut right = vec![0; x.len()];
                mono.process(&[&x], &mut right);
                assert!(
                    all.as_chunks::<2>()
                        .0
                        .iter()
                        .zip(right)
                        .all(|(p, r)| p[1] == r)
                );
            }
        }
    }

    #[test]
    fn scaling_input_clamp_and_ties_even_at_all_word_lengths() {
        for bits in [16, 24, 32] {
            let scale = 2f64.powi(bits as i32 - 1);
            let mut none = Quantizer::new(
                &OutputConfig {
                    bits,
                    dither: DitherKind::None,
                    headroom_db: 0.0,
                },
                1,
                42,
            );
            let x = [0.5 / scale, 1.5 / scale, 2.5 / scale, -1.5 / scale];
            let mut out = [0; 4];
            none.process(&[&x], &mut out);
            assert_eq!(
                out,
                [
                    0,
                    2i32.wrapping_shl(32 - bits),
                    2i32.wrapping_shl(32 - bits),
                    (-2i32).wrapping_shl(32 - bits)
                ]
            );
            for kind in KINDS {
                let cfg = OutputConfig {
                    bits,
                    dither: kind,
                    headroom_db: 0.0,
                };
                let mut a = Quantizer::new(&cfg, 1, 42);
                let mut b = Quantizer::new(&cfg, 1, 42);
                let x = [1.0, -1.0, 0.25];
                let mut expected = [0; 3];
                a.process(&[&x], &mut expected);
                let stats = b.process(&[&[1.5, -1.5, 0.25]], &mut out[..3]);
                assert_eq!(&out[..3], &expected);
                assert_eq!(stats.clipped, 2);
                let mut c = Channel::new(42, 0);
                for (i, &v) in x.iter().enumerate() {
                    let q = c.sample(kind, v * (scale - 1.0 - headroom(kind)));
                    assert_eq!(expected[i], (q as i64).wrapping_shl(32 - bits) as i32);
                }
            }
        }
    }

    #[test]
    fn linear_ntf_zeros_match_integrator_state_characteristic() {
        use crate::Complex64 as C;
        // NTF numerator is det(zI-A), where A is the open integrator
        // transition. Check its complete polynomial, including repeated
        // zeros, at well-conditioned points away from the roots.
        for kind in [
            DitherKind::Resonant5,
            DitherKind::Resonant9,
            DitherKind::Integrator15,
        ] {
            let k = order(kind);
            let mut a = vec![vec![C::default(); k]; k];
            for col in 0..k {
                let mut state = Feedback::default();
                state.s[col] = 1.0;
                state.advance(kind);
                for (row, values) in a.iter_mut().enumerate() {
                    values[col].re = state.s[row];
                }
            }
            for z in [C::new(-1.0, 0.0), C::new(0.0, 1.0), C::new(1.5, 0.4)] {
                let mut m = a.clone();
                for (row, values) in m.iter_mut().enumerate() {
                    for (col, entry) in values.iter_mut().enumerate() {
                        *entry = -*entry + if row == col { z } else { C::default() };
                    }
                }
                let mut det = C::new(1.0, 0.0);
                for col in 0..k {
                    let pivot = (col..k)
                        .max_by(|&a, &b| m[a][col].norm_sqr().total_cmp(&m[b][col].norm_sqr()))
                        .unwrap();
                    if pivot != col {
                        m.swap(pivot, col);
                        det = -det;
                    }
                    let value = m[col][col];
                    det *= value;
                    let pivot_row = m[col].clone();
                    for row in m.iter_mut().skip(col + 1) {
                        let factor = row[col] / value;
                        for j in col + 1..k {
                            row[j] -= factor * pivot_row[j];
                        }
                    }
                }
                let t = z - 1.0;
                let expected = match kind {
                    DitherKind::Resonant5 => t.powu(3) * (t * t + 1.0 / 32.0),
                    DitherKind::Resonant9 => {
                        t.powu(5) * (t * t + 1.0 / 1920.0) * (t * t + 1.0 / 12.0)
                    }
                    _ => t.powu(15),
                };
                assert!(
                    (det - expected).norm() < 1e-11 * expected.norm().max(1.0),
                    "{kind:?} at {z}"
                );
            }
        }
    }

    #[test]
    fn linear_impulse_ntf_peak_and_power() {
        let n = 1 << 18;
        for (kind, peak, power, frequency, bands) in [
            (
                DitherKind::Feedback1,
                20.0,
                7.21,
                0.5,
                [1.0, -4.7, -5.4, -5.5, -5.6],
            ),
            (
                DitherKind::Feedback5,
                10.1,
                3.70,
                0.5,
                [2.2, -5.0, -8.2, -9.1, -9.3],
            ),
            (
                DitherKind::Resonant5,
                5.82,
                4.04,
                0.087784,
                [4.0, 3.6, 2.0, -11.9, -40.2],
            ),
            (
                DitherKind::Resonant9,
                9.09,
                4.38,
                0.102028,
                [4.4, 4.4, 2.6, -23.9, -27.0],
            ),
            (
                DitherKind::Integrator15,
                16.1,
                9.50,
                0.208897,
                [9.7, 9.1, -12.2, -29.6, -48.0],
            ),
        ] {
            let mut f = Feedback::default();
            let mut h = vec![crate::Complex64::default(); n];
            for (i, h) in h.iter_mut().enumerate() {
                let q = f.advance(kind) + if i == 0 { 1.0 } else { 0.0 };
                f.store(-q);
                h.re = q;
            }
            let gain = 10.0 * h.iter().map(|x| x.norm_sqr()).sum::<f64>().log10();
            let mut planner = rustfft::FftPlanner::new();
            planner.plan_fft_forward(n).process(&mut h);
            let (bin, max) = h[..=n / 2]
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.norm_sqr().total_cmp(&b.1.norm_sqr()))
                .unwrap();
            let measured = 20.0 * max.norm().log10();
            assert!((bin as f64 / n as f64 - frequency).abs() < 0.00002);
            for (rate, expected) in [44100.0, 88200.0, 176400.0, 352800.0, 705600.0]
                .into_iter()
                .zip(bands)
            {
                let end = (20000.0 * n as f64 / rate) as usize;
                let band = h[..=end].iter().map(|v| v.norm_sqr()).sum::<f64>() / (end + 1) as f64;
                let db = 10.0 * band.log10();
                assert!((db - expected).abs() < 0.1, "{kind:?} {rate}: {db} dB");
                eprintln!("{kind:?}/{rate}: 0-20k mean NTF² {db:.4} dB");
            }
            assert!((gain - power).abs() < 0.08, "{kind:?} power {gain}");
            assert!((measured - peak).abs() < 0.08, "{kind:?} peak {measured}");
            eprintln!(
                "{kind:?}: peak {measured:.4} dB at {:.6} fs, power {gain:.4} dB",
                bin as f64 / n as f64
            );
        }
    }

    #[test]
    fn packing_saturates_without_changing_raw_feedback() {
        let n = 1 << 17;
        for bits in [16, 24, 32] {
            for seed in [42, crate::chain::DEFAULT_SEED] {
                let max = (1i64 << (bits - 1)) - 1;
                let min = -(1i64 << (bits - 1));
                let v = max as f64 - headroom(DitherKind::Feedback5);
                let mut raw = Channel::new(seed, 0);
                let mut packed = Quantizer::new(
                    &OutputConfig {
                        bits,
                        dither: DitherKind::Feedback5,
                        headroom_db: 0.0,
                    },
                    1,
                    seed,
                );
                let mut overflows = 0;
                for _ in 0..n {
                    let q = raw.sample(DitherKind::Feedback5, v);
                    let mut out = [0];
                    let stats = packed.process(&[&[1.0]], &mut out);
                    let expected = (q as i64).clamp(min, max);
                    assert_eq!(i64::from(out[0]) >> (32 - bits), expected);
                    assert_eq!(packed.pcm[0].feedback.w, raw.feedback.w);
                    if q > max as f64 {
                        overflows += 1;
                        assert_eq!(stats.clipped, 1);
                        assert_eq!(out[0], (max << (32 - bits)) as i32);
                        // A packed rail cannot replace q in the loop state.
                        assert_ne!(packed.pcm[0].feedback.w[0], v - max as f64);
                    } else {
                        assert_eq!(stats.clipped, 0);
                    }
                }
                assert!(
                    overflows > 0,
                    "bits {bits}, seed {seed}: must exercise saturation"
                );
            }
        }
    }

    #[test]
    fn production_quantizer_rails_noise_and_sines_never_wrap() {
        let mut kinds = KINDS.to_vec();
        kinds.extend([DitherKind::None, DitherKind::Tpdf, DitherKind::Shaped]);
        // More than a quarter million frames for EACH independent waveform,
        // bit depth, mode and seed; use odd blocks to cross state boundaries.
        let n = 1 << 18;
        let mut input = [0.0; 257];
        let mut out = [0; 257];
        for kind in kinds {
            for bits in [16, 24, 32] {
                for seed in [42, crate::chain::DEFAULT_SEED] {
                    let mut worst = 0.0f64;
                    let mut clips = 0;
                    for waveform in 0..6 {
                        let mut q = Quantizer::new(
                            &OutputConfig {
                                bits,
                                dither: kind,
                                headroom_db: 0.0,
                            },
                            1,
                            seed,
                        );
                        let mut signal = Kiss::new(123, 0);
                        let scale = 2f64.powi(bits as i32 - 1);
                        let v_scale = if kind.enables_processing() {
                            scale - 1.0 - headroom(kind)
                        } else {
                            scale
                        };
                        let bound = match kind {
                            DitherKind::None => 1.0,
                            DitherKind::Tpdf
                            | DitherKind::HeadroomTpdf
                            | DitherKind::Rectangular
                            | DitherKind::Gaussian => 1.5,
                            DitherKind::HighpassTpdf => 2.0,
                            DitherKind::Feedback1 | DitherKind::Resonant5 => 5.0,
                            DitherKind::Feedback5 => 4.0,
                            DitherKind::Resonant9 => 6.0,
                            DitherKind::Integrator15 => 12.0,
                            DitherKind::Shaped => {
                                1.5 * (1.0
                                    + q.shaper().unwrap().iter().map(|h| h.abs()).sum::<f64>())
                            }
                        };
                        let mut start = 0;
                        while start < n {
                            let len = (n - start).min(input.len());
                            for (i, x) in input[..len].iter_mut().enumerate() {
                                let t = (start + i) as f64;
                                *x = match waveform {
                                    0 => 1.0,
                                    1 => -1.0,
                                    2 => {
                                        if (start + i) % 2 == 0 {
                                            1.0
                                        } else {
                                            -1.0
                                        }
                                    }
                                    3 => signal.uniform() * 2.0 - 1.0,
                                    4 => (t * 0.017).sin(),
                                    _ => (t * 0.97 * std::f64::consts::PI).sin(),
                                };
                            }
                            clips += q.process(&[&input[..len]], &mut out[..len]).clipped;
                            for (i, (&x, &y)) in input[..len].iter().zip(&out[..len]).enumerate() {
                                let word = i64::from(y) >> (32 - bits);
                                assert_eq!(i64::from(y), word << (32 - bits), "left justification");
                                let error = (word as f64 - x * v_scale).abs();
                                assert!(
                                    error <= bound,
                                    "{kind:?}/{bits}, seed {seed}, waveform {waveform}, sample {}: input {x}, packed {word}, error {error}",
                                    start + i
                                );
                                worst = worst.max(error);
                            }
                            start += len;
                        }
                    }
                    eprintln!(
                        "{kind:?}/{bits}, seed {seed}: packed worst error {worst:.9} LSB, clips {clips}"
                    );
                }
            }
        }
    }

    #[test]
    fn full_scale_signal_stability() {
        for kind in KINDS {
            for bits in [16, 24, 32] {
                let mut c = Channel::new(42, 0);
                let scale = 2f64.powi(bits - 1) - 1.0 - headroom(kind);
                let bound = match kind {
                    DitherKind::HeadroomTpdf | DitherKind::Rectangular | DitherKind::Gaussian => {
                        1.5
                    }
                    DitherKind::HighpassTpdf => 2.0,
                    DitherKind::Feedback1 | DitherKind::Resonant5 => 5.0,
                    DitherKind::Feedback5 => 4.0,
                    DitherKind::Resonant9 => 6.0,
                    DitherKind::Integrator15 => 12.0,
                    _ => unreachable!(),
                };
                let mut signal = Kiss::new(123, 0);
                let mut worst = 0.0f64;
                for i in 0..(6 << 17) {
                    let x = match i / (1 << 17) {
                        0 => signal.uniform() * 2.0 - 1.0,
                        1 => (i as f64 * 0.17).sin(),
                        2 => {
                            if i % 2 == 0 {
                                1.0
                            } else {
                                -1.0
                            }
                        }
                        3 => 1.0,
                        4 => -1.0,
                        _ => (i as f64 * 0.97 * std::f64::consts::PI).sin(),
                    };
                    let q = c.sample(kind, x * scale);
                    assert!(
                        q.is_finite() && (q - x * scale).abs() <= bound,
                        "{kind:?}/{bits}: {q}"
                    );
                    for s in c
                        .feedback
                        .s
                        .iter()
                        .chain(&c.feedback.w)
                        .chain(&c.hp_x)
                        .chain(&c.hp_y)
                    {
                        assert!(
                            s.is_finite() && s.abs() <= bound,
                            "{kind:?}/{bits}: state {s}"
                        );
                        worst = worst.max(s.abs());
                    }
                }
                assert!(worst <= bound, "{kind:?}/{bits}: state {worst}");
                eprintln!("{kind:?}/{bits}: worst internal state {worst:.9} LSB");
            }
        }
    }
}
