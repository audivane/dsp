//! Stage kernels of the FIR-type cascades: the windowed-sinc prototype,
//! its cepstral minimum / intermediate phase versions, and the
//! frequency-sampled stage. Design time only (allocates freely).

use std::f64::consts::PI;

use crate::fft::RealFftPair;
use rustfft::num_complex::Complex64;

/// Length setting of a windowed-sinc stage. Odd lengths are used as is,
/// even ones lose a tap, so 256 gives 255 taps.
pub(super) const FIR_LEN_SETTING: usize = 256;

/// Taps of a windowed-sinc stage (both factors).
pub(super) const FIR_TAPS: usize = FIR_LEN_SETTING + (FIR_LEN_SETTING & 1) - 1;

/// Sidelobe ratio of the prototype's Dolph-Chebyshev window (-144.5 dB).
#[cfg(test)]
const FIR_CHEBYSHEV_R: f64 = 1.0 / 16_777_216.0; // 2^-24

/// Cutoff of a windowed-sinc stage as a fraction of the stage's output
/// Nyquist. The x2 value is 0.95351 of the stage input Nyquist; the x3
/// value is that over 3. Both are fixed constants of the recipe.
pub(super) fn fir_cutoff(f: usize) -> f64 {
    match f {
        2 => 0.476755,
        3 => 0.317_836_666_666_666_66,
        _ => unreachable!("stage factor {f}"),
    }
}

/// The windowed-sinc prototype of a stage with factor `f`: 255 taps at the
/// stage output rate, `sin(k fc pi) / (k fc pi)` (k = i - 127) times the
/// symmetric Dolph-Chebyshev window ([`fir_window`]), scaled to a DC gain
/// of 1 (by the reciprocal of the pairwise sum of the taps).
pub(super) fn fir_prototype(f: usize) -> Vec<f64> {
    let n = FIR_TAPS;
    let c = n / 2;
    let w = fir_window();
    let wc = fir_cutoff(f) * PI;
    let mut h: Vec<f64> = (0..n)
        .map(|i| {
            let s = if i == c {
                1.0
            } else {
                let x = (i as f64 - c as f64) * wc;
                x.sin() / x
            };
            s * w[i]
        })
        .collect();
    let inv = 1.0 / pairwise_sum(&h);
    for v in &mut h {
        *v *= inv;
    }
    h
}

/// The 255-tap symmetric Dolph-Chebyshev window (r = 2^-24) of the
/// windowed-sinc stages: the direct cosine sum of
/// [`crate::design::paths::fir::windows::chebyshev_symmetric`] evaluated in x87
/// extended precision (64-bit mantissa, the C library's long double
/// functions), rounded to double, then scaled by 1 / max in double.
///
/// The extended-precision rounding is part of the recipe: the sum cancels
/// heavily at the end taps (5.7e-12 relative off the true window there,
/// against 3.5e-9 for a plain double sum), and the cepstral stages turn a
/// 1e-11 change of the end taps into a fraction of a 32-bit LSB at the
/// output, so the window is stored as it comes out of that evaluation.
/// First half; the window is symmetric.
const FIR_WINDOW_HALF: [f64; 128] = [
    4.210155899140311e-06,
    4.961914493111424e-06,
    7.851332555697555e-06,
    1.1826065165146684e-05,
    1.7174532238246113e-05,
    2.42409455872755e-05,
    3.343299127317525e-05,
    4.523017604944837e-05,
    6.0192851706451536e-05,
    7.897192631673461e-05,
    0.00010231926599552443,
    0.0001310987848299747,
    0.00016629821410652504,
    0.0002090415348945301,
    0.00026060205045026503,
    0.0003224160668198387,
    0.00039609714148136285,
    0.0004834508509224373,
    0.0005864900187527804,
    0.0007074503363654346,
    0.0008488062983525615,
    0.001013287364929823,
    0.0012038942536099595,
    0.0014239152523811197,
    0.0016769424367843583,
    0.0019668876636484383,
    0.0022979982049341034,
    0.0026748718762736796,
    0.0031024715064773985,
    0.0035861385866293844,
    0.004131605930528932,
    0.004745009172261411,
    0.00543289692172145,
    0.006202239395070024,
    0.007060435334494022,
    0.008015317030353858,
    0.009075153258947845,
    0.010248649950779579,
    0.011544948407465744,
    0.012973620890336417,
    0.014544663410415543,
    0.016268485557872,
    0.018155897219233396,
    0.020218092042673243,
    0.022466627525519978,
    0.024913401613779293,
    0.027570625720879385,
    0.030450794091994065,
    0.033566649461105526,
    0.036931144970354525,
    0.040557402345088915,
    0.044458666343243754,
    0.04864825552413118,
    0.05313950940923291,
    0.05794573213600344,
    0.06308013273482292,
    0.06855576218888687,
    0.07438544746677225,
    0.08058172274745425,
    0.0871567580874284,
    0.09412228580908005,
    0.10148952491828461,
    0.1092691038871673,
    0.11747098216474242,
    0.12610437080353304,
    0.1351776526139891,
    0.14469830228032354,
    0.1546728068910297,
    0.16510658735459308,
    0.17600392118554442,
    0.18736786715780832,
    0.1992001923310869,
    0.211501301961612,
    0.22427017281083467,
    0.2375042903643793,
    0.25119959046874746,
    0.2653504058847408,
    0.279949418244321,
    0.29498761588161176,
    0.31045425798897264,
    0.32633684552557324,
    0.342621099278725,
    0.3592909454474858,
    0.37632850908386023,
    0.3937141156894339,
    0.4114263012246878,
    0.429441830744752,
    0.4477357258292245,
    0.4662813009251654,
    0.4850502086717784,
    0.5040124942229275,
    0.5231366585298469,
    0.5423897304915426,
    0.561737347824837,
    0.5811438464501484,
    0.6005723581333334,
    0.6199849160686584,
    0.6393425680335996,
    0.6586054966931368,
    0.6777331465798773,
    0.696684357227155,
    0.7154175018855691,
    0.7338906312096302,
    0.7520616212606571,
    0.7698883251351172,
    0.7873287274945873,
    0.8043411012446957,
    0.820884165586062,
    0.8369172446406248,
    0.8524004258420254,
    0.8672947172690677,
    0.8815622030968426,
    0.8951661963409545,
    0.908071388076498,
    0.9202439923249948,
    0.9316518858193901,
    0.9422647418793596,
    0.9520541576564702,
    0.960993774041029,
    0.9690593875595489,
    0.9762290536334229,
    0.9824831806153876,
    0.9878046140703387,
    0.9921787108207426,
    0.9955934023338763,
    0.9980392470880637,
    0.999509471617524,
    0.9999999999999999,
];

/// [`FIR_WINDOW_HALF`] mirrored to 255 taps.
pub(super) fn fir_window() -> Vec<f64> {
    let n = FIR_TAPS;
    (0..n).map(|i| FIR_WINDOW_HALF[i.min(n - 1 - i)]).collect()
}

/// The symmetric Dolph-Chebyshev window by the same direct cosine sum as
/// [`crate::design::paths::fir::windows::chebyshev_symmetric`],
///
/// ```text
/// w[n] = (1/N) (1 + 2r sum_{k=1..N/2} T(x0 cos(k pi/N)) cos(k (2n+1-N) pi/N))
/// ```
///
/// with T the Chebyshev polynomial of degree N-1 and x0 its root of
/// T(x0) = 1/r, evaluated in double-double and rounded once, then scaled to
/// a peak of 1 in double. The check on [`FIR_WINDOW_HALF`].
#[cfg(test)]
fn chebyshev_window(n: usize, r: f64) -> Vec<f64> {
    let m = n - 1;
    let inv_r = 1.0 / r;
    // x0 = cosh(acosh(1/r) / (N-1)), refined by Newton on T(x0) = 1/r.
    let mut x0 = Dd::from(((inv_r).acosh() / m as f64).cosh());
    for _ in 0..4 {
        let (t, dt) = chebyshev_t(x0, m);
        x0 = x0.add(t.add(Dd::from(-inv_r)).div(dt).neg());
    }
    // cos(j pi / N) for j = 0..2N.
    let cos_pi_n: Vec<Dd> = (0..2 * n)
        .map(|j| {
            let (j, sign) = {
                let j = if j > n { 2 * n - j } else { j }; // cos(2pi - a)
                if 2 * j > n { (n - j, -1.0) } else { (j, 1.0) } // -cos(pi - a)
            };
            let a = Dd::PI.mul_f(j as f64).div_f(n as f64);
            let c = cos_dd(a);
            if sign < 0.0 { c.neg() } else { c }
        })
        .collect();
    let t: Vec<Dd> = (1..=n / 2)
        .map(|k| chebyshev_t(x0.mul(cos_pi_n[k]), m).0)
        .collect();
    let mut w: Vec<f64> = (0..n)
        .map(|i| {
            let d = (2 * i + 1).abs_diff(n);
            let mut s = Dd::ZERO;
            for (k, &tk) in t.iter().enumerate() {
                s = s.add(tk.mul(cos_pi_n[((k + 1) * d) % (2 * n)]));
            }
            s.mul_f(2.0 * r).add(Dd::ONE).div_f(n as f64).hi
        })
        .collect();
    let peak = w.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let inv = 1.0 / peak;
    for v in &mut w {
        *v *= inv;
    }
    w
}

/// T_m(x) and its derivative m U_{m-1}(x), by the three-term recurrences.
#[cfg(test)]
fn chebyshev_t(x: Dd, m: usize) -> (Dd, Dd) {
    let two_x = x.mul_f(2.0);
    let (mut t0, mut t1) = (Dd::ONE, x);
    let (mut u0, mut u1) = (Dd::ONE, two_x);
    for _ in 2..=m {
        let t2 = two_x.mul(t1).add(t0.neg());
        let u2 = two_x.mul(u1).add(u0.neg());
        (t0, t1) = (t1, t2);
        (u0, u1) = (u1, u2);
    }
    // u0 is U_{m-1} after the loop.
    (t1, u0.mul_f(m as f64))
}

/// Pairwise summation: leaves of up to 128 values summed with eight
/// interleaved accumulators, longer runs split in two at a multiple of 8.
pub(super) fn pairwise_sum(a: &[f64]) -> f64 {
    let n = a.len();
    if n < 8 {
        let mut r = 0.0;
        for &v in a {
            r += v;
        }
        r
    } else if n <= 128 {
        let mut r = [0.0; 8];
        r.copy_from_slice(&a[..8]);
        let mut i = 8;
        while i < n - n % 8 {
            for j in 0..8 {
                r[j] += a[i + j];
            }
            i += 8;
        }
        let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
        while i < n {
            res += a[i];
            i += 1;
        }
        res
    } else {
        let mut n2 = n / 2;
        n2 -= n2 % 8;
        pairwise_sum(&a[..n2]) + pairwise_sum(&a[n2..])
    }
}

/// Phase blend of the intermediate-phase stage (0 is minimum phase).
pub(super) const ASYM_P: f64 = 0.8;

/// The cepstral phase transform of a stage kernel `h`:
///
/// 1. M = 2^(bitlen(len) + 4) (4096 for 255 taps).
/// 2. L = log|FFT_M(h)| on bins 0..=M/2, 0 where |X| <= 2^-52.
/// 3. Real cepstrum c = IFFT(L); c[1..M/2] doubled, c[M/2+1..] zeroed.
/// 4. Y = FFT(c); phase (1 - p) Im Y - p k pi/2 (the second term is a
///    delay of p M/4 samples), magnitude exp(Re Y).
/// 5. g = IFFT of that, length M.
///
/// `p == 0` keeps g[0..len] (minimum phase). Otherwise the kernel is cut
/// out of the circular g around its largest value: walking down from the
/// peak, the first sample whose difference to the sample before it is at
/// most `t` starts the kernel; walking up, the first sample whose
/// difference to the sample after it is at most `t` ends it (both kept);
/// `t` is 2^-32 for a length setting up to 1024, else 2^-52. The cut
/// depends on where two neighbours happen to differ by less than `t`, so
/// different rounding upstream can move it by a sample.
///
/// The kernels have deep stopband nulls, so the log magnitude is very
/// sensitive to the rounding of the first FFT: that one is a direct DFT
/// in double-double arithmetic (rounded once to double at the log). The
/// rest is ordinary double FFTs.
pub(super) fn cepstral(h: &[f64], p: f64, len_setting: usize) -> Vec<f64> {
    let len = h.len();
    let m = 1usize << (usize::BITS - len.leading_zeros() + 4);
    let half = m / 2;

    let logmag = log_magnitude_dd(h, m);

    let mut fft = RealFftPair::new(m);
    let mut spec: Vec<Complex64> = logmag.iter().map(|&l| Complex64::new(l, 0.0)).collect();
    let mut c = vec![0.0; m];
    fft.inverse_complex_normalized(&mut spec, &mut c);
    for v in &mut c[1..half] {
        *v *= 2.0;
    }
    for v in &mut c[half + 1..] {
        *v = 0.0;
    }
    fft.forward_padded_complex(&c, &mut spec);
    for (k, y) in spec.iter_mut().enumerate() {
        let phi = (1.0 - p) * y.im - (p * k as f64) * (PI / 2.0);
        let e = y.re.exp();
        *y = Complex64::new(e * phi.cos(), e * phi.sin());
    }
    let mut g = vec![0.0; m];
    fft.inverse_complex_normalized(&mut spec, &mut g);

    if p < f64::EPSILON {
        g.truncate(len);
        return g;
    }
    let t = if len_setting <= 1024 {
        2f64.powi(-32)
    } else {
        f64::EPSILON // 2^-52
    };
    let pk = argmax(&g);
    let at = |i: isize| g[i.rem_euclid(m as isize) as usize];
    let mut b = pk as isize - 1;
    while (at(b) - at(b - 1)).abs() > t {
        b -= 1;
    }
    let mut e = pk as isize + 1;
    while (at(e) - at(e + 1)).abs() > t {
        e += 1;
    }
    (b..=e).map(at).collect()
}

/// Index of the first largest (signed) value.
pub(super) fn argmax(v: &[f64]) -> usize {
    let mut best = 0;
    for (i, &x) in v.iter().enumerate() {
        if x > v[best] {
            best = i;
        }
    }
    best
}

/// `log|X[k]|` for k = 0..=m/2, X the length-`m` DFT of `h` zero-padded,
/// with the DFT and the squared magnitude in double-double; 0 where
/// |X| <= 2^-52.
fn log_magnitude_dd(h: &[f64], m: usize) -> Vec<f64> {
    let floor_sq = 2f64.powi(-104);
    dft_dd_raw(h, m)
        .into_iter()
        .map(|(re, im)| {
            let sq = re.mul(re).add(im.mul(im));
            if sq.hi > floor_sq {
                0.5 * (sq.hi.ln() + sq.lo / sq.hi)
            } else {
                0.0
            }
        })
        .collect()
}

/// The length-`m` DFT (e^{-i w n}) of `x` zero-padded, bins 0..=m/2, in
/// double-double and rounded once: (real parts, imaginary parts).
pub(super) fn dft_dd(x: &[f64], m: usize) -> (Vec<f64>, Vec<f64>) {
    dft_dd_raw(x, m)
        .into_iter()
        .map(|(re, im)| (re.hi, im.hi))
        .unzip()
}

fn dft_dd_raw(x: &[f64], m: usize) -> Vec<(Dd, Dd)> {
    assert!(x.len() <= m && m.is_multiple_of(8));
    let tw = twiddles_dd(m);
    (0..=m / 2)
        .map(|k| {
            let mut re = Dd::ZERO;
            let mut im = Dd::ZERO;
            for (n, &v) in x.iter().enumerate() {
                let (c, s) = tw[(k * n) % m];
                re = re.add(c.mul_f(v));
                im = im.add(s.mul_f(-v));
            }
            (re, im)
        })
        .collect()
}

/// `(cos, sin)(2 pi j / m)` for j = 0..m in double-double (m a multiple
/// of 8): Taylor series on the first octant, symmetry for the rest.
fn twiddles_dd(m: usize) -> Vec<(Dd, Dd)> {
    let q = m / 4;
    let o = m / 8;
    let oct: Vec<(Dd, Dd)> = (0..=o)
        .map(|j| {
            let x = Dd::PI.mul_f(j as f64 / (m / 2) as f64);
            (cos_dd(x), sin_dd(x))
        })
        .collect();
    (0..m)
        .map(|j| {
            let (quad, r) = (j / q, j % q);
            let (c, s) = if r <= o {
                oct[r]
            } else {
                (oct[q - r].1, oct[q - r].0)
            };
            match quad {
                0 => (c, s),
                1 => (s.neg(), c),
                2 => (c.neg(), s.neg()),
                _ => (s, c.neg()),
            }
        })
        .collect()
}

fn sin_dd(x: Dd) -> Dd {
    let x2 = x.mul(x);
    let mut term = x;
    let mut sum = x;
    for k in 1..40 {
        term = term.mul(x2).div_f(((2 * k) * (2 * k + 1)) as f64).neg();
        sum = sum.add(term);
        if term.hi.abs() < 1e-36 {
            break;
        }
    }
    sum
}

fn cos_dd(x: Dd) -> Dd {
    let x2 = x.mul(x);
    let mut term = Dd::ONE;
    let mut sum = Dd::ONE;
    for k in 1..40 {
        term = term.mul(x2).div_f(((2 * k - 1) * (2 * k)) as f64).neg();
        sum = sum.add(term);
        if term.hi.abs() < 1e-36 {
            break;
        }
    }
    sum
}

/// Double-double number `hi + lo`, |lo| <= ulp(hi) / 2.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Dd {
    hi: f64,
    lo: f64,
}

impl Dd {
    const ZERO: Dd = Dd { hi: 0.0, lo: 0.0 };
    const ONE: Dd = Dd { hi: 1.0, lo: 0.0 };
    const PI: Dd = Dd {
        hi: std::f64::consts::PI,
        lo: 1.224_646_799_147_353_2e-16,
    };

    fn from(x: f64) -> Dd {
        Dd { hi: x, lo: 0.0 }
    }

    fn sqrt(self) -> Dd {
        if self.hi <= 0.0 {
            return Dd::ZERO;
        }
        let s = self.hi.sqrt();
        let p = s * s;
        let r = self.add(Dd {
            hi: -p,
            lo: -s.mul_add(s, -p),
        });
        Dd::quick(s, r.hi / (2.0 * s))
    }

    fn div(self, o: Dd) -> Dd {
        let q1 = self.hi / o.hi;
        let r = self.add(o.mul_f(q1).neg());
        let q2 = r.hi / o.hi;
        let r = r.add(o.mul_f(q2).neg());
        Dd::quick(q1, q2).add(Dd {
            hi: r.hi / o.hi,
            lo: 0.0,
        })
    }

    fn two_sum(a: f64, b: f64) -> Dd {
        let s = a + b;
        let bb = s - a;
        Dd {
            hi: s,
            lo: (a - (s - bb)) + (b - bb),
        }
    }

    fn quick(a: f64, b: f64) -> Dd {
        let s = a + b;
        Dd {
            hi: s,
            lo: b - (s - a),
        }
    }

    fn add(self, o: Dd) -> Dd {
        let s = Dd::two_sum(self.hi, o.hi);
        let t = Dd::two_sum(self.lo, o.lo);
        let r = Dd::quick(s.hi, s.lo + t.hi);
        Dd::quick(r.hi, r.lo + t.lo)
    }

    fn neg(self) -> Dd {
        Dd {
            hi: -self.hi,
            lo: -self.lo,
        }
    }

    fn mul(self, o: Dd) -> Dd {
        let p = self.hi * o.hi;
        let e = self.hi.mul_add(o.hi, -p);
        Dd::quick(p, e + (self.hi * o.lo + self.lo * o.hi))
    }

    fn mul_f(self, b: f64) -> Dd {
        let p = self.hi * b;
        let e = self.hi.mul_add(b, -p);
        Dd::quick(p, e + self.lo * b)
    }

    fn div_f(self, b: f64) -> Dd {
        let q1 = self.hi / b;
        let p = q1 * b;
        let r = self.add(Dd {
            hi: -p,
            lo: -q1.mul_add(b, -p),
        });
        Dd::quick(q1, r.hi / b)
    }
}

/// Length of a frequency-sampled stage (taps per stage, any factor).
pub(super) const FFT_STAGE_LEN: usize = 512;

/// Cutoff constant of the frequency-sampled stage, about 0.95 of the
/// stage input Nyquist. It is the double 0x3FEE666666000000 (0.95 with
/// its low mantissa bits cleared), not 0.95.
const FFT_STAGE_CUTOFF: f64 = 0.949_999_999_254_941_9;

/// The frequency-sampled stage kernel for factor `f`, unit DC gain (to
/// about 1e-11): bins 0..nb of the length-`n` real spectrum set to 1 (nb =
/// floor((n/2 + 1) * ((1/f) * the cutoff constant)), 122 for x2 and 81
/// for x3 at n = 512), the inverse DFT with 1/n,
///
/// ```text
/// t[i] = (1 + 2 sum_{k=1..nb-1} cos(2 pi k i / n)) / n,
/// ```
///
/// halves swapped so the peak sits at n/2, times a periodic Kaiser window
/// with beta 7 pi. `t` and the window are evaluated in double-double and
/// rounded once (an FFT or a double I0 series would be off by up to a
/// few ulps, which shows up at the 32-bit LSB level on long signals).
pub(super) fn fft_stage_kernel(f: usize) -> Vec<f64> {
    let n = FFT_STAGE_LEN;
    let k = n / 2 + 1;
    let fc = (1.0 / f as f64) * FFT_STAGE_CUTOFF;
    let nb = (k as f64 * fc).floor() as usize;
    let tw = twiddles_dd(n);
    let t: Vec<f64> = (0..n)
        .map(|i| {
            let mut s = Dd::ZERO;
            for k in 1..nb {
                s = s.add(tw[(k * i) % n].0);
            }
            s.mul_f(2.0).add(Dd::ONE).div_f(n as f64).hi
        })
        .collect();
    let w = kaiser_periodic_dd(n, 7.0, PI);
    (0..n).map(|i| w[i] * t[(i + n / 2) % n]).collect()
}

/// Periodic Kaiser window `I0(beta sqrt(1 - (2i/n - 1)^2)) / I0(beta)`,
/// beta = `alpha * pi` with `pi` the double, as
/// [`crate::design::paths::fir::windows::kaiser_periodic`] but in double-double
/// (65-term I0 series) and rounded once.
fn kaiser_periodic_dd(n: usize, alpha: f64, pi: f64) -> Vec<f64> {
    let beta = Dd::from(pi).mul_f(alpha);
    let i0 = |z: Dd| {
        let half = z.mul_f(0.5);
        let mut term = Dd::ONE;
        let mut sum = Dd::ONE;
        for k in 1..65 {
            term = term.mul(half).div_f(k as f64);
            sum = sum.add(term.mul(term));
        }
        sum
    };
    let den = i0(beta);
    (0..n)
        .map(|i| {
            let x = 2.0 * i as f64 / n as f64 - 1.0;
            let r = Dd::from(1.0 - x * x).sqrt(); // 1 - x^2 is exact here
            i0(beta.mul(r)).div(den).hi
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairwise_sum_matches_plain_sum_closely() {
        let v: Vec<f64> = (0..255).map(|i| ((i * 37 % 101) as f64).sin()).collect();
        let plain: f64 = v.iter().sum();
        assert!((pairwise_sum(&v) - plain).abs() < 1e-13);
        assert_eq!(pairwise_sum(&[1.0, 2.0, 3.0]), 6.0);
    }

    #[test]
    fn dd_twiddles_are_accurate() {
        let m = 4096;
        let tw = twiddles_dd(m);
        for j in [0usize, 1, 511, 512, 513, 1024, 1500, 2048, 3000, 4095] {
            let (c, s) = tw[j];
            let x = 2.0 * PI * j as f64 / m as f64;
            assert!((c.hi - x.cos()).abs() < 2e-15, "cos {j}");
            assert!((s.hi - x.sin()).abs() < 2e-15, "sin {j}");
            // cos^2 + sin^2 = 1 to double-double precision
            let one = c.mul(c).add(s.mul(s)).add(Dd::ONE.neg());
            assert!(one.hi.abs() < 1e-30, "j {j}: {}", one.hi);
        }
        // sin(pi/4) = sqrt(2)/2 to ~1e-32
        let (c, s) = tw[512];
        let d = c.add(s.neg());
        assert!(d.hi.abs() < 1e-31);
    }

    #[test]
    fn window_table_is_the_chebyshev_window() {
        // Against the window in double-double (exact to the last bit of a
        // double): the table's extended-precision rounding is at most
        // 6e-12 relative, the plain double sum 4e-9.
        let t = fir_window();
        let exact = chebyshev_window(255, FIR_CHEBYSHEV_R);
        let dbl = crate::design::paths::fir::windows::chebyshev_symmetric(255, FIR_CHEBYSHEV_R);
        for i in 0..255 {
            assert!(((t[i] - exact[i]) / exact[i]).abs() < 6e-12, "{i}");
            assert!(((dbl[i] - exact[i]) / exact[i]).abs() < 4e-9, "{i}");
            assert_eq!(t[i], t[254 - i]);
        }
        assert!((exact[127] - 1.0).abs() <= f64::EPSILON);
        // End taps of the exact window (also checked at 50 digits).
        assert_eq!(exact[0], 4.210_155_899_116_507e-6);
        assert_eq!(exact[1], 4.961_914_493_087_708e-6);
    }

    #[test]
    fn prototype_has_unit_dc_gain_and_symmetry() {
        for f in [2, 3] {
            let h = fir_prototype(f);
            assert_eq!(h.len(), 255);
            assert!((h.iter().sum::<f64>() - 1.0).abs() < 1e-14);
            for i in 0..127 {
                assert!((h[i] - h[254 - i]).abs() < 1e-16);
            }
            assert_eq!(argmax(&h), 127);
        }
        // Peak of the x2 prototype: the cutoff over the DC sum.
        let h = fir_prototype(2);
        assert!((h[127] - 0.476755).abs() < 1e-6);
    }

    #[test]
    fn kaiser_dd_agrees_with_the_double_version() {
        let a = kaiser_periodic_dd(512, 7.0, PI);
        let b = crate::design::paths::fir::windows::kaiser_periodic(512, 7.0 * PI);
        for i in 0..512 {
            assert!((a[i] - b[i]).abs() <= 1e-14 * a[i].max(1e-300), "{i}");
        }
        assert_eq!(a[256], 1.0);
        // Values checked against a 45-digit evaluation (correctly rounded).
        assert_eq!(a[0], 3.288_908_901_952_998e-9);
        assert_eq!(a[1], 7.201_320_186_387_285e-9);
        assert_eq!(a[100], 0.011_829_594_044_255_277);
    }

    #[test]
    fn fft_stage_bins_and_peak() {
        // nb = 122 for x2 and 81 for x3; the peak is (2 nb - 1) / n.
        let h2 = fft_stage_kernel(2);
        assert_eq!(h2[256], 243.0 / 512.0);
        let h3 = fft_stage_kernel(3);
        assert!((h3[256] - 161.0 / 512.0).abs() < 1e-15);
        assert!((h2.iter().sum::<f64>() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn minimum_phase_keeps_the_magnitude() {
        let h = fir_prototype(2);
        let g = cepstral(&h, 0.0, FIR_LEN_SETTING);
        assert_eq!(g.len(), 255);
        // DC gain kept and energy front-loaded.
        assert!((g.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        let e: f64 = g.iter().map(|v| v * v).sum();
        let e_head: f64 = g[..64].iter().map(|v| v * v).sum();
        assert!(e_head > 0.99 * e);
    }

    #[test]
    fn intermediate_phase_trim_gives_289_taps() {
        let h = fir_prototype(2);
        let g = cepstral(&h, ASYM_P, FIR_LEN_SETTING);
        assert_eq!(g.len(), 289);
        assert_eq!(argmax(&g), 133);
    }
}
