# Minimum phase and "Stop at Nyquist": fixes for the scientific review

This folder holds the evidence behind the fixes to the findings in
`../scientific-review/README.md`: the new minimum-phase designer
(`crates/dsp-core/src/minphase.rs`), the recalibrated Nyquist-stop cutoff
(`nyquist_stop_*` in `crates/dsp-core/src/resample.rs`), and the refined
stopband summary. `review-rerun/` is the review's own scratch crate run
against the result. `src/main.rs` is the experiment tool used to choose
the method and constants, and `logs/` holds its output.

Build everything with `--release`. Both crates are standalone (not workspace
members). Run from `audivane-dsp/`:

```sh
# The review's experiments, against the new code
cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- small
cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- big 4194304
REVIEW_CUTOFF=0.95 cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- phase 4194304
REVIEW_FFT_PAD=128 REVIEW_ROOTS=1 cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- phase
REVIEW_POINTS=256 REVIEW_SPAN=0.1 cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- dense 4194303 20
cargo run --release --manifest-path docs/research/scientific-review/Cargo.toml -- measure 4194304 20

# This folder's tool (modes: prod, with, rad, zeros, calib, sweepns, diag, time)
R="cargo run --release --manifest-path docs/research/minimum-phase/Cargo.toml --"
GRID=32 $R prod 16384                       # production design vs linear twin
A0=2 MULT=8 GRID=4 ENVLIM=-250 PROFILE=1 BETAS=20 $R with 4194304   # contour radius study
$R zeros 4096                               # winding counts: zeros of the linear twin vs radius
NS=65536 RS=2 BS=5,5.25,20 $R calib         # smallest k meeting -A (dense peak search)
MAXT=262144 $R sweepns; MIN=1 MAXT=262144 $R sweepns   # Nyquist-stop verification sweep
$R time 4194304 min                         # design time and peak RSS
python3 docs/research/minimum-phase/roots_linear_sweep.py   # numpy roots of the linear twin

# Heavy Rust tests
cargo test -p audivane-dsp-core --release minimum_phase_4m -- --ignored --nocapture
cargo test -p audivane-dsp-core --release nyquist_stop_full_matrix_verification -- --ignored --nocapture
```

## 1. Minimum phase

### Why the old design could not meet its contract

The contract is the same magnitude as the linear-phase twin, minimum phase.
The exact answer exists and has the same length: reflect every zero outside
the unit circle. The old code took the real cepstrum of `max(|H|, floor)` on
the unit circle. A linear-phase low-pass has up to millions of zeros *on*
that circle (`ZEROS`/`on` counts in `logs/roots_linear_sweep.log`), so
`ln|H|` is singular. Its cepstrum decays like `1/n` with a comb at multiples of
the null spacing. Any finite FFT aliases it, and the floor changes the
magnitude.

`src/main.rs` `sweep` reproduces this with a *soft* floor (`|H|^2 + eps^2`,
which at least has an exact N-tap spectral factor) for floors A+20..300 dB and
padding 4N..64N. The error falls only like `L^-2`. Reaching 1e-4 dB in the
passband needs ~100 N points, which is impossible at 4M taps.

### The method: radially scaled complex cepstrum

The fix evaluates the *complex* cepstrum of `H` on the circle `|z| = e^{2/N}`
instead of the unit circle (`g[n] = h[n] e^{-2n/N}`). Polynomial rooting of
short twins (`roots_linear*.py`) and winding counts on long ones
(`zeros` mode, `logs/zeros*.log`) show the linear twin's zero geometry:

| Kaiser beta | nearest off-circle zero to the unit circle |
|---|---|
| 0 | 7.4/N |
| 0.5 to 1.5 (even N) | real pair at -1, 1.25/N to 5/N |
| 2 | 6.0/N |
| 5 | 12.6/N |
| 8 | 15.9/N |
| 14 | 21.1/N |
| 20 | 25.3/N |

The numbers are independent of the cutoff and the ratio. The one exception
is odd lengths: their zero-phase amplitude is stationary at Nyquist, so a
real reciprocal pair can sit arbitrarily close to `z = -1` (0.88/N for 63
taps, ×2, β 8: `roots_odd_nyquist.py`). Those are found by a sign scan on the
negative real axis and reflected exactly before the cepstrum
(`reflect_nyquist_real_zeros`). On the scaled contour every zero is at least
2/N away, so the phase unwraps bin by bin, `ln G` is analytic in an annulus,
and the cepstrum decays geometrically. The reflection `ĉ[n] = ĝ[n] +
ĝ[-n] e^{-4n/N}` and the unscaling are exact. The true twin has exactly N
taps, so truncation drops only residue.

The contour radius trades aliasing against rounding: the unscaling multiplies
rounding error by `e^{2}`. `with` mode, 4,194,304 taps, β 20, L = 8N:

| radius ×N | passband err (dB) | ≥ -100 dB err (dB) | stopband envelope excess (above -250 dB) |
|---|---|---|---|
| 1 | 4.4e-5 | 7.2e-5 | +1.86 dB |
| 1.5 | 2.5e-7 | 1.2e-6 | +4.38 dB |
| **2** | **2.1e-8** | **6.6e-7** | +3.36 dB (at -241 dB, see below) |
| 3 | 1.4e-8 | 6.1e-6 | +7.13 dB |
| 4 | 3.5e-8 | 9.9e-6 | +10.97 dB |

Radius 2/N is used, with `L = next_power_of_two(8 N)`. A smaller radius
would need a larger FFT, which does not fit in the memory budget at 4M.

The f64 floor: at 4M taps the linear twin's own spectrum bottoms out at
about -251 dB re passband (coefficient rounding, `PROFILE=1` output). Deep
in the stopband the rounding noise has zeros of its own near the circle.
`minphase.rs` therefore exempts bins below -220 dB from the unwrapping
step check (either branch only moves a noise zero across the contour) and
floors bins below -320 dB. What limits the result is the rounding of the
cepstral solution itself, amplified up to `e^2` by the unscaling: the
absolute error `| |H_min| - |H_lin| |` above the stopband edge is -212 to
-228 dB re passband at 4M taps for every β, -227 to -239 dB at 1M, and
-249 dB or lower up to 32k (β ≥ 8). The stopband tolerances below are stated
against that floor.

### Result against the review's experiments (`review-rerun/`)

| Check | Before (HEAD, floor A+20) | After |
|---|---|---|
| near-edge passband mismatch, 4M ×16 β 8/14/20 (`big`) | 5.7e-3 / 2.1e-3 / 1.5e-3 dB | 1.6e-8 / 1.6e-8 / 7.7e-9 dB |
| transition amplitude error, 16k β 20 (`small`) | 5.19e-4 | 3.5e-11 |
| cutoff 0.95, 4M, worst level above Nyquist, β 8 (`phase`) | -101.29 dB (linear -148.69) | -148.69 dB |
| same, β 14 | -155.74 dB (linear -198.32) | -198.45 dB |
| same, β 20 | -210.18 dB (linear -248.82) | -245.35 dB (f64 floor, see above) |
| external zeros, 16k (`REVIEW_ROOTS`) | \|z\|-1 up to 4.75e-5 (0.78/N) | ≤ 3.7e-8 (6.1e-4/N), on-circle nulls moved by rounding where \|H\| ≈ -300 dB |
| 4,194,303 taps β 20, peak above Nyquist (`dense`) | -179.664 dB (target -190.188) | -191.065 dB |
| 32,767 taps ×16 β 8/14/20, min-phase peak above Nyquist (`dense`) | -75.69 / -130.21 / -179.67 dB | -82.717 / -136.744 / -190.841 dB (linear -82.717 / -136.744 / -190.841) |
| stopband summary, 4M β 20 (`measure`) | reported 192.008 dB, actual peak -190.030 dB | reported 190.870028 dB, actual -190.870247 dB |
| DFT precision 4M ×2 / ×16 (`dft`) | -271.06 / -277.62 dB | -270.51 / -278.42 dB (unchanged method) |

The review's `winding_full` diagnostic no longer applies: the exact twin
keeps its stopband nulls *on* the unit circle, so a winding count on the unit
circle is undefined there. The tests count on `|z| = e^{0.25/N}` instead.

### Tolerances (asserted by `minimum_phase_*` tests)

* `| |H_min| - |H_lin| | ≤ 1e-4 dB` where `|H_lin| ≥ -1 dB`, and
  `≤ 0.01 dB` where `|H_lin| ≥ -100 dB`.
* Above the stopband edge, `| |H_min| - |H_lin| |` is at least 25 dB under
  the Kaiser specification `A = β/0.1102 + 8.7` dB.
* Above the stopband edge, the worst min-phase level from any frequency up to
  the output Nyquist exceeds the linear twin's by at most 1 dB wherever the
  latter is above `-(A + 5)` dB: 20 dB above the error bound, so the error
  moves a level by at most 0.83 dB. The fast tests (up to 32k taps) assert it
  down to -220 dB. Below that only the absolute bound holds: the f64
  rounding of the unscaled cepstral solution is about -220 dB re passband at
  4M taps (`with` mode reports it as `max ||H_min|-|H_lin||`), so a fixed
  dB-ratio bound at -230 dB, as first drafted, is not attainable there
  (4,194,303 taps, β 20, Nyquist-stop: +1.45 dB with the comparison taken
  down to -210 dB).
* Every zero inside `e^{0.25/N}`. Checked by winding count and, on short
  kernels, by Schur-Cohn and numpy rooting. The exception is f64 rounding
  noise below about -300 dB.
* Energy partial sums of the min-phase kernel dominate the linear twin's.

Measured worst cases (`logs/minimum-phase-4m-contract.log`: 4,194,304,
4,194,303 and 1,000,001 taps, β 8/14/20, Nyquist-stop and cutoff 0.95):

| tolerance | bound | worst measured |
|---|---|---|
| passband (≥ -1 dB) | 1e-4 dB | 2.9e-8 dB |
| ≥ -100 dB | 0.01 dB | 1.7e-5 dB |
| stopband error, re passband | ≤ -(A + 25) dB | -212.0 to -227.7 dB at 4M (any β), -226.5 to -239.3 dB at 1M+1; tightest: 29.1 dB under A (4,194,304 taps, β 20, cutoff 0.95) |
| stopband envelope (above -(A + 5) dB) | +1 dB | +0.44 dB (4,194,303 taps, β 20, Nyquist-stop) |

The fast tests cover 1k to 32k taps, β 2..20, cutoff 1, 0.95 and
Nyquist-stop, with the envelope asserted down to -220 dB; their worst
stopband error is 66 dB under A at β 20 (β 2: 165 dB).

Design time and peak RSS of the test process at 4,194,304 taps: linear
0.16-0.19 s; minimum phase 5.04-5.15 s and 1412 MiB, within the 15 s / 2 GiB
budget (1,000,001 taps: 1.2 s).

The minimum-phase β range is 2..20. Above 20 the tolerances fail at the
longest kernels (`with` mode, `ENVLIM=-215 BETAS=22,24`): at 4,194,304
taps, β 22 Nyquist-stop has its minimum-phase stopband peak 3.5 dB above
the linear one (-204.83 vs -208.32 dB), and at β 24 the unwrap fails and
the floored fallback is used. At 1,048,576 taps, β 24 misses the passband
bound (1.02e-4 dB). The daemon rejects minimum phase above β 20.

## 2. Stop at Nyquist

`calib` mode finds the smallest `k` for which the linear design's peak over
[source Nyquist, output Nyquist] is at most -A. It uses a dense search: an
8× oversampled FFT, then every peak within 6 dB of the maximum refined by
direct evaluation. `logs/kinf_*.log` lists it for β 0..24 in steps of 1/4
at 64k and 16k taps. It is a sawtooth between 1.00 and 1.284, with a tooth
wherever a new sidelobe becomes the binding one. Above β 18 the teeth rise
by about 0.024 per unit β, because Kaiser's `A(β)` overstates the true
sidelobe level more and more. The tooth positions shift with taps/ratio,
which makes a per-β table brittle, so the formula uses the upper envelope
`max(1.17, 1.19 + 0.03(β-20))` plus 0.02.

Short filters need more: when the transition is a sizable fraction of the
band the two band edges interact (`logs/calib_jumps.log`, `calib_small.log`).
The excess is bounded by `k_eff = k(1 + 0.7 kX)`, which gives

```
cutoff = 1 - y (1 + 0.7 y),   y = k(β) X,   X = ratio (A - 7.95) / (14.36 (N - 1))
k(β) = max(1.19, 1.21 + 0.03 (β - 20)),   A = β / 0.1102 + 8.7
```

The feasible domain is β 5..24 and cutoff ≥ 0.5. Below β 5, Kaiser's
transition estimate breaks down for short filters (+0.84 dB at β 2.75, 175
taps, ×32). Above 24 the target lies below the f64 floor.
`logs/nyquist-stop-sweep-*.log` verifies 8,262 designs per phase (β 5..24 in
1/8 steps, ratios 2, 3, 4, 8, 16, 32, 1× to 100× the minimum length, up to
262,144 taps). The worst peaks are 0.150 dB (linear) and 0.127 dB
(minimum) under -A.
`logs/nyquist-stop-full-matrix.log` (the `#[ignore]`d Rust test, measured
engine response) adds 16,385, 65,536, 1,000,001, 4,194,303 and 4,194,304
taps × ratio 2/4/8/16 × linear/minimum × β 8/14/20 × 44.1/48 kHz: all 240
cases pass, worst peak 0.582 dB under -A (tolerance +0.02 dB).

Designs outside the calibrated range are rejected by the daemon's
`validate_upsampler` (INVALID_PARAMS) at the largest ratio the target can
reach over the allowed source rates. The message and the UI tooltip say
"Stop at Nyquist is only guaranteed from N taps at ×R": below that length
the calibrated formula cannot promise the stopband at the source Nyquist,
which is not the same as the design being impossible. The UI (`app/src/lib/upsamplerLimits.ts`, a mirror checked
against shared cases) disables the option and gives the reason in its
tooltip.

## 3. Second review (`../scientific-review-2/`)

The second review found two must-fix problems in the fixes above. First, the
scaled cepstrum left exterior zeros of short kernels unreflected or fell back
silently, and the caller discarded the failure flag. Second, the stopband
summary could miss a lobe between FFT bins. `review2-rerun/` is the review's
harness, adapted to the new API, and `src/bin/fix2.rs` is the experiment tool
behind the fixes. Run from `audivane-dsp/`:

```sh
export CARGO_TARGET_DIR=/tmp/rev2-target   # keeps target/ out of docs/
B="cargo run --release --manifest-path docs/research/minimum-phase/review2-rerun/Cargo.toml --"
$B case 15 16 14 1; $B case 36 2 20 1; $B case 35 8 19.81721669866479 0.3330865066556352
$B shortscan                                  # the review's 7,560-design zero scan
$B summary 1084 3 9.28102622649481 0.988318032981387
$B summaryscan 500
python3 docs/research/scientific-review-2/roots.py docs/research/minimum-phase/review2-rerun/fixtures/n15

F="cargo run --release --manifest-path docs/research/minimum-phase/Cargo.toml --bin fix2 --"
$F psweep 2 128 full                          # root path, full grid per length
$F psweep 129 1024                            # root path, 6 designs per length
$F wsweep 1025 4096 1 0.25                    # cepstral path, winding counts on e^{0.25/N}
$F csweep 40 1024 0.25                        # the uncapped cepstrum on short lengths
$F long 6                                     # long-length matrix (NS=... to pick lengths)
$F radius 4194304 3 20 ns 2,1.5,1.75,1.25     # one design, several contour radii

# Heavy Rust tests (run one at a time)
cargo test -p audivane-dsp-core --release minimum_phase_sweep_to_4096 -- --ignored --nocapture
cargo test -p audivane-dsp-core --release minimum_phase_4m -- --ignored --nocapture
```

### Minimum phase: method

* **Up to 1024 taps: polynomial roots.** The Aberth-Ehrlich iteration
  starts from Newton-polygon guesses and runs total-step sweeps (threaded
  from degree 192). Newton steps in double-double arithmetic then polish
  each root, with roots outside the circle refined in `1/z`. Every zero
  outside the circle by more than its own accuracy is reflected exactly. The
  kernel is multiplied by all-pass factors on a 16N-point grid. Whatever lands
  past N taps measures the root error and must stay ≤ 1e-9 (measured 3e-14).
  The result is then rooted again and checked.
* **Longer kernels: the scaled cepstrum** of section 1. Its contour radius
  is now adaptive. The radii tried are 2, 1.5 and 1 (in units of 1/N); from
  524,288 taps the order is 1.5, 2, 1, 1.25. The first result that passes
  the unwrap and magnitude checks is used. The switch to roots at 1024 taps
  has margin: run uncapped on 40..1024 taps (325,000 designs), the cepstrum
  left zeros outside up to 186 taps, and once at 416.
* **No fallback.** Every result is checked (below). A design that fails is a
  `MinimumPhaseError`. It propagates through `try_design_kernel`,
  `Upsampler::try_new` and `ChainError::Upsampler`, and the daemon reports
  it as INVALID_PARAMS ("invalid chain: upsampler: the minimum-phase filter
  could not be designed to tolerance: ..."). `measure_upsampler` returns the
  same text, and the Upsampling view shows it. All of this happens on the
  control thread; the real-time path only receives a finished kernel.

### Minimum phase: tolerances (checked on every design)

* Passband and stopband magnitude: the contract of section 1, now checked on
  every returned kernel against an FFT of at least 8N points.
  * `≤ 1e-4 dB` where `|H_lin| ≥ -1 dB`.
  * `≤ 0.01 dB` where `|H_lin| ≥ -100 dB`.
  * Absolute error `≤ -(A + 25) dB` from the stopband edge up.
* Zeros, up to 1024 taps. Every zero of the returned f64 kernel outside the
  unit circle must be a pseudozero within `δ = 64 u N` (`u = 2^-53`).
  Everywhere on the radial segment to the circle,
  `|B(ζ)| ≤ δ Σ|b_k||ζ|^-k`: a relative change of every coefficient by at
  most δ, a few ulps, puts the zero on the circle. Where `|H|` is at the
  rounding floor of its own coefficients, its zeros are not determined
  more closely than that.
* Zeros, longer kernels. Checked by the unwrap residue at run time. The
  sweeps check them by winding counts on `e^{0.25/N}`.

### Minimum phase: before and after

The review's counterexamples (`review2-rerun/logs/n15.log`, `n36.log`,
`n35.log`, `roots-n*.log`):

| design | before (3de0d59) | after |
|---|---|---|
| 15 taps, ×16, β 14, cutoff 1 | `exact=true`, conjugate pair left at \|z\| = 1.034996, on-circle level -141.4 dB | roots, 4 zeros reflected, largest \|z\| = 1 + 2.7e-12, stopband error -314.3 dB; long-double `roots.py` finds none outside |
| 36 taps, ×2, β 20, cutoff 1 | silent fallback, passband 1.6385e-4 dB (bound 1e-4), stopband error -209.49 dB (bound -215.19) | largest \|z\| = 1 + 1.07e-9, passband 3.9e-15 dB, stopband error -312.2 dB |
| 35 taps, ×8, β 19.817, cutoff 0.333 | silent fallback, 0.04421 dB at ≥ -100 dB (bound 0.01), stopband error -201.64 dB (bound -213.53) | largest \|z\| = 1 + 2.46e-9, 2.1e-11 dB, stopband error -316.4 dB |
| `shortscan`, 7,560 designs, N 3..128 | 84 with zeros outside e^{0.25/N}, 21 fallbacks | 0 and 0 (`shortscan.log`) |

Sweeps of the new code:

| range | designs | result |
|---|---|---|
| N 2..128, full grid (ratios 2, 3, 4, 8, 16, 32 × 11 β in 2..20 × 5 cutoffs) | 41,910 | no failures. No zero outside the circle where `\|B\|` there is above -250 dB re DC. Largest pseudozero perturbation 6.3e-3 δ, largest radius 1 + 2.35e-4 (rounding noise) |
| N 129..1024, 6 designs per length | 5,376 | no failures. Largest perturbation 0.04 δ, largest radius 1 + 2.5e-3 (rounding noise), passband ≤ 2.7e-13 dB |
| N 1025..4096, random designs | 6,144 | no failures, no zero outside e^{0.25/N} |
| `minimum_phase_sweep_to_4096_taps`: every N 2..4096, 3 designs each | 12,285 | all pass, no untrusted winding count, 307 s on 48 threads (`logs/minimum-phase-sweep-4096.log`) |
| long matrix: 65,535 to 4,194,304 taps (11 lengths) × ratios 2, 3, 4, 8, 16, 32 × β 2, 5, 8, 11, 14, 17, 20 × cutoff 1, 0.95, 0.5, Nyquist-stop | 1,782 | with radii 1.5, 2, 1: one failure (4,194,304 taps, ×3, β 20, Stop at Nyquist), all others pass (`logs/long-matrix.log`). With the fourth radius, the 324 designs at 4,194,303 and 4,194,304 taps all pass, tightest stopband margin 0.3 dB (`logs/long-matrix-4m-four-radii.log`) |

The 4M β-20 designs sit on a rounding floor. Their stopband error is about
-206 to -229 dB against the -215.2 dB bound. It peaks on single bins at the
first stopband nulls, and which contour radius lands under the bound is
effectively random. The one design that the first three radii missed was
4,194,304 taps, ×3, Stop at Nyquist:

| R | 2 | 1.75 | 1.5 | 1.25 | 1 |
|---|---|---|---|---|---|
| stopband error | -212.7 dB | -229.2 dB | -210.9 dB | -218.8 dB | passband 1.012e-4 dB |

Hence the fourth radius, 1.25. Every design that passed before keeps the
radius it had, so its design time does not change.

Design time and peak RSS (`logs/minimum-phase-4m-contract-review2.log`:
`minimum_phase_4m_contract_time_and_memory`, release, single design at a
time; 4,194,304, 4,194,303 and 1,000,001 taps × ratio 16 and 2 × β 8/14/20 ×
Nyquist-stop/0.95/1). The test asserts at most 15 s and 2 GiB per design,
and all 54 designs pass:

| 4,194,304 taps, β 14 | linear | minimum | peak RSS |
|---|---|---|---|
| ×16, cutoff 1 | 0.18 s | 6.80 s | 1540 MiB |
| ×16, cutoff 0.95 | 0.18 s | 6.09 s | 1540 MiB |
| ×2, cutoff 1 | 0.18 s | 7.65 s | 1540 MiB |
| ×2, cutoff 0.95 | 0.18 s | 6.29 s | 1540 MiB |

Before (3de0d59) the minimum-phase design took 5.09 s and used 1.35 GiB.
The difference is the magnitude check that now runs on every design. At
1,000,001 taps it takes 1.4 to 1.5 s.

**The slow path.** Only the first contour radius ran for every design below
4,194,303 taps in the long matrix (1,458 designs), and for 316 of the 324
designs at 4,194,303 and 4,194,304 taps. The other 8, all β 20, needed more
radii. Each extra radius costs about 4 to 5 s on its own (10.3 s for two in
the 4M test). The times below are from the matrix run, which ran six designs
in parallel; one radius took 6 to 7.6 s there:

| design (β 20) | radii | time |
|---|---|---|
| 4,194,303 ×2: cutoff 1, 0.95, Stop at Nyquist; 4,194,303 ×32: cutoff 0.5; 4,194,304 ×2: cutoff 0.95 | 2 | 10.3 to 11.4 s |
| 4,194,304 ×3: cutoff 1, 0.95 | 3 | 15.4 s |
| 4,194,304 ×3: Stop at Nyquist | 4 | 28.4 s |

The four radii are accepted in that order because no other order does
better on this data: 1.5 alone covers 316, 2 covers 5 of the rest, 1 covers
2 and 1.25 the last one. Each retry already reuses the linear magnitude.
The cepstrum itself depends on the radius, so no FFT work carries over.
Making the β-20 floor itself lower would need a longer FFT, which costs
memory the 2 GiB budget does not have. Root path: 0.1 to 0.19 s at 512 taps,
0.3 to 0.6 s at 1024.

### Stopband summary: method

`stopband_summary` (`resample.rs`) gives every frequency from the source
Nyquist to the output Nyquist a bound, instead of refining sampled maxima.

1. **Per-bin Taylor bound.** For each bin interval (half a bin either side,
   `s = (M/2)·π/L` in radians):
   `|H(f)| ≤ Σ_{j<p} |H_c^(j)(f_k)| s^j/j! + s^p/p! Σ|h|`. The terms come
   from `p` FFTs of `((n - c)/(M/2))^j h[n]`, taken about the kernel centre
   `c`.
2. **Refinement.** Intervals whose bound could exceed the target, or the
   best level found by more than 0.01 dB, are refined. The refinement
   interpolates the response with Chebyshev polynomials from direct-DFT
   nodes, with interpolation error `E ≤ 2√2 Σ|h| (s/2)^m/m!`. Each of 64
   cells per bin then gets a bound on `|P|²` from its exact Taylor
   coefficients. This repeats until no interval is left.
3. **Truncation terms** stay 60 dB under the target for the start and 80 dB
   under the peak for the peak (0.0017 dB). They never go below 0.1× the
   evaluation floor `u √M ‖h‖₂`, which is -265 dB re passband at 4M taps
   and -300 dB at 1k.

**Tolerance:** the reported peak is an upper bound on the continuous
response from the reported start to the output Nyquist. It is above the
true peak by at most 0.01 + 0.002 dB, or by the evaluation floor
(absolute), whichever is larger. That holds in exact arithmetic; the f64
rounding of the evaluations comes on top.

### Stopband summary: before and after

| case (48 kHz, Fraction cutoff) | before (3de0d59) | after |
|---|---|---|
| N 1084, ×3, β 9.28102622649481, cutoff 0.988318032981387 | start 24117.19 Hz, reported -93.708 dB, true -92.748 dB: **0.960 dB optimistic** | start 24137.51 Hz, reported -92.97724 (linear) / -92.97704 (minimum) dB, true -92.98015 dB: 0.003 dB conservative |
| N 3034, ×16, β 17.728550989592016, cutoff 0.95, linear | start 24281.25 Hz, reported -169.617 dB, true -169.262 dB: 0.354 dB optimistic | start 24424.80 Hz, reported -169.61905, true -169.62256 dB: 0.0035 dB conservative |
| `summaryscan` 500, cases reported above -200 dB | worst 0.960 dB optimistic | 327 cases, all conservative, by 0.000003 to 0.0047 dB |
| `summaryscan` 500, reported -250 to -200 dB | up to 0.467 dB optimistic | 52 cases. Up to 0.031 dB under the measured value, within its absolute rounding (largest absolute excess -279.1 dB re passband, at any level) |

In the -250 to -200 dB band the measured "true" value is itself an f64
evaluation of a 1k- to 4k-tap sum. There, the absolute difference (at most
-279 dB re passband) is the meaningful number, not the dB ratio. The
review's -250 to -280 dB cases (up to 0.87 dB) were also measurement noise,
as the review notes.

The tests are `stopband_summary_finds_the_lobe_between_bins` (the review's
case) and `stopband_summary_bounds_random_designs` (18 designs against an
independent search: a 16× FFT plus golden-section refinement of every local
maximum within 20 dB, slack 0.02 dB).

Cost: `measure_upsampler` at 4,194,304 taps takes 12.3 s for linear phase
(7.1 s before) and 16.2 s for minimum phase (11.7 s before), including the
design.

### Feasibility wording

The daemon message, the UI tooltip and the taps hint now say "Stop at
Nyquist is only guaranteed from N taps at ×R". The limits are unchanged.
