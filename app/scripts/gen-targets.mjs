// Downloads the AutoEq target curves and reduces them to the 1/24-octave JSON
// the overlay code draws (`src/assets/targets/`). Run with `npm run gen:targets`
// when the upstream files change, then update the commit in SOURCES.md.
//
// AutoEq: https://github.com/jaakkopasanen/AutoEq (MIT, © 2018-2022 Jaakko Pasanen)
import { mkdirSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

const OUT = resolve("src/assets/targets");
const REPO = "https://raw.githubusercontent.com/jaakkopasanen/AutoEq/master/targets/";
const GIT = "https://github.com/jaakkopasanen/AutoEq/blob/master/targets/";

const F_MIN = 20;
const F_MAX = 20000;
const STEPS = 240; // about 1/24 octave

/** @type {{file: string, out: string, name: string}[]} */
const TARGETS = [
  { file: "Harman over-ear 2018.csv", out: "harman-over-ear-2018.json", name: "Harman over-ear 2018" },
  { file: "Harman in-ear 2019.csv", out: "harman-in-ear-2019.json", name: "Harman in-ear 2019" },
  { file: "Diffuse field ISO 11904-1.csv", out: "diffuse-field-iso-11904-1.json", name: "Diffuse field (ISO 11904-1)" },
];

const grid = Array.from({ length: STEPS + 1 }, (_, i) => F_MIN * Math.pow(F_MAX / F_MIN, i / STEPS));

function parse(text) {
  const points = [];
  for (const line of text.split(/\r?\n/)) {
    const t = line.trim();
    if (!t || /^[*#;]/.test(t)) continue;
    const [f, db] = t.split(/[,;\t]+|\s+/).map(Number);
    if (Number.isFinite(f) && Number.isFinite(db) && f > 0) points.push([f, db]);
  }
  points.sort((a, b) => a[0] - b[0]);
  return points;
}

/** Linear in log frequency, like the app's own resampler. */
function sample(points, f) {
  if (f <= points[0][0]) return points[0][1];
  const last = points[points.length - 1];
  if (f >= last[0]) return last[1];
  let lo = 0;
  let hi = points.length - 1;
  while (hi - lo > 1) {
    const mid = (lo + hi) >> 1;
    if (points[mid][0] <= f) lo = mid;
    else hi = mid;
  }
  const [f0, v0] = points[lo];
  const [f1, v1] = points[hi];
  const t = (Math.log(f) - Math.log(f0)) / (Math.log(f1) - Math.log(f0));
  return v0 + (v1 - v0) * t;
}

const round = (v, digits) => Math.round(v * 10 ** digits) / 10 ** digits;

mkdirSync(OUT, { recursive: true });
for (const t of TARGETS) {
  const url = REPO + encodeURIComponent(t.file);
  const res = await fetch(url, { headers: { "user-agent": "audivane-dsp gen-targets" } });
  if (!res.ok) throw new Error(`${url}: HTTP ${res.status}`);
  const points = parse(await res.text());
  if (points.length < 100) throw new Error(`${t.file}: only ${points.length} points`);
  const data = {
    version: 1,
    name: t.name,
    kind: "target",
    source: `AutoEq · targets/${t.file} · MIT · ${GIT}${encodeURIComponent(t.file)}`,
    points: grid.map((f) => [round(f, 2), round(sample(points, f), 2)]),
  };
  const rows = data.points.map((p) => `    [${p[0]}, ${p[1]}]`).join(",\n");
  const text = `{
  "version": ${data.version},
  "name": ${JSON.stringify(data.name)},
  "kind": "target",
  "source": ${JSON.stringify(data.source)},
  "points": [
${rows}
  ]
}
`;
  writeFileSync(resolve(OUT, t.out), text);
  console.log(`${t.out}: ${points.length} source points → ${data.points.length} at 1/24 octave`);
}
