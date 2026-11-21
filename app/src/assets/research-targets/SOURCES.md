# Research target sources and redistribution notices

Prepared/retrieved **2026-11-14** for Audivane.

These assets are data for the overlay implementation, not registration code.
All JSON files have exactly the contract's ten fields. `derived: false` means
an existing target/reference design, resampled and normalized as documented here;
`derived: true` means an analytical curve/envelope constructed for Audivane.
No paper figures were digitized. No empirical preference bounds are bundled.

## Downloaded numerical sources

Publisher/maintainer: **Jaakko Pasanen / AutoEq**. Repository snapshot commit
[`7ae0f56d53074872b028649617a22bbb4232feb7`](https://github.com/jaakkopasanen/AutoEq/tree/7ae0f56d53074872b028649617a22bbb4232feb7),
committer timestamp **2025-07-20T11:56:36Z**. This is the repository snapshot date,
not a claim that each target was created on that date. Each linked file was
actually downloaded from this immutable revision. All five representations use
the repository's MIT licence; the exact notice is retained below. This is not
a licence to copy the underlying AES papers or ISO/manufacturer tables.

| Bundled JSON | Exact downloaded CSV |
| --- | --- |
| `harman-oe-2018.json` | [Harman over-ear 2018.csv](https://raw.githubusercontent.com/jaakkopasanen/AutoEq/7ae0f56d53074872b028649617a22bbb4232feb7/targets/Harman%20over-ear%202018.csv) |
| `harman-ie-2019.json` | [Harman in-ear 2019.csv](https://raw.githubusercontent.com/jaakkopasanen/AutoEq/7ae0f56d53074872b028649617a22bbb4232feb7/targets/Harman%20in-ear%202019.csv) |
| `diffuse-field-gras-kemar.json` | [Diffuse field GRAS KEMAR.csv](https://raw.githubusercontent.com/jaakkopasanen/AutoEq/7ae0f56d53074872b028649617a22bbb4232feb7/targets/Diffuse%20field%20GRAS%20KEMAR.csv) |
| `diffuse-field-5128.json` | [Diffuse field 5128.csv](https://raw.githubusercontent.com/jaakkopasanen/AutoEq/7ae0f56d53074872b028649617a22bbb4232feb7/targets/Diffuse%20field%205128.csv) |
| `oratory1990-optimum-hifi.json` | [oratory1990 optimum hifi over-ear.csv](https://raw.githubusercontent.com/jaakkopasanen/AutoEq/7ae0f56d53074872b028649617a22bbb4232feb7/targets/oratory1990%20optimum%20hifi%20over-ear.csv) |

SHA-256 of the original downloaded bytes, before parsing or normalization:

```text
12797010a78a693adeaa3e91f961e88cca3d25abc7883d144369915fc7c18f7c  Harman over-ear 2018.csv
9f7acab72e1f7507889a13d4278390089d495ccedcfd3aa48df939a6c7491139  Harman in-ear 2019.csv
96b0ea7df98adb2024b1261d131360a74825f75e53fb664644a381ffb55ce426  Diffuse field GRAS KEMAR.csv
f906951f4ef7229fee579ca0bb574eec4aeea93f69e1e224ca4f94461836d361  Diffuse field 5128.csv
d0d2cd58582a2362b73d2eac20f21daf69e97da2e665e1ddf98a55f4499a2e31  oratory1990 optimum hifi over-ear.csv
```

Additional identity evidence: AutoEq commit
[`8616bed4a616c4c0a2dbeb9ff67907cf2e521b39`](https://github.com/jaakkopasanen/AutoEq/commit/8616bed4a616c4c0a2dbeb9ff67907cf2e521b39),
2023-10-29T06:27:16Z, renamed `targets/harman_in-ear_2019v2.csv` to
`targets/Harman in-ear 2019.csv`. Thus the included IE file is the v2 lineage;
the missing suffix is a filename change, not a newly validated target.

## Exact numerical transformations

Input columns are `frequency,raw`. Each selected CSV contains 695 rows spanning
20.00–19955.54 Hz. Output is Hz and dB; raw values are interpolated **linearly in
dB against log frequency**, then the interpolated value at 1000 Hz is subtracted.
No amplitude-domain averaging, smoothing, added bass shelf, tilt, or coupler
conversion is applied. The two Harman files already contain their bass response.

All ten outputs share this 242-point grid:

```python
grid = sorted({20.0, 1000.0, 20000.0} |
              {round(20 * 2 ** (k / 24), 6) for k in range(240)})
```

For bracketing input points `(f0,y0)` and `(f1,y1)`:

```text
t = ln(f/f0) / ln(f1/f0)
raw(f) = y0 + t*(y1-y0)
output(f) = raw(f) - raw(1000)
```

For the final output frequency only, 20000 Hz exceeds the source endpoint by
44.46 Hz (0.00321 octave): `raw(20000) = raw(19955.54)`. This explicit endpoint
hold is not a measured extension and does not infer a new treble slope. There
is no other extrapolation. Frequencies are rounded to six decimal places and
final dB values to four decimal places using Python's `round`.

The subtracted reference values (dB, before final rounding) were:

| Source | Interpolated value at 1 kHz |
| --- | ---: |
| Harman OE 2018 | +0.0030991810942620547 |
| Harman IE 2019 | −0.003801637811475891 |
| DF GRAS KEMAR | −0.002252047264344864 |
| DF 5128 | −0.0007024567172138358 |
| oratory1990 Optimum HiFi | +0.0030991810942620547 |

To reproduce the imported point arrays after downloading a pinned CSV:

```python
import bisect, csv, math

with open("downloaded.csv", newline="") as stream:
    rows = [(float(r["frequency"]), float(r["raw"]))
            for r in csv.DictReader(stream)]
xs, ys = zip(*rows)

def sample(f):
    if f <= xs[0]: return ys[0]
    if f >= xs[-1]: return ys[-1]
    hi = bisect.bisect_right(xs, f)
    lo = hi - 1
    t = math.log(f / xs[lo]) / math.log(xs[hi] / xs[lo])
    return ys[lo] + t * (ys[hi] - ys[lo])

grid = sorted({20.0, 1000.0, 20000.0} |
              {round(20 * 2 ** (k / 24), 6) for k in range(240)})
reference = sample(1000)
points = [[f, round(sample(f) - reference, 4)] for f in grid]
```

## Original analytical data (MIT)

Constructed 2026-11-14 for this project; no third-party curve data are inputs.
For `x = log2(f/1000)`, with `f` in Hz:

| JSON | Formula |
| --- | --- |
| `speaker-tilt-flat.json` | `T = 0*x` |
| `speaker-tilt-minus-0-5.json` | `T = -0.5*x` |
| `speaker-tilt-minus-0-7.json` | `T = -0.7*x` |
| `speaker-tilt-minus-1-0.json` | `T = -1.0*x` |
| `speaker-tilt-exploration.json` | `lower = min(-1.0*x, -0.5*x)`; `upper = max(-1.0*x, -0.5*x)` |

Evaluate these on the same grid above and round the final dB values to four
decimals. The minimum/maximum is necessary because the edge order swaps at
1 kHz. All formula files have `derived: true`.

The slope choices and exploration endpoints are **engineering choices**, not
values digitized from Toole, B&K or Harman, and not a confidence band or an
empirical distribution of preferred slopes. The citation to Toole in the tilt
metadata supports room-response interpretation only. The exploration band is
recommended off by default and must not be displayed as published preference
bounds. The report explains why.

## Inspected data not redistributed here

- **Ravizza et al., Zenodo v1.0**, 2023-09-29,
  [DOI 10.5281/zenodo.8388242](https://doi.org/10.5281/zenodo.8388242).
  [Record metadata](https://zenodo.org/api/records/8388242) explicitly lists
  `license.id = cc-by-4.0`. Both
  [MagnitudeFrequencyResponses.csv](https://zenodo.org/api/records/8388242/files/MagnitudeFrequencyResponses.csv/content)
  and [PreferenceRatings.csv](https://zenodo.org/api/records/8388242/files/PreferenceRatings.csv/content)
  were downloaded 2026-11-14. SHA-256:
  `58c5f89f3af7cb2aecb36fe29dcdf91f9be340e92bec2c28ae6bc8c8e83e0eaa`
  and `dc7b7e9abe1fce44bd83dcd789a600ffff2938f7e4da20043b04183d8469c7a9`,
  respectively. This is eligible **CC BY 4.0** raw preference/GEQ data, not
  a ready-made 20 Hz–20 kHz target or bounds file. No derived population
  statistics are asserted from it. Retain its licence if used later; do not
  relicense it MIT.
- **Banzai/Haruto graph-tool fork**, commit
  [`edb098a4f06bb14fda4d62de0a216895c6d1d5bd`](https://github.com/kanzai1122/banzai1122.squig.link/tree/edb098a4f06bb14fda4d62de0a216895c6d1d5bd),
  2026-04-04T10:18:17Z. `assets/pref_bounds/Bounds L.txt`, `Bounds R.txt`,
  `assets/js/config.js`, README and licence were inspected/downloaded
  2026-11-14. Its software MIT notice does not establish an original data grant,
  statistical derivation or fixture identity for every third-party band/target.
  No bounds or community target files from this fork are copied here.
- ISO Part 2 tables, B&K 1974 Figure 5, Harman listener figures, Super 22,
  Crinacle/IEF targets and Headphones.com/GraphWorks bands are linked in the
  report. No redistribution permission for those original data was established
  by this audit. Their papers, page images and graph assets are not included.

## AutoEq MIT notice (verbatim)

Source:
[LICENSE at the pinned AutoEq revision](https://raw.githubusercontent.com/jaakkopasanen/AutoEq/7ae0f56d53074872b028649617a22bbb4232feb7/LICENSE),
downloaded 2026-11-14, SHA-256
`a6e96e96a51b5bd7df0b926c325f7cef267064bc4b3c013a61a7565521a6a77a`.

```text
MIT License

Copyright (c) 2018-2022 Jaakko Pasanen

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

The original Audivane analytical data are also released under the MIT terms
above, with this notice: **Copyright (c) 2026 Audivane contributors**. Keep
`SOURCES.md` with any redistributed assets so the notices, attribution and
transformation disclosures remain available. This data licence does not change
the application's source-code licence.

## Offline validation

From the repository root:

```sh
node app/scripts/check-research-targets.mjs
```

The checker also accepts a directory of compatible JSON fixtures as its first
argument. It has no dependencies outside Node's standard library.
