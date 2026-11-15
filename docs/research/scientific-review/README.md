# Scientific review: tail energy and phase

Scope: `git diff b0b21cd..be3370513aae9774dfa44f3fde8099f5e34173bc -- audivane-dsp/crates/dsp-core/src/resample.rs`, with the surrounding designer and response measurement. No tracked source changes, commits, or live PipeWire operations. Standalone experiments and raw logs are in this directory.

## Verdicts

1. **Minimum-phase floor: wrong as a complete implementation of the stated same-magnitude/minimum-phase contract.** Log regularization is sound, and the change improves beta=8/14 passband matching and recovers beta=20 stopband depth. However, its new floors change deep-stopband magnitudes substantially; the FFT/truncation does not reliably preserve even the unfloored passband magnitude or minimum phase. The beta=20 change increases near-edge passband error relative to the previous implementation.
2. **DFT FMA: sound.** Error-free multiplication residual followed by fractional reduction removes the length-dependent phase loss. Remaining measured error is approximately -271 to -278 dB relative to passband for the 4M-tap checks, comfortably below -190 dB. This is a measured error envelope on the stated samples, not a universal rigorous noise bound.
3. **Nyquist-stop cutoff: wrong as an attenuation guarantee; its dimensional derivation is correct.** k=1.2 is an empirical selection, and the current minimum-phase implementation exceeds the nominal target even at power-of-two lengths. Accepted non-power-of-two lengths are substantially worse. The clamp preserves a passband but cannot preserve the attenuation promise.

## Method and reproduction

The research crate directly calls the production `design_kernel`, `dft_magnitudes`, `nyquist_stop_cutoff`, and `measure_upsampler`. The old designer is reproduced by changing only the floor back to -180 dB, with the same linear kernel, cutoff and FFT length. Comparisons to the previous implementation use power-of-two lengths, where both implementations select 4N FFTs.

The separate C reference uses 64-bit-mantissa long double trigonometry, block dot products, sums, and compensated long-double product reduction. Compensation matters even in the reference: multiplying a 53-bit input frequency by a 14-bit block index can exceed a 64-bit mantissa. `fmal` preserves that residual. Reference frequencies are the exact stored f64 cycles/sample; division rounding in f/rate remains an ordinary frequency-coordinate rounding, below roughly 2e-11 Hz at a 768 kHz rate. Its effect on crossings is negligible here.

Run from `audivane-dsp/`:

```sh
cargo test --workspace
cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- small
cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- big 4194304
cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- fraction 16384
REVIEW_CUTOFF=0.95 cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- phase 4194304
REVIEW_FFT_PAD=128 REVIEW_ROOTS=1 cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- phase
cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- dense
REVIEW_POINTS=256 REVIEW_SPAN=0.1 cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- dense 4194304
REVIEW_POINTS=256 REVIEW_SPAN=0.1 cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- dense 4194303 20
REVIEW_DFT_GRID=1 cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- dft 4194304 20 2
REVIEW_DFT_GRID=1 cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- dft 4194304 20 16
cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- measure 4194304 20
```

Defaults: ratio=16, beta in [8,14,20], N=16384, Nyquist-stop cutoff. Arguments: mode, N, optional beta, optional ratio. `REVIEW_CUTOFF` selects a fixed source-Nyquist fraction, `REVIEW_K` overrides k for experiments. `REVIEW_FFT_PAD` controls measurement FFT density, not production design padding. `small` additionally compares cepstral padding 16N and 64N; avoid using that mode at 4M unless the memory cost is intended. All large kernels used release builds.

## 1. Floor and homomorphic conversion

The homomorphic derivation is correct: inverse-transform log magnitude, retain c[0], double the positive-time cepstrum, retain the FFT-midpoint coefficient once, zero negative times, exponentiate the forward transform, inverse-transform. See [Julius O. Smith's derivation](https://www.dsprelated.com/freebooks/sasp/Minimum_Phase_Causal_Cepstra.html). Flooring prevents log(0), but the target becomes max(|H|, epsilon). It is no longer the original magnitude; hard clipping also leaves derivative discontinuities and an infinite cepstral/time tail. Finite FFT folding and retaining only N impulse samples introduce further errors. There is no theorem that padding just beyond 2N eliminates those tails.

The new floor is -(A+20) dB: **-101.2953 / -155.7417 / -210.1882** for beta=8/14/20. Twenty dB means epsilon is 0.1 of the nominal stopband amplitude; it is a heuristic, not a same-magnitude accuracy guarantee. At N=4M, f/f_out=0.123456789, beta=8, the linear magnitude is approximately -211 dB, the old minimum-phase magnitude -180.28 dB and the new minimum-phase magnitude -101.295 dB. The raised floor is an approximately 79 dB change to the old deep stopband.

At ratio=16, N=4,194,304, the old/new maximum passband magnitude differences in dB are:

| beta | previous floor | new floor | new transition amplitude error |
|---|---:|---:|---:|
| 8 | 2.14019 | 0.00571045 | 0.00157514 |
| 14 | 0.0358520 | 0.00214336 | 0.000471380 |
| 20 | 0.000235673 | 0.00150412 | 0.000566362 |

Passband here extends through fc - 0.6*Delta_f; transition is fc +/- 0.6*Delta_f. Amplitudes are normalized to the interpolation gain. Errors are concentrated near the edge: below 0.9fc, the new 4M beta=20 difference is only 3.3e-11 dB or less in the Fraction=0.95 check, and similarly tiny for Nyquist-stop. Do not interpret the near-edge errors as broadband audible ripple.

The existing Fraction mode is affected: at cutoff=0.95, N=4M, the linear/new-minimum worst levels above source Nyquist are **-148.695/-101.291**, **-198.317/-155.740**, **-248.823/-210.184 dB** for beta=8/14/20. Near-edge passband differences are 0.01036/0.002721/0.001744 dB. Thus the new low-beta floor materially changes magnitude outside the new cutoff mode too.

Increasing cepstral padding confirms aliasing rather than f64 noise. At 16k, beta=20, Nyquist-stop, padding 4N -> 16N reduces passband difference **0.0012754 -> 0.0000009709 dB** and transition amplitude difference **5.194e-4 -> 5.893e-7**. At beta=8, 4N -> 64N reduces near-edge passband difference **0.005845 -> 7.825e-7 dB**. Sampling density accounts for the small difference between these 8N measurement-grid numbers and the denser 128N measurements in `roots.log`.

The FFT expression at resample.rs:116 selects **16,777,216**, not 8,388,608, points for N=4,194,304. For N=4,194,303 it selects only **8,388,608**: a discontinuity from about 4N to about 2N. This is not safe. At the latter length, beta=20, minimum-phase peak above source Nyquist is **-179.664 dB**, versus a **-190.188 dB** target; linear phase remains **-190.834 dB**. At N=32,767 the corresponding minimum-phase peaks are -75.690/-130.214/-179.671 dB for beta=8/14/20. Power-of-two-only verification misses this.

Energy is strongly front loaded: at 16k, 50% energy indices are 51/75/100, 99% indices 1274/1663/1945, and DC group delays 37.73/58.07/78.47 samples for beta=8/14/20, versus linear delay 8191.5. But front loading is not proof of minimum phase. At 128N response sampling, full-circle winding is -6/-6/-20. Long-double Newton/Horner evaluation independently finds external zeros; e.g. beta=20 has a root with **|z|-1=4.7495e-5**, normalized polynomial residual approximately **3.3e-19**. Beta=8 external roots have |z|-1 around 1.2e-5. The returned truncated FIR is not strictly minimum phase. Counts from coarser grids can underestimate the number of external zeros; the explicit roots suffice for this finding.

For the requested beta range there is no log underflow or precision barrier at 2^23/2^24 FFT sizes: the deepest epsilon is approximately 3.09e-11 relative to peak, far above f64 FFT roundoff. DC and Nyquist cepstral coefficients are handled correctly. Even-length linear kernels have exactly zero Nyquist response by symmetry; the floored minimum-phase target necessarily changes this to a nonzero response.

**Fix:** define an explicit magnitude-error tolerance, including the accepted stopband floor. Make padding depend on convergence/tail error and never silently drop to 2N for accepted non-power-of-two lengths. At least 4N is necessary for the demonstrated discontinuity but not sufficient to assert exact magnitude/minimum phase. The 16N/64N experiments establish useful convergence, not a universal minimum-phase proof. If a strict minimum-phase FIR is required, validate/factor the final FIR rather than inferring it from the untruncated causal cepstrum. Include both Fraction and Nyquist-stop regressions and off-power-of-two lengths. Higher padding has substantial memory cost at 4M; preserve that constraint explicitly when selecting the algorithm.

## 2. DFT precision

With normal, finite operands, p_hi=fl(c*n0) and p_lo=fma(c,n0,-p_hi) retain the exact product as a two-term expansion. n0 is an exactly represented integer. Subtracting floor(p_hi) and adding p_lo then reduces the exact stored frequency product modulo one with rounding at the remainder scale. A small negative remainder is harmless to sin/cos periodicity. The final multiplication by 2*pi, short-table generation, and sums still round, but there is no million-sample-sized product left in the sin/cos argument.

For N=4,194,304, beta=20, 301 frequencies per ratio, including transition/stopband points and a 256-point broad stopband grid:

| ratio | old worst complex error | new worst complex error | public API magnitude error |
|---|---:|---:|---:|
| 2 | -190.88 dB | -271.06 dB | -271.73 dB |
| 16 | -207.02 dB | -277.62 dB | -277.79 dB |

All errors are divided by passband interpolation gain. The remaining sampled error is at most roughly 2.8e-14 normalized amplitude, over 80 dB below a -190 dB signal. Exact nulls and responses below approximately -270 dB should not be interpreted as accurately measured depths. The old error can inflate or cancel sidelobes: ratio=2 at c=0.2500012102781014 has true response -194.7767 dB, old evaluation -206.0500 dB, new evaluation -194.7769 dB. The previous '-185 dB floor' is a scale-dependent estimate, not a fixed floor at every frequency.

The public measurement's 13 crossing levels, -0.0001 through -150 dB, agree with long-double evaluation of the designed kernel at the reported frequencies to within **0.000046 dB** for 4M, beta=20, ratio=16, source=48 kHz. These crossings are not limited by DFT noise. See `measure-final.log`.

**Fix:** no correctness fix needed for the FMA change. A future precision regression could test cancellation-prone filters against a higher-precision reference. FMA compensation of the m<=255 table product and compensated summation could lower the remaining error but are unnecessary for the requested -190 dB measurements.

## 3. Nyquist-stop derivation and margin

Kaiser's order estimate is M approximately (A-7.95)/(2.285*Delta_omega). With M=N-1 and Delta_omega=2*pi*Delta_f/f_out, Delta_f/f_out approximately (A-7.95)/(14.36*(N-1)). Subtract k*Delta_f/2 from source Nyquist f_out/(2r) and divide by that Nyquist: **cutoff=1-k*r*(A-7.95)/(14.36*(N-1))**. The ratio and factor of two are correct. The underlying estimate is empirical, and does not promise an exact sidelobe ceiling; [SciPy's Kaiser documentation explicitly demonstrates a first-lobe violation](https://docs.scipy.org/doc/scipy/reference/generated/scipy.signal.kaiserord.html).

Denser direct evaluations show that the table's apparent minimum-phase margin is misleading. The tests allow -A+0.5 dB, and 128 points over 4*Delta_f can miss peaks. Measured local peak levels at/above Nyquist (ratio=16):

| N | beta | target -A | linear | minimum |
|---:|---:|---:|---:|---:|
| 16,384 | 8 | -81.2953 | -82.7021 | -81.4733 |
| 16,384 | 14 | -135.7417 | -136.7619 | **-135.5970** |
| 16,384 | 20 | -190.1882 | -190.8920 | **-189.9887** |
| 4,194,304 | 8 | -81.2953 | -82.7316 | -81.5029 |
| 4,194,304 | 14 | -135.7417 | -136.7281 | **-135.5835** |
| 4,194,304 | 20 | -190.1882 | -190.8341 | **-190.0301** |

The 4M scans densely cover the first 0.1*Delta_f above Nyquist, sufficient to find these counterexamples; they are not a global proof of every later sidelobe. The 16k scans cover 4*Delta_f, and 128N FFT checks cover the whole band. Two source rates are scale-equivalent for kernel design; frequencies above are normalized to output rate.

An exact same-magnitude minimum-phase conversion would need no separate margin. Here the extra ~1 dB and sometimes >10 dB come from conversion/truncation error. Fix that before final calibration. k=1.3 did meet the targets for the three 16k/ratio=16 examples (`k13.log`), but that is not a full-domain guarantee. A sound implementation should measure/refine the actual sidelobe envelope or calibrate a conservative margin against the accepted domain after fixing conversion, with a strict -A assertion and peak search.

The lower clamp is only a passband-preservation fallback. For the explicitly tested 64-tap, ratio=16, beta=20 case, cutoff=0.5 yields **-6.963 dB at source Nyquist**, not -190.188 dB. Reject unattainable Nyquist-stop designs or report their inability to meet the target. Returning 0.5 silently is incompatible with the mode's stated guarantee. N=1 is an even simpler infeasible constant-gain case.

There is also a pre-existing measurement limitation at resample.rs:816-872. For 4M, beta=20, ratio=16, source=48 kHz, the API reports stopband start **24,000 Hz**, attenuation **192.007585 dB**, while its own direct measured-impulse API returns **-190.030124 dB at 24,000.082601 Hz**. The fixed 16,384-point statistics grid and sparse plot miss that sidelobe. This is frequency-grid undersampling, not the new DFT's noise floor. Use a grid tied to impulse length/transition width and refine local maxima for stopband summary statistics.

## Required changes

- **Must-fix, resample.rs:116/132/155:** make the floor/magnitude tolerance explicit and fix cepstral/truncation aliasing, including the 2N discontinuity. Add tests for 32,767 and 4,194,303 taps, both cutoff modes, edge magnitude error and final minimum-phase validity.
- **Must-fix, resample.rs:17/2379/2384:** calibrate/verify k against actual peaks after the conversion fix; assert the stated -A target, and remove the +0.5 dB relaxation as evidence for that stricter claim.
- **Must-fix, resample.rs:43:** handle infeasible short designs explicitly; the clamp cannot certify stopband placement.
- **Should-fix, resample.rs:816:** refine stopband summary peaks so the reported attenuation is an envelope; this limitation predates the diff.
- **Should-fix, resample.rs:113:** correct the FFT memory/size comment (16.8M at the 4M power-of-two limit), and remove the unconditional assertion that padding prevents cepstral wrap-around.

`cargo test --workspace`: **359 passed, 0 failed, 9 ignored**, including doc-test results. Full log: `workspace-tests.log`. The ignored 288-case matrix was not rerun; the independent denser counterexamples are stronger evidence against its claimed guarantee.
