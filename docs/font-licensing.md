# Bundled font licensing

The app bundles two type families, licensed under the SIL Open Font License
1.1, separately from Audivane DSP's AGPL-3.0-only code:

| Family (CSS name) | Upstream | Files | Notice |
| --- | --- | --- | --- |
| Geist | Geist 1.9.2 variable, weights 400 to 600, Latin subset | `app/src/assets/fonts/geist-latin-wght.woff2` | `app/src/assets/fonts/OFL-Geist.txt` |
| Audivane Mono Latin | IBM Plex Mono 400/500, Latin subset, renamed | `app/src/assets/fonts/ibm-plex-mono-latin-{400,500}-normal.woff2` | `app/src/assets/fonts/OFL-IBM-Plex.txt` |

Geist sets the interface text; Audivane Mono Latin sets numbers and values.

## Geist

Geist (copyright The Geist Project Authors) declares no Reserved Font Name, so
the subset keeps its own names. It was made from
`dist/fonts/geist-sans/Geist-Variable.ttf` in the `geist@1.9.2` npm package
with `fonttools[woff]==4.61.1`: `fonttools varLib.instancer` limits the weight
axis to 400-600, then `pyftsubset` keeps the Latin code points and all layout
features and writes WOFF2. The upstream file's SHA-256, the code points and
the exact command are recorded in `third_party/fonts.json`. Glyph outlines and
metrics inside the kept range are as upstream.

## IBM Plex Mono

IBM Plex declares the Reserved Font Name "Plex". A subset is a modified
version under the OFL, so the subsets use the primary name **Audivane Mono
Latin** in their internal family, unique, full and PostScript names and in the
CSS. Glyphs, outlines, metrics and character coverage are unchanged, and the
IBM copyright and license fields are kept. The asset filenames identify their
origin. These are project derivatives, not IBM releases. This follows the OFL
guidance for modified webfont subsets:
https://openfontlicense.org/webfonts-and-reserved-font-names/

To reproduce the rename, start from the original fontsource Latin subsets,
install `fonttools[woff]==4.61.1` in a Python virtual environment and run
`tools/rename-subset-fonts.py`. Do not run it on renamed outputs.

## Packages

The packages install the notices as
`/usr/share/licenses/audivane-dsp/OFL-Geist.txt` and `OFL-IBM-Plex.txt`
together with this record.

## Verification

`third_party/fonts.json` records each family's policy and, for every file,
its SHA-256 and a content fingerprint. `node tools/check-subset-fonts.mjs`
(also run by `packaging/release/release.sh`) decodes each WOFF2 with no
third-party dependencies and checks the inventory, the hashes, the name
records (the renamed family carries the Audivane name and no reserved name;
Geist carries its own name and no Audivane name), that the copyright and OFL
URL are intact and that each family's OFL notice sits beside the fonts. For
the renamed subsets, a fingerprint over every table except `name` (and the
head checksum) must equal the one recorded from the upstream subset, so the
glyph and metric data are proven identical to the originals. For Geist, the
file and fingerprint must match the ones recorded for the output of the
recorded subset command.
