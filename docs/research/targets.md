# Target curves and preference bounds for Audivane

Checked 2026-11-14 for the EQ graph overlays (see "EQ graph overlays" in
[app/README.md](../../app/README.md)). This is a data and source audit; it
does not register overlays or change the application.

## Delivery and recommended built-ins

Bundle the **AutoEq numerical representations** of Harman OE 2018, Harman IE
2019 v2, GRAS KEMAR diffuse field, B&K 5128 diffuse field, and oratory1990
Optimum HiFi, with AutoEq's MIT notice. The last three are optional reference
choices, not interchangeable preference standards. Also supplied are original
analytical speaker tilts of 0, −0.5, −0.7 and −1.0 dB/octave, and an optional
**tilt exploration** envelope between −1.0 and −0.5 dB/octave, under MIT.

Recommended initial selections:

| Context | Initial selection | Bounds default |
| --- | --- | --- |
| Reference near-field setup (Kali IN-5) | Keep **−0.7 dB/octave**, anchored at 1 kHz | **Off**; optional slope exploration band |
| New near-field speaker profile | −0.5 dB/octave as a starting point; retain 0 as a comparison | Off |
| Compatible GRAS/711-family over-ear measurements | Harman OE 2018, with the fixture named | Import only for empirical preference bounds |
| Compatible IEC 60318-4/711 IEM measurements | Harman IE 2019 v2, with bass included | Import only for empirical preference bounds |
| B&K 5128 measurements | Fixture-matched reference/import; DF is a baseline | Import only; do not transfer 711 targets unchanged |

The speaker defaults are **engineering starting points**, including the project's
existing choice, not numerical optima proved by a listening study. There is real
research on preference variation, and an openly licensed raw preference dataset,
but this audit did not find a universal, ready-to-bundle, peer-reviewed
frequency-by-frequency lower/upper preference dataset. Do not name the generated
speaker envelope “Harman bounds,” “95% confidence,” or “acceptable response.”

## Coordinate system comes before target selection

The graph must distinguish electrical EQ gain, raw acoustic response, and
compensated response. A headphone target's ear-gain hump is an **acoustic**
reference, not an EQ filter to apply to every headphone. Compare it with
`measurement + current EQ`, using a compatible fixture and compensation. For a
measurement already compensated by target C, display `target − C` as well.
Normalizing everything at 1 kHz removes an overall level offset; it does not
remove differences between ear simulators, pinnae, insertion depths or people.

Likewise, a speaker room response measured by a microphone is different from a
headphone response at an artificial eardrum. In particular, AutoEq's
`Harman loudspeaker in-room flat 2013.csv` is in its headphone target collection;
it must not be selected as a room-microphone speaker target merely from its name.
Above roughly 8–10 kHz, headphone measurements deserve particular care because
placement and ear geometry change resonances. Keep source/fixture information
visible when importing squig/REW data. The research below supports comparison
tools and personalization, not automatic pass/fail judgements.

## Headphone and IEM targets: provenance and licensing

All imported JSON URLs point to immutable AutoEq commit
`7ae0f56d53074872b028649617a22bbb4232feb7` (2025-07-20). Exact raw-file hashes,
sampling rules and the full MIT notice are in
[SOURCES.md](../../app/src/assets/research-targets/SOURCES.md).

| Target | Canonical research or authoritative representation | Machine-readable data and licence | Bundle decision |
| --- | --- | --- | --- |
| **Harman over-ear 2018** | Olive, Welti & Khonsaripour, 2018, AES 144, paper **9919**, *A Statistical Model that Predicts Listeners’ Preference Ratings of Around-Ear and On-Ear Headphones*. [AES record](https://aes.org/publications/elibrary-page/?id=19436). This is the study introducing/testing the new AE/OE target, not just the later 2018 headphone survey. | [AutoEq CSV](https://github.com/jaakkopasanen/AutoEq/blob/7ae0f56d53074872b028649617a22bbb4232feb7/targets/Harman%20over-ear%202018.csv), **MIT representation**. AES manuscript/figures are copyright material, not MIT. | Included as `harman-oe-2018`. Full bass version, not a `without bass` file. |
| **Harman in-ear 2019 v2** | Authoritative downloadable representation: Pasanen/AutoEq. Research foundations: Olive, Welti & Khonsaripour, 2016, AES Headphone Technology, paper **6-1**, *The Preferred Low Frequency Response of In-Ear Headphones* ([AES](https://aes.org/publications/elibrary-page/?id=18369)); their 2017 AES 143 papers **9840** and **9878**, *A Statistical Model…In-Ear Headphones*, [Part 1](https://aes.org/publications/elibrary-page/?id=19237), [Part 2](https://aes.org/publications/elibrary-page/?id=19275). | [AutoEq CSV](https://github.com/jaakkopasanen/AutoEq/blob/7ae0f56d53074872b028649617a22bbb4232feb7/targets/Harman%20in-ear%202019.csv), **MIT representation**. [Rename commit](https://github.com/jaakkopasanen/AutoEq/commit/8616bed4a616c4c0a2dbeb9ff67907cf2e521b39), 2023-10-29, explicitly renames `targets/harman_in-ear_2019v2.csv` to this filename. | Included as `harman-ie-2019`, displayed as **2019 v2**. Do not invent a “2019 v2 paper”: this audit verified the file identity, not a separate paper establishing every revision. |
| **Diffuse field / ISO 11904-2** | ISO, **11904-2:2021**, *Acoustics — Determination of sound immission from sound sources placed close to the ear — Part 2: Technique using a manikin*. [ISO](https://www.iso.org/standard/81332.html). Replaces the 2004 edition. It is an exposure-measurement/conversion method, not a listener-preference target. | ISO's standard is copyrighted; no open redistribution licence found for its tables. AutoEq instead supplies **MIT representations** named [GRAS KEMAR](https://github.com/jaakkopasanen/AutoEq/blob/7ae0f56d53074872b028649617a22bbb4232feb7/targets/Diffuse%20field%20GRAS%20KEMAR.csv), [5128](https://github.com/jaakkopasanen/AutoEq/blob/7ae0f56d53074872b028649617a22bbb4232feb7/targets/Diffuse%20field%205128.csv), and [ISO **11904-1**](https://github.com/jaakkopasanen/AutoEq/blob/7ae0f56d53074872b028649617a22bbb4232feb7/targets/Diffuse%20field%20ISO%2011904-1.csv). The last is not Part 2. | **Link only** for ISO Part 2 tables. Included separately: `diffuse-field-gras-kemar` and `diffuse-field-5128`, explicitly named AutoEq references; neither is labelled “ISO 11904-2.” |
| **Free field** | A direction-specific free-field-to-eardrum transfer reference, dependent on the head/fixture and incidence angle. ISO 11904-2 discusses conversion to free-field-related levels; this is not one universal headphone curve. | Community graph files exist, e.g. [Free Field Target.txt](https://github.com/kanzai1122/banzai1122.squig.link/blob/edb098a4f06bb14fda4d62de0a216895c6d1d5bd/data/targets/Free%20Field%20Target.txt). Original measurement/standard edition, angle and rights chain were not established here. | Link/import only until the specific reference and data rights are identified. A zero-dB electrical/speaker reference is **not** an eardrum free-field target. |

The 2018 OE study rated 31 headphone models with 130 trained/untrained listeners;
the new target was preferred on average. The 2017 IE experiments rated 30 models
with 71 listeners. These are reasons to offer the corresponding targets as useful
references, not evidence that every individual should exactly match them.

ISO Part 2's stated scope is 20 Hz–10 kHz; its abstract points to Part 1 above
10 kHz. Consequently, a file called “ISO Part 2” extending to 20 kHz needs an
explicit extension/source explanation. DF describes spatial averaging of sound
arrival at a head; FF describes a particular incidence. Neither baseline by itself
encodes a distribution of musical preferences. A 5128 full-head DF response is
also not automatically an IEM reference that bypasses the pinna.

### Community references

| Reference | Authority, data availability, and licence finding | App recommendation |
| --- | --- | --- |
| **Super 22** | **Super* Review**, not Crinacle. The original [Squiglink target](https://squig.link/?share=Super_22_Target) is authoritative for that name. A graph-tool fork has a different file named [Super 22 **Adjusted**](https://github.com/kanzai1122/banzai1122.squig.link/blob/edb098a4f06bb14fda4d62de0a216895c6d1d5bd/data/targets/Super%2022%20Adjusted%20Target.txt). No original-author open data grant was verified. | Link/import only. Do not silently substitute Adjusted or “Super Review” for Super 22. |
| **Crinacle / IEF Neutral 2020, 2023; IEF Preference 2025** | Crinacle's [*The New 2025 IEF Target*](https://crinacle.com/2025/02/05/the-new-2025-ief-target/) (2025) documents the lineage and links the authoritative graph tools. He describes 2023 as a modified −0.8 dB/oct DF-based translation, and 2025 as a personal preference curve. Machine-readable graph data are distinct from a paper. No explicit original-target redistribution grant was verified. | Link/import only. Preserve year and fixture; no “Crinacle/Super 22” combined attribution. |
| **oratory1990 Optimum HiFi** | Community target, available as [AutoEq CSV](https://github.com/jaakkopasanen/AutoEq/blob/7ae0f56d53074872b028649617a22bbb4232feb7/targets/oratory1990%20optimum%20hifi%20over-ear.csv) under AutoEq's **MIT** representation. The creator [explains its relationship to B&K's speaker idea](https://www.reddit.com/r/headphones/comments/w2r1we/) (2022). It is an over-ear adaptation, not the original B&K graph. | Included as `oratory1990-optimum-hifi`, optional community reference. Do not claim a new population study or use for speaker microphones. |
| **JM-1 and other squig targets** | Authoritative version/rig and redistribution permission must be established per target. The same fork carries JM-1, IEF and reviewer files, but its [MIT notice](https://github.com/kanzai1122/banzai1122.squig.link/blob/edb098a4f06bb14fda4d62de0a216895c6d1d5bd/LICENSE) credits graph software author Haruto Hiroki. That is insufficient evidence of each external target creator's grant. | Link/import only in this delivery. Public access, download buttons and an open-source graph renderer do not establish provenance for every dataset. |

MIT permits distribution of the selected AutoEq representations with MIT/GPL/AGPL
software when its notice is retained. This does not relicense AES papers, ISO
tables, manufacturer's manuals, trademarks or unrelated measurement collections.
The app currently declares AGPL-3.0-only; data notices still accompany its assets.

## Preference bounds: what the evidence actually provides

### Early Harman work and the 2018/2019 results

Olive, Welti & McMullin (2013), *Listener Preferences for In-Room Loudspeaker and
Headphone Target Responses*, AES 135, paper **8994** ([AES](https://aes.org/publications/elibrary-page/?id=17042)),
used bass/treble adjustments and found individual and training differences. Its
group results/error bars do not constitute a published lower/upper response CSV.
A confidence interval on a **group mean** is not an interval containing 95% of
individual listeners' preferences.

Olive & Welti (2015), *Factors That Influence Listeners’ Preferred Bass and Treble
Levels in Headphones*, AES 139, paper **9382** ([AES](https://aes.org/publications/elibrary-page/?id=17940)),
reports a 249-listener adjustment experiment. Program, age and experience affected
settings. The [author's explanation](https://seanolive.blogspot.com/2015/11/factors-that-influence-listeners.html)
also distinguishes selecting among headphones from adjusting tone controls.
Differences in average settings do not specify full-frequency population bounds.

Olive, Welti & Khonsaripour (2019), *Segmentation of Listeners Based on Their
Preferred Headphone Sound Quality Profiles*, AES 146, paper **10156**
([AES](https://aes.org/publications/elibrary-page/?id=20289)), reanalyzes preference
ratings, including the 2018 AE/OE work. It identifies clusters of taste rather
than a continuous lower/upper tolerance around Harman. Do not cite it as a new
“Harman IE 2019 v2 validation” or reinterpret headphone-rating error bars as dB
limits.

Olive's own **2022 Danish Sound Day presentation**, slide **26**, summarizes:
64% near Harman, 15% favoring 3–6 dB more bass below 300 Hz and about 1 dB more
above 1 kHz, and 21% favoring 2–3 dB less bass and about 1 dB more treble above
1 kHz ([author's slides](https://danishsoundcluster.dk/wp-content/uploads/2022/11/Olive_DSD_2022.pdf)).
These published numbers establish a rough **bass-offset span −3 to +6 dB**
relative to Harman for those summaries: midpoint +1.5 dB, half-span 4.5 dB.
They do **not** establish a symmetric ±3 dB envelope. Nor do they specify shelf
transition shapes or independence of bass and treble preferences. The slides are
copyright Harman, not an open data licence; no figures were digitized.

### Later work, including a usable numerical derivation

Sean Olive (2024), *A comparison of in-ear headphone target curves for the
Brüel & Kjær Head & Torso Simulator Type 5128*, AES 157, paper **10193**
([AES](https://aes.org/publications/elibrary-page/?id=22696)), compares five targets
with 32 listeners. Its abstract describes a 72% cluster and a 28% cluster favoring
roughly 2 dB less bass and 2 dB more treble than the first. This is useful evidence
for personalization on a new fixture, not a 95% preference band. Open access to
the manuscript does not itself grant an MIT licence to its figures.

Sean Olive (2025), *The Preferred In-Ear Headphone Target Curve Based on Method of
Adjustments*, AES International Conference on Headphone Technology, paper **2**
([AES](https://aes.org/publications/elibrary-page/?id=22928)), tested 36 mostly
trained listeners against a B&K 5128 DF baseline. Its **published abstract** gives
cluster ranges of bass +2 to +9 dB and treble −5 to +2 dB; the mean 3 kHz adjustment
was 0.1 dB. The following is a calculation from those stated numbers, not from a
figure:

```text
bass parameter range:    [2, 9] dB = 5.5 ± 3.5 dB
treble parameter range: [-5, 2] dB = -1.5 ± 3.5 dB
```

This is a **derived parameter-space range**. It is relative to DF on the 5128,
not relative to Harman IE 2019. It is neither a confidence interval nor evidence
that every combination in that rectangle was preferred. Only the abstract was
verified for this study; the complete filter definitions and participant-level
covariance were not available in the audited material.

To turn those ranges into an honest full curve one would need the baseline
`D(f)` and study filter responses `L(f; b)` and `H(f; t)`:

```text
C(f; b,t) = D(f) + L(f; b) + H(f; t)
N(C)(f)  = C(f) - C(1000)
lower(f) = min N(C)(f), upper(f) = max N(C)(f)
```

The extrema must use justified combinations of `b,t`; filter tails and
normalization can change which combination is extreme. Inventing shelf centers,
Q values, parameter independence, or extrapolated treble would produce a new
engineering model, not reproduce the study. Therefore **no headphone/IE bounds
JSON is emitted** from these scalar summaries. The derivations above retain
exactly the information that is available.

### Squig preference bands and an actual open dataset

Griffin Silver / listener (2026-10-22), [*What Headphone Sound Profiles Do People
Like?*](https://headphones.com/blogs/features/what-headphone-sound-profiles-do-people-like),
Headphones.com, identifies its original **2023** bounds as an interpretation of
2013 Harman adjustment/confidence-interval results. It describes revised **2026**
GraphWorks bounds, with fixture-specific compensation and a different method.
These generations must not be conflated. This is the primary explanation of
that publisher's bounds, not a peer-reviewed release of raw listener data.
No explicit reusable numeric-data licence was verified; link/import only.

A concrete graph-tool implementation exists: the banzai/Haruto fork's
[configuration](https://github.com/kanzai1122/banzai1122.squig.link/blob/edb098a4f06bb14fda4d62de0a216895c6d1d5bd/assets/js/config.js)
loads [Bounds L.txt](https://github.com/kanzai1122/banzai1122.squig.link/blob/edb098a4f06bb14fda4d62de0a216895c6d1d5bd/assets/pref_bounds/Bounds%20L.txt)
and [Bounds R.txt](https://github.com/kanzai1122/banzai1122.squig.link/blob/edb098a4f06bb14fda4d62de0a216895c6d1d5bd/assets/pref_bounds/Bounds%20R.txt).
Both files were downloaded for inspection. The repository's
[history notes](https://github.com/kanzai1122/banzai1122.squig.link/blob/edb098a4f06bb14fda4d62de0a216895c6d1d5bd/README.md)
credit listener's requests and mention replacing a PNG implementation. They do
not establish original numeric provenance, statistical coverage or a data grant
from the bounds' creator. `L`/`R` are transport channel names, not proof that a
particular file is the lower edge at every frequency. No copied fork bands are
bundled, and no paper figure was traced.

**A real open raw dataset does exist:** Gabriele Ravizza, Julián Villegas,
Christer P. Volk & Tore Stegenborg-Andersen (2023), *Preference ratings and 32
magnitude frequency response curves*, Zenodo v1.0,
[DOI 10.5281/zenodo.8388242](https://doi.org/10.5281/zenodo.8388242).
Its two downloaded CSVs contain individual ratings from 56 Danish/Japanese
assessors and 30-band GEQ representations of 32 tested curves (31 Hz–25 kHz).
The [record API](https://zenodo.org/api/records/8388242) explicitly declares
**CC BY 4.0**. The associated paper is Ravizza et al. (2023), *An Over-Ear
Headphone Target Curve for Brüel & Kjær Head And Torso Simulator Type 5128
measurements*, AES 155, express paper **127**
([AES](https://aes.org/publications/elibrary-page/?id=22281)).

The dataset may be redistributed with attribution, licence notice and disclosure
of changes, including alongside GPL-style software; its assets retain CC BY 4.0.
It is **linked, not bundled here for methodological reasons**, not because reuse
is forbidden. It supplies neither a ready-made bound nor response data at 20 Hz.
Deriving a band would require a declared curve-selection/weighting rule,
subject-level aggregation, filter reconstruction and an explicit treatment of
20–31 Hz. A min/max over all test stimuli would include deliberately disfavored
stimuli; a percentile over them is not a percentile of preferred responses.

## Speaker and room references

### Harman/Olive in-room preference

Olive, Jackson, Devantier & Hunt (2009), *The Subjective and Objective Evaluation
of Room Correction Products*, AES 127, paper **7960**
([AES](https://aes.org/publications/elibrary-page/?id=15154)), found meaningful
preference differences among correction systems. The
[author's account](https://seanolive.blogspot.com/2009/11/subjective-and-objective-evaluation-of.html)
reports that favored corrections had smooth downward in-room responses. His
clarification also says the appropriate response depends on directivity and room
reflectivity; very direct or dead-room listening can need a flatter response.
This supports offering tilt controls, not a uniquely correct −1 dB/octave curve.
The paper's figures are not an openly licensed table or preference-bound dataset.

The 2013 adjustment study above separately compares speakers and headphones;
it should not be used to equate their raw measurement targets. A chosen
Harman-style bass shelf and treble decline is a design unless the exact measured
response and provenance are specified.

### B&K 1974

Henning Møller (1974), *Relevant loudspeaker tests in studios, in Hi-Fi dealers'
demo rooms, in the home etc. — using 1/3 octave, pink-weighted, random noise*,
Brüel & Kjær application note **17-197**, presented at AES 47
([original publisher PDF](https://www.bksv.com/media/doc/17-197.pdf)).
The study involved five loudspeakers, three rooms and five critical listeners.
Figure **5**, printed page 4, supplies the familiar gently declining “optimum”
in-room shape; the text discusses a small bass lift and high-frequency decline.
It is a historical room-response recommendation with limited experimental scope,
not a universal confidence band or a specified constant-slope formula.

The paper was read, but its graph was **not digitized**. No open data licence or
official machine-readable table was established. Link only for the original
curve; do not call a generated straight tilt “the exact B&K 1974 curve.”

### Toole, CTA-2034 and estimated in-room response

Floyd E. Toole (2015), *The Measurement and Calibration of Sound Reproducing
Systems*, **JAES 63(7/8), 512–541**, [DOI 10.17743/jaes.2015.0064](https://doi.org/10.17743/jaes.2015.0064),
[AES record](https://aes.org/publications/elibrary-page/?id=17839), distinguishes
direct sound, reflections and room-dominated bass. A steady-state room curve
alone is an ambiguous description; loudspeaker directivity and the room affect
its slope. A smooth downward predicted response can follow from a speaker whose
direct/listening-window response is flat. Flattening the room curve can then
damage the direct sound. The manuscript/figures are copyright material; no
Toole figure is included as numerical data.

**ANSI/CTA-2034-B (2024)**, *Standard Method of Measurement for In-Home
Loudspeakers* ([CTA](https://www.cta.tech/standards/ansicta-2034-b/)), standardizes
measurements of frequency response, directivity and output. Spinorama/EIR is a
**speaker-dependent prediction**, not a universal target or an uncertainty band.
CTA's standard has no open redistribution grant established here.

For that model, Erin Hardison (2021-04-14), [*Kali Audio IN-5 3-Way
Studio Monitor Review*](https://www.erinsaudiocorner.com/loudspeakers/kali_in-5/),
publishes NFS/spinorama results and explains the common EIR weighting: 12%
listening window, 44% early reflections, 44% sound power. In energy notation:

```text
EIR(f) = 10 log10(0.12 * 10^(LW(f)/10)
                + 0.44 * 10^(ER(f)/10)
                + 0.44 * 10^(SP(f)/10))
```

An estimate for a typical furnished domestic room does not identify this
near-field desk's actual response or its room modes. The review is linked;
its measurement data are not bundled under an assumed software licence.

### Speaker formulas and defaults

Ordinary room-target practice often starts with roughly **−0.5 to −1 dB/octave**
over a substantial mid/treble range, but there is no universal tilt requirement.
As a contemporary manufacturer example, [Dirac's target-curve guide](https://www.dirac.com/resources/target-curve)
describes treble endpoints around −2 to −6 dB at 20 kHz. If one explicitly
anchors those endpoints at 1 kHz and assumes a straight logarithmic slope, that
corresponds to −0.46 to −1.39 dB/octave. **That conversion is ours**, not a Dirac
published constant-slope formula or a listener confidence interval. For studio
practice, [Neumann's MA 1 explanation](https://www.neumann.com/en-sg/products/monitor-accessories/ma-1?variant=509138)
uses a room-specific, editable target rather than prescribing one universal tilt.

The included analytical family is:

```text
x(f) = log2(f / 1000 Hz)
T_s(f) = s * x(f) dB, s in {0, -0.5, -0.7, -1.0} dB/octave
```

Thus −0.7 means **0.7 dB less per doubling of frequency**, or −2.32535 dB per
decade. It produces a 6.97605 dB fall across 20 Hz–20 kHz:

| Hz | T_−0.7 (dB relative to 1 kHz) |
| ---: | ---: |
| 20 | +3.9507 |
| 100 | +2.3253 |
| 1000 | 0 |
| 10000 | −2.3253 |
| 20000 | −3.0253 |

Keep −0.7 as the initial speaker reference. For a new near-field
studio profile, −0.5 is a reasonable comparison; include 0 for strongly direct
listening, and −1.0 for a warmer comparison. This is a recommendation informed
by the room/direct-sound distinction, **not measured preference statistics**.
At the desk, check several nearby microphone positions and preserve reasonable
direct sound. Do not force a tilt's rising bass extension onto a monitor beyond
its usable output, or boost deep cancellation nulls merely to meet the line.

For an optional slope exploration band, we chose endpoints `s_a = -1.0` and
`s_b = -0.5` and computed:

```text
lower(f) = min(s_a*x(f), s_b*x(f))
upper(f) = max(s_a*x(f), s_b*x(f))
```

Both edges meet at 0 dB/1 kHz, and their order swaps around that pivot. This is
the exact envelope of the specified slope family: 20 Hz gives
`[+2.8219, +5.6439]` dB and 20 kHz gives `[−4.3219, −2.1610]` dB. It is defensible
as an **interactive exploration range** containing the default tilt. It is not
defensible as a published population preference band, measurement tolerance or
an instruction to EQ all local peaks inside it. Accordingly it is named
`speaker-tilt-exploration`, marked `derived: true`, and recommended **off** by
default. Contract L's empirical preference-bound requirement remains import-only.

## Data handling and verification

Ten JSON files use precisely the requested fields, 242 points each, Hz/dB, with
20 Hz and 20 kHz endpoints and an explicit 1 kHz anchor. The regular grid is
`20 * 2^(k/24)`; 1 kHz and 20 kHz are inserted. Existing targets are interpolated
linearly in dB against log frequency and normalized by subtracting their
interpolated 1 kHz level. The AutoEq source files stop at 19955.54 Hz; their last
value is held for the final 44.46 Hz (0.00321 octave), **not a measured 20 kHz
extension**. This and four-decimal dB rounding are documented transformations.

`derived: false` identifies an existing target design, even though regridding
and normalization are necessary. `derived: true` identifies the original formula
curves/envelope. No target design, paper figure or missing listener data was
eyeballed or reconstructed. No downloaded figures or manuscripts are shipped.

Run, from any working directory:

```sh
node audivane-dsp/app/scripts/check-research-targets.mjs
```

The checker verifies the exact schema, metadata types, unique IDs, point widths,
finite values, strictly increasing frequencies, 20 Hz–20 kHz coverage, maximum
1/24-octave gaps, lower ≤ upper, and 0 dB ±0.05 at the explicit 1 kHz anchor
(both edges for bounds). An optional directory argument supports checking import
fixtures without editing bundled assets. The report makes a limited source audit,
not an assertion that no other preference dataset can exist.
