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
