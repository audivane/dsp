# Reference checks against numpy/scipy

`run.py` validates the Rust DSP against independent Python implementations:
the real `avdsp` binary processes generated WAV files, and the results are
compared sample for sample with numpy/scipy. Nothing here re-implements the
Rust code: the Python side uses textbook RBJ coefficients, `scipy.signal`
and the kernel dumped by `avdsp kernel`.

## Requirements

- Python 3 with `numpy` and `scipy`
- A built `avdsp` (the script does not build anything)

```sh
# from the repository root
cargo build -p audivane-dsp-cli
python3 tests/reference/run.py
```

The default binary is `target/debug/avdsp` relative to the workspace root. Use
`--avdsp PATH` or the `AVDSP` environment variable to test another build, for
example an optimized one:

```sh
cargo build --profile fastrel -p audivane-dsp-cli
python3 tests/reference/run.py --avdsp target/fastrel/avdsp
```

## What is checked

| Check | Comparison | Tolerance |
| --- | --- | --- |
| `eq_response` | `avdsp eq response` vs `scipy.signal.sosfreqz` of independently computed RBJ coefficients, 10-band profile at 44.1 kHz and 192 kHz | 1e-6 dB (and 1e-6 rad phase) |
| `convolution` | `avdsp process --conv` with `--trim-latency`, `--dither none`, float output vs `scipy.signal.fftconvolve` (mono and stereo, odd block size 333) | 1e-6 |
| `upsampling` | `avdsp process --upsample 4` vs `scipy.signal.upfirdn` using the kernel written by `avdsp kernel` (which has DC gain `ratio`, for zero-stuffing) | 1e-6 |
| `bypass` | 16/24-bit random PCM through `avdsp process` with no stages and a matching `--format` | byte-identical `data` chunk |
| `levelmatch` | analytic A-weighted offset of a `+6 dB` profile against a flat profile | −6.0 dB ± 0.05 |

Notes:

- The EQ check asks `avdsp` for a log-spaced frequency grid and evaluates the
  Python coefficients at exactly those frequencies, so no interpolation is
  involved.
- The upsampling check reads the chain latency from the `process` output
  (`latency N out frames`), because `--trim-latency` removes exactly that
  delay. It allows a ±1 sample alignment offset in case an implementation
  rounds the linear-phase group delay, and reports the offset it used.
- The bypass check compares the raw PCM payload, not the WAV headers, so
  metadata differences do not matter.
- `--only NAME` runs a single check (substring match); `--keep` leaves the
  generated WAVs in a temporary directory for inspection.

## Exit status

Each check prints `PASS`/`FAIL` with its measured error, followed by
`N/M checks passed`. The exit status is non-zero if any check failed, so the
script works as a CI step.
