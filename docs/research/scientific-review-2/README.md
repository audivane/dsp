# Scientific review of 3de0d59

Reviewed `3de0d596a8111428d3212b26823237c4c23b8521`, as merged at
`3ef7e12`. The working tree was clean at the start. Production code was not
changed. The earlier review, the fix's diff, and the minimum-phase research
README, scripts, calibration logs and reruns were inspected first.

**Verdict: two must-fix findings.** The long-filter magnitude improvement is
real, but the minimum-phase contract fails for accepted short kernels, and
the response summary can still miss an intervening sidelobe. No Nyquist-stop
attenuation violation was found in the supported engine ratio range 2..32.
That last result is experimental evidence, not a proof over continuous beta
and every accepted length.

## 1. Must-fix: the contour does not separate all exterior zeros

Locations: `crates/dsp-core/src/minphase.rs:57`, `:180`, `:221` and `:232`;
the caller discards the failure flag at `crates/dsp-core/src/resample.rs:249`.

The cepstral reflection identities are sound **if** no exterior zero lies
between the unit circle and the contour. This implementation assumes that
only negative real zeros can occupy that annulus and pre-reflects only
those. That assumption is false for accepted short Kaiser-sinc kernels.
Conjugate reciprocal quadruples can also lie in the annulus.

A simple production counterexample is **15 taps, ratio 16, beta 14, cutoff
1, minimum phase**. The daemon accepts all these values. The contour is
`exp(0.05) = 1.051271096`, and production returns `exact=true`. Yet the final
FIR has this conjugate exterior pair:

```
z = -0.7293093127325986 +/- 0.7343879188946861 i
|z| = 1.0349964681374344
N log|z| = 0.515970214259
```

This exceeds even the relaxed radius `exp(0.25/N) = 1.016806330386261`.
The corresponding pair in the linear twin has radius 1.034996593762407;
the conversion leaves it outside instead of reflecting it. The response
at the root angle on the unit circle is **-141.44 dB**, nowhere near the
claimed -300 dB coefficient-noise exception for short kernels. Long-double
Horner/Newton refinement of the actual stored f64 coefficients gives
normalized residual **1.77e-20**. This is not a winding-grid artifact or
root solver noise. See `logs/roots-n15.log`, `logs/n15-simple.log`, and
`fixtures/n15/`.

A systematic short-kernel check (N=3..128, ratios 2/4/8/16/32, beta
2/8/14/18/19/20, cutoff 0.95/1) finds **84 of 7,560 designs** with exterior
zeros beyond `exp(0.25/N)`, using a 256N-or-denser contour grid and accepting
counts only with adjacent phase steps below 1.5 rad. There are also **21
fallbacks**. The affected cases extend through N=48; the alpha cap alone is
not sufficient. See `logs/shortscan.log`. The independent polynomial search
in `geometry.py` also finds complex zeros in the annulus.

Other accepted geometries put a zero too near the contour, fail the unwrap
check and silently use the floored fallback. That also breaks the magnitude
contract:

| Accepted design | Production result | Measured violation |
|---|---|---|
| N=36, ratio 2, beta 20, cutoff 1 | `exact=false` | passband difference **1.6385e-4 dB**, bound 1e-4; stopband absolute magnitude error **-209.49 dB**, bound -215.19 |
| N=35, ratio 8, beta 19.81721669866479, cutoff 0.3330865066556352 | `exact=false` | difference where linear >= -100 dB **0.04421 dB**, bound 0.01; stopband error **-201.64 dB**, bound -213.53 |

The edge errors are checked with direct DFT and a long-double witness, not
just FFT bins (`logs/fallback36.log`, `logs/n35.log`). `design_kernel` returns
only `.kernel`, so neither the engine nor the daemon knows that its promised
method failed. Restricting beta to 2..20 does not prevent these failures.

**Required correction:** cover the actual short-kernel zero geometry (for
example, validated polynomial reflection for short kernels), or reject
unsupported geometries explicitly. Propagate or eliminate fallback failure
instead of reporting a successful minimum-phase twin. Test the final zeros
as well as the magnitude. Increasing padding alone does not reflect a zero
that the chosen contour classifies as interior.

The magnitudes in the first counterexample happen to match closely. That is
expected: leaving an exterior zero in place preserves magnitude too. It is
why magnitude matching and front-loaded energy cannot certify minimum phase.

## 2. Must-fix: an unsampled sidelobe never enters peak refinement

Locations: `crates/dsp-core/src/resample.rs:1088` through `:1107`, called by
the response summary at `:968`. Only sampled local maxima become search
intervals; a genuine lobe can sit between bins without generating one.

Production `measure_upsampler` at source rate 48 kHz, **N=1084, ratio 3,
beta 9.28102622649481, Fraction cutoff 0.988318032981387**, reports:

| Quantity | Linear | Minimum |
|---|---:|---:|
| stopband start | 24117.1875 Hz | 24117.1875 Hz |
| reported worst level | -93.708351541 dB | -93.708351541 dB |
| measured response at the missing peak | **-92.748318009 dB** | **-92.748318007 dB** |
| optimism | **0.960033532 dB** | **0.960033534 dB** |

The true peak is around **24134.26986 Hz**, above the reported start. It also
exceeds the nominal Kaiser target -92.919838716 dB, so this affects both the
reported start and attenuation. This is Fraction mode, not a counterexample
to the Nyquist-stop cutoff formula.

The production 4096-point FFT samples are:

| bin | Hz | dB |
|---:|---:|---:|
| 686 | 24117.1875 | -109.94653 |
| 687 | 24152.34375 | -100.13844 |
| 688 | 24187.5 | -94.22692 |

Bins 686 and 687 are not local maxima. The first candidate is bin 688;
its interval starts at 24152.34375 Hz, after the missing peak. More nested
refinement of existing candidates cannot recover it. An independent
long-double symmetric cosine sum confirms -92.748318008422 dB. An FFT of
16384 or 65536 points does generate a covering interval for this case.
See `logs/summary-witness.log`, `logs/hidden-peak.log`, and `hidden_peak.py`.

The independent 500-case summary scan finds other optimistic reports,
including **0.354 dB** at N=3034, ratio 16, beta 17.728550989592016,
cutoff 0.95, linear phase. Deep results below about -250 dB in that scan
also contain measurement roundoff and are not needed for this finding.

**Required correction:** detect intervening lobes before selecting
refinement candidates, or use a sufficiently dense/adaptive search with a
bound that justifies skipped intervals. The claimed envelope cannot follow
from refining only local maxima of the current coarse FFT.

## Claim-by-claim assessment

### Same-length minimum phase / magnitude

The returned length is preserved. The scaled cepstrum is a sound way to
approximate zero reflection when its annulus assumption holds. It removes
the old large-kernel floor problem and the padding discontinuity. It is
**not an exact or universally minimum-phase implementation on the accepted
domain**, as finding 1 demonstrates. The `exact` flag means the unwrap
checks passed; it does not establish all exterior zeros were reflected.

Normal long filters improve substantially. Before numbers below are from
the first review, not a fresh build of the parent; after numbers are from
this review unless otherwise identified.

| Check | Earlier floor implementation | Current implementation |
|---|---:|---:|
| N=4,194,304, ratio 16, beta 20, cutoff 0.95: near-edge passband mismatch | 1.744e-3 dB | **2.8661e-8 dB**, direct long-double witness |
| Same case: minimum-phase peak above source Nyquist | -210.184 dB | **about -245.33 dB**, linear about -248.81, on the 16N FFT grid (deep-floor estimate) |
| Same case: absolute magnitude difference above estimated stopband edge | old floor changes the target | **-219.28764 dB**, required <= -215.18820 |
| Same case: suffix-envelope excess above -(A+5) | no prior contractual bound | **+0.37279 dB**, required <= +1 |
| N=4,194,303, ratio 32, beta 20, cutoff 0.95: passband mismatch | not rerun | **1.3710e-8 dB**, direct witness; stopband error **-237.07458 dB** |

The largest representative check uses **16N = 67,108,864 measurement FFT
points**, versus the author's 4N contract grid, plus 2049 direct points
spanning 12/N around the cutoff and long-double witnesses. The tightened
grid does not reveal a large-kernel tolerance violation. The baseline
random minimum-phase magnitude search also passes 4000 cases, but its
random draw misses the short geometries found by the systematic zero scan.

### Tolerances and their relaxation

The stated absolute and relative errors are sensible engineering
tolerances; they are not literally exact magnitude equality. Relaxing the
fixed +1 dB comparison at -220/-230 dB is **justified for this f64 algorithm
and resource budget**. The independently measured -219.29 dB absolute
error scale at 4M is already larger than a -230 dB signal. No relative
bound can remain useful arbitrarily close to the linear twin's nulls.
This is not a proof that every possible f64 algorithm must have that floor.

The A+25 absolute bound logically implies <=0.828 dB positive level error
where the linear level is >= -(A+5): the error is then at most 0.1 of the
signal and `20 log10(1.1) = 0.82785`. This part of the relaxation is honest.
The tests compare suffix maxima, explicitly documented as envelopes, not
pointwise ratios at nulls. Short accepted fallback cases still violate the
contract, independently of this reasonable relaxation.

The 4N/16N twin grids and 8N peak grids are numerical convergence evidence,
not rigorous bounds over every frequency. The summary counterexample
shows why grid-local-max selection needs care. At 16N, the large-kernel
results agree with the author's results within their tolerances; that does
not certify all intermediate parameter values or all zero locations.

### Nyquist-stop formula

**No attenuation counterexample found for engine ratios 2..32.** Independent
checks include continuous beta values between the author's 1/8 steps,
odd lengths, lengths immediately above the formula's feasibility limit,
beta near 5 and 24, all integer ratios through 32, and a 4M odd-length
linear case at beta 23.88840542485691.

The first sweep screens 12,000 linear kernels on >=16N FFT grids, including
an extra ratio-64 probe. A second 12,000-case sweep covers every integer
ratio 2..32. The 30 closest cases in each are remeasured with >=32N FFTs,
golden-section direct refinement and independent long-double evaluation
at the winning frequency. The closest supported case in the first sweep
is **-0.10805 dB** relative to -A; in the all-ratios sweep, the closest
refined result is **-0.18231 dB**. A further **3000 minimum-phase** cases
cover ratios 2..32 and beta 5..20, with the same screening/refinement
procedure; their closest refined margin is **-0.15314 dB**. These are screening experiments, not
refined global maxima for every single screened kernel.

A more targeted **66,033-case** beta/length search at ratio 32, beta
23.40..23.64 in 0.00012 steps, checks N_min through N_min+32. Its endpoint
levels remain below -A (worst about -0.10645 dB). This targeted search is
of the source Nyquist endpoint; it does not claim to search all other
frequencies for all 66,033 cases. The independent broader peak searches
and targeted individual cases cover interior sidelobes.

Additional refined cases:

| Design | Peak level | Relative to -A |
|---|---:|---:|
| N=315, ratio 32, beta 5.0001, both phases, production Nyquist-stop cutoff | -55.563584 dB | -1.490626 dB |
| N=1653, ratio 32, beta 24, linear | -227.856584 dB | -1.370740 dB |
| N=4,194,303, ratio 2, beta 23.88840542485691, linear | -225.956113 dB | -0.482925 dB |

Source rate only scales the frequency coordinate for a fixed ratio, so
44.1 and 48 kHz need not be separate mathematical kernel cases. For a
MaxRate target, changing source rate changes the resolved ratio; this is
why checking every reachable ratio matters. The largest ratio alone is
the most conservative *formula-feasibility* check, but that does not prove
the oscillatory actual attenuation is monotone in ratio.

The formula remains an empirical upper-envelope calibration. The sweeps
do not establish a theorem for every N and continuous beta, and production
does not verify a peak bound when accepting a design. I would describe it
as calibrated with the stated tolerance, rather than universally certified.
If a literal per-design guarantee is required, add a validated response
check or a conservative analytic bound. I am not classifying an unobserved
attenuation breach as a must-fix finding.

### Should-fix: feasibility means the calibration's limit, not physical impossibility

`crates/dsp-core/src/resample.rs:69` and the matching UI message say that a
shorter filter cannot reach the stopband. The check actually rejects when
the conservative empirical cutoff falls below 0.5. For **1253 taps, ratio
32, beta 20, cutoff 0.5**, both phases meet -A: linear peak **-190.583278 dB**,
minimum **-190.583284 dB**, versus target **-190.188203 dB**. Nevertheless
the formula requires at least 1254 taps. See `logs/rejection-boundary.log`.

The guard is internally consistent with its advertised beta/cutoff policy
and is a reasonable conservative restriction. Change the explanation to
"outside the supported/calibrated design range"; it is not an exact
feasibility theorem. The beta-20 minimum-phase upper limit is also a
practical accuracy restriction, not a mathematical impossibility limit.

There is an inherited config-domain mismatch: daemon validation accepts
ratios up to 64 (`crates/daemon/src/control/config.rs:683`), while the engine
asserts 1..32 (`crates/dsp-core/src/resample.rs:305`). A ratio-64 measurement
probe panicked during initial harness development. Both lines predate this
fix; it is not charged as a new regression. Align those limits before
claiming coverage of every config-accepted ratio. Ratio-64 design probes
in `logs/nscan.log` and `logs/edge64.log` are outside the usable engine
domain and do not establish a supported playback case.

### FFT sizing / cost

The old 4N-to-2N discontinuity is fixed. `fft_len_for` uses at least 8N and,
for very short kernels, an additional lower bound from the capped alpha.
The general shorthand "exactly 8N for every N" is inaccurate: lengths
round up to a power of two and short lengths use at least 512 points.
This is a harmless shorthand here; the implementation has the intended
lower bound.

The documented 4M FFT is correctly **33,554,432 points**. Independent
design-only measurement gives **5.128 s**, peak **1,411,680 KiB = 1.346 GiB**
(`logs/design-cost.log`). The denser *verification* uses more memory:
about 3.57 GiB including the 16N measurement spectra. Do not mistake that
for production design RSS. The cost claim is credible on this machine.

## Reproduction

Run from `audivane-dsp/`, with Rust/C toolchains and Python numpy/scipy.
All targets stay outside this evidence folder:

```sh
export CARGO_TARGET_DIR=/tmp/audivane-scientific-review-2-target
cargo build --release --manifest-path docs/research/scientific-review-2/Cargo.toml
R="$CARGO_TARGET_DIR/release/scientific-review-2"

# Minimum-phase counterexamples and root proof
$R shortscan
$R case 15 16 14 1
$R case 36 2 20 1
PAD=128 $R case 35 8 19.81721669866479 0.3330865066556352
OPENBLAS_NUM_THREADS=1 python3 docs/research/scientific-review-2/roots.py
OPENBLAS_NUM_THREADS=1 python3 docs/research/scientific-review-2/geometry.py

# Reported-versus-actual attenuation, plus an independent explanation
$R summary 1084 3 9.28102622649481 0.988318032981387
OPENBLAS_NUM_THREADS=1 python3 docs/research/scientific-review-2/hidden_peak.py
$R summaryscan 500

# Nyquist-stop adversarial probes (signed margin: negative passes)
$R nscan 12000
ALL_RATIOS=1 $R nscan 12000
ALL_RATIOS=1 MIN=1 $R nscan 3000
$R edge 2000 32 23.52 0.12
$R edge 2000 64 22.23 0.12  # extra design probe, not usable engine ratio
PEAK=1 PAD=64 $R case 315 32 5.0001
$R nycase 1653 32 24
PEAK=1 PAD=64 $R case 1253 32 20 0.5
$R nycase 4194303 2 23.88840542485691

# Large-kernel accuracy and production design cost
PAD=16 /usr/bin/time -v $R case 4194304 16 20 0.95
PAD=4 $R case 4194303 32 20 0.95
/usr/bin/time -v $R time 4194304 16 20 0.95

cargo test --release -p audivane-dsp-core --lib
```

`case` accepts N, ratio, beta and optional cutoff; omitting cutoff selects
the production Nyquist-stop cutoff. `PAD` sets its measurement FFT density.
`PEAK=1` adds a >=32N full stopband search; keep it unset for the largest
twin checks unless that additional cost is desired. `DUMP_DIR` writes exact
17-digit coefficient text; for example, regenerate `fixtures/n15/` with
`DUMP_DIR=docs/research/scientific-review-2/fixtures/n15 $R case 15 16 14 1`.
The supplied summary fixture is the production linear kernel for the
1084-tap counterexample; `roots.py` and `hidden_peak.py` can run without
rebuilding Rust.

`reference.c` and `build.rs` reuse the first review's long-double reference.
The new Rust harness calls the current production designer and public
measured-response API, but uses independent search grids, golden-section
refinement and long-double final frequency evaluations. `roots.py` uses
numpy rooting only for initial guesses, then long-double complex Newton
and Horner evaluation of the stored f64 polynomial. Linux x86_64 numpy's
long double has a 64-bit mantissa here.

`logs/4m-beta24.log` is an intentionally out-of-contract minimum-phase
negative control, excluded from the findings; beta 24 is allowed only for
linear phase. Original stochastic and deterministic seeds are in
`src/main.rs` / `geometry.py`. Raw logs are retained even when a later
experiment found a simpler counterexample. The extra `above-Nyquist grid
peaks` print was added to the harness after some logs were recorded; it
does not change the calculations in those logs.

Core release suite: **118 passed, 0 failed, 6 ignored**, including the new
contract tests and peak-summary tests (`logs/core-tests.log`). The tests
passing does not cover the counterexamples above. No full 240-case matrix
or original 8262-case sweep was rerun: the independent domain probes,
short-kernel enumeration and selected 4M cases were used instead. No
observed defect establishes an audible problem for the normal long-filter
presets; the demonstrated failures are false phase/accuracy guarantees and
misreported attenuation.
