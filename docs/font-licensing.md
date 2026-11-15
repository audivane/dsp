# Bundled font licensing

The app bundles two type families, licensed under the SIL Open Font License
1.1, separately from Audivane DSP's AGPL-3.0-only code:

| Family (CSS name) | Upstream | Files | Notice |
| --- | --- | --- | --- |
| Audivane Sans Latin | IBM Plex Sans 400/500/600, Latin subset, renamed | `app/src/assets/fonts/ibm-plex-sans-latin-{400,500,600}-normal.woff2` | `app/src/assets/fonts/OFL.txt` |
| Audivane Mono Latin | IBM Plex Mono 400/500, Latin subset, renamed | `app/src/assets/fonts/ibm-plex-mono-latin-{400,500}-normal.woff2` | `app/src/assets/fonts/OFL.txt` |

IBM Plex declares the Reserved Font Name "Plex". A subset is a modified
version under the OFL, so the subsets use the primary names **Audivane Sans
Latin** and **Audivane Mono Latin** in their internal family, unique, full and
PostScript names and in the CSS. Glyphs, outlines, metrics and character
coverage are unchanged, and the IBM copyright and license fields are kept.
The asset filenames identify their origin. These are project derivatives, not
IBM releases. This follows the OFL guidance for modified webfont subsets:
https://openfontlicense.org/webfonts-and-reserved-font-names/

The packages install the OFL notice as
`/usr/share/licenses/audivane-dsp/OFL-IBM-Plex.txt` together with this record.

## Verification

`third_party/fonts.json` records each family's policy and, for every file,
the original, renamed and content SHA-256. `node tools/check-subset-fonts.mjs`
(also run by `packaging/release/release.sh`) decodes each WOFF2 with no
third-party dependencies and checks the inventory, the hashes, that the name
records carry the Audivane family and no reserved name, that the copyright and
OFL URL are intact, that the OFL notice declaring the Reserved Font Name sits
beside the fonts, and that a fingerprint over every table except `name` (and
the head checksum) equals the one recorded from the upstream subset, so the
glyph and metric data are proven identical to the originals.

To reproduce the rename, start from the original fontsource Latin subsets,
install `fonttools[woff]==4.61.1` in a Python virtual environment and run
`tools/rename-subset-fonts.py`. Do not run it on renamed outputs.
