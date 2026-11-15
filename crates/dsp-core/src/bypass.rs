//! Bit-exact sample conversions for the passthrough path.
//!
//! PipeWire hands us f32. Every 16/24-bit integer sample is exactly
//! representable in f32 as `n / 2^(bits-1)`, so the conversion back to a
//! left-justified i32 must reproduce the original integer exactly.

/// f32 in [-1, 1) → left-justified i32 (`x * 2^31`), saturating at the ends.
/// Exact for every input that came from a <= 24-bit integer.
///
/// Non-grid inputs are rounded to the nearest i32.  NaN maps to 0 and
/// infinities saturate to `i32::MIN`/`i32::MAX`, matching Rust's saturating
/// float-to-int casts.
pub fn f32_to_i32(input: &[f32], out: &mut [i32]) {
    for (o, &x) in out.iter_mut().zip(input) {
        // `x as f64` is exact.  Multiplying by 2^31 is exact (power of two).
        // `.round()` gives round-half-away-from-zero; the saturating `as i32`
        // cast then maps +1.0 to i32::MAX, -1.0 to i32::MIN, over-range to the
        // ends and NaN to 0.
        *o = ((x as f64) * 2_147_483_648.0).round() as i32;
    }
}

/// i32 (left-justified) → f32, exact for samples with <= 24 significant bits.
pub fn i32_to_f32(input: &[i32], out: &mut [f32]) {
    for (o, &x) in out.iter_mut().zip(input) {
        // Division by a power of two is exact; an f32 has a 24-bit
        // significand, so every <= 24-bit integer sample survives intact.
        *o = (x as f64 / 2_147_483_648.0) as f32;
    }
}

/// Bit depth actually needed to represent `x` exactly (0..=32; 0 for 0.0).
///
/// Defined as the smallest `b` such that `x * 2^(b-1)` is an integer, i.e. the
/// number of bits in `x` when written as `n / 2^(b-1)` (two's-complement
/// fractional form).  A 24-bit sample such as `1/2^23` or `(2^23-1)/2^23`
/// reports 24.  Values that would need more than 32 bits (tiny denormals) and
/// non-finite values report 32.
pub fn significant_bits(x: f32) -> u32 {
    let bits = x.to_bits();
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x007f_ffff;

    if exponent == 0xff {
        // NaN or infinity.
        return 32;
    }
    if exponent == 0 {
        // Zero or subnormal: value = mantissa * 2^-149.
        if mantissa == 0 {
            return 0;
        }
        let e = -149 + mantissa.trailing_zeros() as i32;
        return (1 - e).clamp(1, 32) as u32;
    }

    // Normal: value = (2^23 + mantissa) * 2^(exponent - 150).
    let significand = mantissa | 0x0080_0000;
    let e = exponent - 150 + significand.trailing_zeros() as i32;
    (1 - e).clamp(1, 32) as u32
}

/// 64-bit FNV-1a over the little-endian bytes of `samples`, for self-tests.
pub fn hash_i32(samples: &[i32]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &sample in samples {
        for byte in sample.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small deterministic LCG so the test needs no `rand` dependency.
    struct Lcg(u64);
    impl Lcg {
        fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 32) as u32
        }
    }

    #[test]
    fn exhaustive_16_bit_round_trip() {
        // Every 16-bit value, left-justified exactly as the I/O layer stores it.
        let mut ints = Vec::with_capacity(65_536);
        for n in -32_768i32..=32_767 {
            ints.push(n << 16);
        }
        let mut floats = vec![0f32; ints.len()];
        i32_to_f32(&ints, &mut floats);
        // The float values must match n/2^15 exactly.
        for (f, expected) in floats.iter().zip(&ints) {
            assert_eq!(*f, *expected as f32 / 2_147_483_648.0);
        }
        let mut back = vec![0i32; ints.len()];
        f32_to_i32(&floats, &mut back);
        assert_eq!(back, ints);
        // And the float round-trip is bit-exact.
        let mut again = vec![0f32; ints.len()];
        i32_to_f32(&back, &mut again);
        for (a, b) in floats.iter().zip(&again) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    #[test]
    fn random_24_bit_round_trip() {
        let mut rng = Lcg(0x1234_5678_9abc_def0);
        let mut ints = Vec::with_capacity(100_000);
        let mut floats = Vec::with_capacity(100_000);
        for _ in 0..100_000 {
            // Uniform over [-2^23, 2^23).
            let n = (rng.next_u32() % (1 << 24)) as i32 - (1 << 23);
            let left = n << 8;
            let f = n as f32 / 8_388_608.0;
            let mut single = [0i32; 1];
            f32_to_i32(&[f], &mut single);
            assert_eq!(single[0], left, "n={n}");
            ints.push(left);
            floats.push(f);
        }
        // Whole-buffer API, both directions.
        let mut out_i = vec![0i32; ints.len()];
        f32_to_i32(&floats, &mut out_i);
        assert_eq!(out_i, ints);
        let mut out_f = vec![0f32; ints.len()];
        i32_to_f32(&ints, &mut out_f);
        assert_eq!(out_f, floats);
        // Round-trip i32 -> f32 -> i32 is bit-exact.
        let mut back = vec![0i32; ints.len()];
        f32_to_i32(&out_f, &mut back);
        assert_eq!(back, ints);
    }

    #[test]
    fn edge_values() {
        let cases: [(f32, i32); 8] = [
            (0.0, 0),
            (-0.0, 0),
            (1.0, i32::MAX),
            (-1.0, i32::MIN),
            (f32::from_bits(1.0f32.to_bits() - 1), 2_147_483_520), // just below 1.0
            (f32::from_bits((-1.0f32).to_bits() - 1), -2_147_483_520), // just inside -1.0
            (f32::NAN, 0),
            (f32::INFINITY, i32::MAX),
        ];
        for (x, expected) in cases {
            let mut out = [0i32; 1];
            f32_to_i32(&[x], &mut out);
            assert_eq!(out[0], expected, "x={x}");
        }
        let mut out = [0i32; 1];
        f32_to_i32(&[f32::NEG_INFINITY], &mut out);
        assert_eq!(out[0], i32::MIN);

        // Round-trip of the values just inside the range.
        let just_below_one = f32::from_bits(1.0f32.to_bits() - 1);
        let mut i = [0i32; 1];
        f32_to_i32(&[just_below_one], &mut i);
        let mut f = [0f32; 1];
        i32_to_f32(&i, &mut f);
        assert_eq!(f[0].to_bits(), just_below_one.to_bits());
    }

    #[test]
    fn significant_bits_values() {
        assert_eq!(significant_bits(0.0), 0);
        assert_eq!(significant_bits(-0.0), 0);
        assert_eq!(significant_bits(0.5), 2);
        assert_eq!(significant_bits(-0.5), 2);
        assert_eq!(significant_bits(0.25), 3);
        assert_eq!(significant_bits(0.75), 3);
        assert_eq!(significant_bits(1.0), 1);
        assert_eq!(significant_bits(-1.0), 1);
        // 24-bit sample magnitudes.
        assert_eq!(significant_bits(1.0 / 8_388_608.0), 24);
        assert_eq!(significant_bits((8_388_607.0f32) / 8_388_608.0), 24);
        // 16-bit sample magnitudes.
        assert_eq!(significant_bits(1.0 / 32_768.0), 16);
        assert_eq!(significant_bits(32_767.0 / 32_768.0), 16);
        // A value that needs 32 bits of fraction.
        assert_eq!(significant_bits(1.0 / 2_147_483_648.0), 32);
        assert_eq!(significant_bits(f32::NAN), 32);
        assert_eq!(significant_bits(f32::INFINITY), 32);
    }

    #[test]
    fn hash_is_fnv1a_64() {
        // Known FNV-1a 64 vectors over the little-endian bytes.
        assert_eq!(hash_i32(&[]), 0xcbf2_9ce4_8422_2325);
        assert_eq!(hash_i32(&[0]), 0x4d25_767f_9dce_13f5);
        assert_eq!(hash_i32(&[1]), 0xad2a_ca77_4798_5764);
        assert_eq!(hash_i32(&[-1]), 0x994f_7665_3e2a_3951);
        // Deterministic, and sensitive to every sample.
        let samples: Vec<i32> = (0..1024).map(|i| i * 7919 - 12345).collect();
        let h = hash_i32(&samples);
        assert_eq!(h, hash_i32(&samples));
        let mut changed = samples.clone();
        changed[512] = changed[512].wrapping_add(1);
        assert_ne!(h, hash_i32(&changed));
    }
}
