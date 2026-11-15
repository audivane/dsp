# Built-in overlay data

## Targets

Source: [AutoEq](https://github.com/jaakkopasanen/AutoEq) by Jaakko Pasanen,
MIT licence (see below). Files taken from the `targets/` directory at commit
[`7ae0f56d53074872b028649617a22bbb4232feb7`](https://github.com/jaakkopasanen/AutoEq/commit/7ae0f56d53074872b028649617a22bbb4232feb7)
(2025-07-20), blob `sha` as noted, reduced to about 1/24 octave (241 points,
20 Hz – 20 kHz) by `scripts/gen-targets.mjs`. Values are interpolated linearly
in log frequency; nothing else changed.

| File here | AutoEq file | blob sha |
| --- | --- | --- |
| `harman-over-ear-2018.json` | [`targets/Harman over-ear 2018.csv`](https://github.com/jaakkopasanen/AutoEq/blob/master/targets/Harman%20over-ear%202018.csv) | `c8b1e7ef08fa1f6a3f0c9836c6e9227392a0848a` |
| `harman-in-ear-2019.json` | [`targets/Harman in-ear 2019.csv`](https://github.com/jaakkopasanen/AutoEq/blob/master/targets/Harman%20in-ear%202019.csv) | `f96afd93500fcf13466f53c3e9d56307dc5467b9` |
| `diffuse-field-iso-11904-1.json` | [`targets/Diffuse field ISO 11904-1.csv`](https://github.com/jaakkopasanen/AutoEq/blob/master/targets/Diffuse%20field%20ISO%2011904-1.csv) | `b3d1e3bed337ed933ba3320388dae9c18db5ae73` |

Note on the diffuse-field target: AutoEq publishes the **ISO 11904-1**
diffuse-field curve; there is no `ISO 11904-2` file in the repository, so that
is the one shipped (named for what it is, not what the plan guessed).

Regenerate with:

```sh
npm run gen:targets   # needs network access
```

Generated in code, no data file: the **Tilt** target (a straight line of
dB/octave through 0 dB at 1 kHz, in `lib/overlays.ts::tiltTarget`) and the
**Flat** target.

## Preference bounds

None are shipped. The published preference-bound research (Olive, Welti and
McMullin, "Listener Preferences for In-Room Loudspeaker and Headphone Target
Responses", AES 2013) exists as papers and figures, not as a
machine-readable data set with a licence that allows redistribution here. So
the app offers imported bounds only: one file with `frequency, lower, upper`
columns, or two single-curve files. See "EQ graph overlays" in `app/README.md`.

## Licence (AutoEq)

```
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
