// EQ graph overlays: target curves, preference bounds and measurements.
//
// The input files are the ones people already have: REW text/CSV exports,
// squig.link and AutoEq CSV (`frequency,raw`), or any plain `freq dB` list,
// comma/tab/semicolon/space separated, with an optional header and comment
// lines (`*`, `#`, `;`). Everything is reduced to sorted [Hz, dB] points; the
// graph resamples them onto one log grid (`DRAW_FREQS`).
//
// A curve is normalised at a reference frequency (1 kHz by default) so it
// sits on the graph's 0 dB line, then shifted by the overlay's own offset.
// A measurement can also show "measurement + current EQ" (the predicted
// curve); `eqResponseDb` is the single place that asks the EQ maths for the
// response, so the layered L/R sections need change only there.

import type { EqProfile } from "../api/types";
import { parseNumber } from "./format";
import { profileChannelResponseDb } from "./eqmath";
import harmanInEar2019 from "../assets/targets/harman-in-ear-2019.json";
import harmanOverEar2018 from "../assets/targets/harman-over-ear-2018.json";
import diffuseFieldIso11904 from "../assets/targets/diffuse-field-iso-11904-1.json";
import dfB5128 from "../assets/research-targets/diffuse-field-5128.json";
import dfGrasKemar from "../assets/research-targets/diffuse-field-gras-kemar.json";
import optimumHifi from "../assets/research-targets/oratory1990-optimum-hifi.json";
import speakerTiltBand from "../assets/research-targets/speaker-tilt-exploration.json";

export type OverlayKind = "target" | "bounds" | "measurement";

/** A point on a curve: frequency in Hz, level in dB. */
export type Point = [number, number];

/** Drawn overlays have at least this many points; anything less is a bad file. */
export const MIN_POINTS = 10;

export const DRAW_F_MIN = 20;
export const DRAW_F_MAX = 20000;
const DRAW_STEPS = 240;

/** The log grid every overlay is drawn on: about 1/24 octave, 20 Hz–20 kHz. */
export const DRAW_FREQS: readonly number[] = (() => {
  const out: number[] = [];
  const a = Math.log(DRAW_F_MIN);
  const b = Math.log(DRAW_F_MAX);
  for (let i = 0; i <= DRAW_STEPS; i++) out.push(Math.exp(a + ((b - a) * i) / DRAW_STEPS));
  return out;
})();

/** What an overlay file holds. Stored as JSON in the app's config directory. */
export interface OverlayData {
  version: 1;
  name: string;
  kind: OverlayKind;
  /** Where the curve came from, shown in the overlay menu. */
  source?: string;
  /** Target or measurement. */
  points?: Point[];
  /** Bounds: the lower and upper curves (each from a file or one 3-column file). */
  lower?: Point[];
  upper?: Point[];
  /** Generated tilt target: dB per octave through 0 dB at 1 kHz. */
  tilt_db_per_oct?: number;
  /** How the graph shows it; kept with the file so it survives a restart. */
  view?: OverlayView;
}

export interface OverlayView {
  colour: string;
  visible: boolean;
  offset_db: number;
  /** Measurements: also draw measurement + current EQ. */
  predicted: boolean;
}

/** An overlay as the app holds it: the file plus its display settings. */
export interface Overlay {
  /** Stable id; the overlay's name, also its file name. */
  id: string;
  name: string;
  kind: OverlayKind;
  colour: string;
  visible: boolean;
  offsetDb: number;
  predicted: boolean;
  data: OverlayData;
  /** True for the curves shipped in `src/assets/targets`. */
  builtin: boolean;
}

/** A problem with a file or a name, phrased for a toast. */
export class OverlayError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "OverlayError";
  }
}

export const OVERLAY_COLOURS = ["#e8a33d", "#4fb3e8", "#7bc86c", "#c08bf0", "#ef7a5a", "#35c0b4"];

export const OVERLAY_KIND_LABEL: Record<OverlayKind, string> = {
  target: "Target",
  bounds: "Bounds",
  measurement: "Measurement",
};

const DEFAULT_VIEW: OverlayView = { colour: OVERLAY_COLOURS[0], visible: true, offset_db: 0, predicted: true };

// ------------------------------------------------------------------ parsing

/** Fields of one line: preserve empty cells for comma/tab/semicolon, collapse runs of spaces otherwise. */
function fields(line: string): string[] {
  const parts = /[,;\t]/.test(line) ? line.split(/[,;\t]/) : line.trim().split(/\s+/);
  return parts.map((s) => s.trim().replace(/^["']|["']$/g, ""));
}

function number_(s: string | undefined): number | null {
  const t = s?.trim();
  if (!t) return null;
  const v = Number(t);
  return Number.isFinite(v) ? v : null;
}

/** Sorted by frequency, finite and positive only, one point per frequency. */
export function tidyPoints(raw: readonly Point[]): Point[] {
  const kept = raw.filter((p) => Number.isFinite(p[0]) && Number.isFinite(p[1]) && p[0] > 0);
  kept.sort((a, b) => a[0] - b[0]);
  const out: Point[] = [];
  for (const p of kept) {
    if (out.length && out[out.length - 1][0] === p[0]) out[out.length - 1] = [p[0], p[1]];
    else out.push([p[0], p[1]]);
  }
  return out;
}

/** Read every data line into fixed level column slots (frequency is column 0). */
function readColumns(text: string): [Point[], Point[], Point[]] {
  const curves: [Point[], Point[], Point[]] = [[], [], []];
  for (const raw of text.replace(/^\uFEFF/, "").split(/\r?\n/)) {
    const line = raw.replace(/\r$/, "");
    const trimmed = line.trim();
    if (!trimmed || /^[*#;]/.test(trimmed) || trimmed.startsWith("//")) continue;
    const f = fields(line);
    // A header line, a descriptive line or a broken row: no numbers to keep.
    if (f.length < 2) continue;
    const nums = f.map(number_);
    const freq = nums[0];
    if (freq === null) continue;
    for (let c = 1; c < Math.min(4, nums.length); c++) {
      const level = nums[c];
      if (level !== null) curves[c - 1].push([freq, level]);
    }
  }
  return curves;
}

function countPoints(points: readonly Point[], name: string): void {
  if (points.length < MIN_POINTS) {
    throw new OverlayError(
      `“${name}” has ${points.length} usable ${points.length === 1 ? "point" : "points"}; an overlay needs at least ${MIN_POINTS}.`,
    );
  }
}

/**
 * Parse a target or measurement file (one curve; extra columns are ignored),
 * or a bounds file (frequency, lower, upper).
 */
export function parseOverlayText(text: string, opts: { kind: OverlayKind; name: string }): OverlayData {
  const { kind, name } = opts;
  const curves = readColumns(text);
  if (curves.every((c) => c.length === 0)) {
    throw new OverlayError(`“${name}” has no frequency/level rows in it.`);
  }

  if (kind === "bounds") {
    if (curves[0].length > 0 && curves[1].length === 0 && curves[2].length === 0) {
      throw new OverlayError(
        `“${name}” has one level column; a bounds file needs frequency, lower and upper (or import two files).`,
      );
    }
    const lower = tidyPoints(curves[0]);
    const upper = tidyPoints(curves[1]);
    countPoints(lower, name);
    countPoints(upper, name);
    return { version: 1, name, kind, lower, upper };
  }

  const points = tidyPoints(curves[0]);
  countPoints(points, name);
  return { version: 1, name, kind, points };
}

/** Preference bounds from two single-curve files. */
export function boundsFromCurves(lowerText: string, upperText: string, name: string): OverlayData {
  const lower = parseOverlayText(lowerText, { kind: "target", name }).points!;
  const upper = parseOverlayText(upperText, { kind: "target", name }).points!;
  return { version: 1, name, kind: "bounds", lower, upper };
}

// ------------------------------------------------------------- resampling

/** The curve's level at `f`, linearly interpolated in log frequency. */
export function sampleAt(points: readonly Point[], f: number): number | null {
  if (!points.length || !(f > 0)) return null;
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
  if (!(f1 > f0)) return v1;
  const t = (Math.log(f) - Math.log(f0)) / (Math.log(f1) - Math.log(f0));
  return v0 + (v1 - v0) * t;
}

/** Sample a curve onto any frequency grid (null when there is no curve). */
export function resample(points: readonly Point[] | undefined, grid: readonly number[]): (number | null)[] {
  if (!points || points.length < 2) return grid.map(() => null);
  return grid.map((f) => sampleAt(points, f));
}

/** Level to subtract so the curve reads 0 dB at `refHz`. */
export function normalisationDb(points: readonly Point[], refHz: number): number {
  return sampleAt(points, refHz) ?? 0;
}

/** A curve as the graph draws it: normalised at `refHz`, then offset. */
export function displayPoints(points: readonly Point[], refHz: number, offsetDb = 0): Point[] {
  const norm = normalisationDb(points, refHz);
  return points.map(([f, db]) => [f, db - norm + offsetDb]);
}

/**
 * Bounds are shifted as one band: their width at the reference frequency must
 * stay visible, so the band is centred on 0 dB there instead of each edge
 * being normalised on its own.
 */
export function boundsNormalisationDb(lower: readonly Point[], upper: readonly Point[], refHz: number): number {
  return (normalisationDb(lower, refHz) + normalisationDb(upper, refHz)) / 2;
}

export function displayBounds(
  lower: readonly Point[],
  upper: readonly Point[],
  refHz: number,
  offsetDb = 0,
): { lower: Point[]; upper: Point[] } {
  const norm = boundsNormalisationDb(lower, upper, refHz);
  const shift = (pts: readonly Point[]) => pts.map(([f, db]) => [f, db - norm + offsetDb] as Point);
  return { lower: shift(lower), upper: shift(upper) };
}

// ------------------------------------------------------------------ EQ link

/**
 * The EQ's response in dB at `freqs`.
 *
 * Layered EQ: the combined response of each channel (global preamp + enabled
 * L+R section + that channel's enabled section). When L and R differ, the
 * predicted curve uses their mean in dB. Nothing else in the overlay code
 * knows how the EQ is shaped.
 */
export function eqResponseDb(eq: EqProfile | null, fs: number, freqs: readonly number[]): number[] {
  if (!eq) return freqs.map(() => 0);
  const l = profileChannelResponseDb(eq, 0, fs, freqs);
  const r = profileChannelResponseDb(eq, 1, fs, freqs);
  return l.map((v, i) => (v + r[i]) / 2);
}

const PREDICT_STEPS = 480;
const PREDICT_FREQS: readonly number[] = (() => {
  const out: number[] = [];
  const a = Math.log(DRAW_F_MIN);
  const b = Math.log(DRAW_F_MAX);
  for (let i = 0; i <= PREDICT_STEPS; i++) out.push(Math.exp(a + ((b - a) * i) / PREDICT_STEPS));
  return out;
})();

function eqBandFreqs(eq: EqProfile | null): number[] {
  if (!eq) return [];
  const out: number[] = [];
  if (eq.linked_enabled) {
    for (const b of eq.bands) if (b.enabled) out.push(b.freq_hz);
  }
  if (eq.left?.enabled) {
    for (const b of eq.left.bands) if (b.enabled) out.push(b.freq_hz);
  }
  if (eq.right?.enabled) {
    for (const b of eq.right.bands) if (b.enabled) out.push(b.freq_hz);
  }
  return out;
}

/**
 * Measurement + current EQ: what the speakers or headphones should do with the
 * EQ applied. Normalised with the measurement, not on its own, so an EQ change
 * at the reference frequency still shows up as a change. Evaluated on a dense
 * log grid (and any active band centers) so narrow EQ filters show accurately
 * even when the underlying measurement has sparse frequency points.
 */
export function predictedPoints(measurement: readonly Point[], eq: EqProfile | null, fs: number, refHz: number, offsetDb = 0): Point[] {
  if (!measurement.length) return [];
  const norm = normalisationDb(measurement, refHz);
  const minF = measurement[0][0];
  const maxF = measurement[measurement.length - 1][0];
  const rawFreqs: number[] = [];
  for (const [f] of measurement) rawFreqs.push(f);
  if (maxF > minF) {
    for (const f of PREDICT_FREQS) {
      if (f > minF && f < maxF) rawFreqs.push(f);
    }
    for (const f of eqBandFreqs(eq)) {
      if (f > minF && f < maxF) rawFreqs.push(f);
    }
  }
  rawFreqs.sort((a, b) => a - b);
  const freqs: number[] = [];
  for (const f of rawFreqs) {
    if (freqs.length === 0 || f - freqs[freqs.length - 1] > freqs[freqs.length - 1] * 1e-6) {
      freqs.push(f);
    }
  }
  const eqDb = eqResponseDb(eq, fs, freqs);
  return freqs.map((f, i) => [f, (sampleAt(measurement, f) ?? 0) + eqDb[i] - norm + offsetDb]);
}

// ---------------------------------------------------------------- built-ins

export function tiltTarget(dbPerOct: number, refHz = 1000): OverlayData {
  const sign = dbPerOct > 0 ? "+" : dbPerOct < 0 ? "−" : "";
  const name = `Tilt ${sign}${Math.abs(dbPerOct).toFixed(1)} dB/oct`;
  return {
    version: 1,
    name,
    kind: "target",
    source: "Generated: a straight line in log frequency through 0 dB at 1 kHz.",
    tilt_db_per_oct: dbPerOct,
    points: DRAW_FREQS.map((f) => [f, dbPerOct * Math.log2(f / refHz)] as Point),
  };
}

export function flatTarget(): OverlayData {
  return {
    version: 1,
    name: "Flat",
    kind: "target",
    source: "Generated: 0 dB at every frequency.",
    points: DRAW_FREQS.map((f) => [f, 0] as Point),
  };
}

/** A shipped target, if its data is whole; a broken one is left out, not fatal. */
function builtin(target: unknown): OverlayData | null {
  const data = target as OverlayData;
  const points = data.kind === "bounds" ? data.lower : data.points;
  if (!points || points.length < MIN_POINTS) return null;
  return data;
}

interface ResearchCurve {
  name: string;
  kind: "target" | "bounds";
  source: string;
  url?: string;
  license?: string;
  points: number[][];
}

/** Curves from the research set (`src/assets/research-targets/SOURCES.md`). */
function research(raw: unknown): OverlayData | null {
  const r = raw as ResearchCurve;
  const source = [r.source, r.license, r.url].filter(Boolean).join(" · ");
  if (r.kind === "bounds") {
    return builtin({
      version: 1, name: r.name, kind: "bounds", source,
      lower: r.points.map((p) => [p[0], p[1]] as Point),
      upper: r.points.map((p) => [p[0], p[2]] as Point),
    });
  }
  return builtin({ version: 1, name: r.name, kind: "target", source, points: r.points.map((p) => [p[0], p[1]] as Point) });
}

/**
 * Built-in curves: targets from the AutoEq project (MIT; `src/assets/targets/SOURCES.md`)
 * plus the researched set (`src/assets/research-targets/SOURCES.md`).
 */
export const BUILTIN_TARGETS: readonly OverlayData[] = [
  builtin(harmanOverEar2018),
  builtin(harmanInEar2019),
  builtin(diffuseFieldIso11904),
  research(dfB5128),
  research(dfGrasKemar),
  research(optimumHifi),
  research(speakerTiltBand),
].filter((d): d is OverlayData => d !== null);

// ------------------------------------------------------------- runtime model

export function overlayData(raw: unknown): OverlayData | null {
  if (!raw || typeof raw !== "object") return null;
  const d = raw as Partial<OverlayData>;
  if (typeof d.name !== "string" || !d.name.trim()) return null;
  if (d.kind !== "target" && d.kind !== "bounds" && d.kind !== "measurement") return null;
  const points = d.kind === "bounds" ? (d.lower ?? []) : (d.points ?? []);
  if (!Array.isArray(points) || points.length < 2) return null;
  return d as OverlayData;
}

const HEX = /^#[0-9a-f]{3,8}$/i;

/** Turn a stored/parsed file into the app's overlay (colour from the palette). */
export function toOverlay(data: OverlayData, opts: { colour?: string; builtin?: boolean } = {}): Overlay {
  const view = data.view ?? DEFAULT_VIEW;
  const colour = opts.colour ?? view.colour;
  return {
    id: data.name,
    name: data.name,
    kind: data.kind,
    colour: typeof colour === "string" && HEX.test(colour) ? colour : DEFAULT_VIEW.colour,
    visible: view.visible !== false,
    offsetDb: Number.isFinite(view.offset_db) ? view.offset_db : 0,
    predicted: view.predicted !== false,
    data,
    builtin: opts.builtin ?? false,
  };
}

/** The file to store for an overlay: its curve plus the current display settings. */
export function overlayFile(o: Overlay): OverlayData {
  return { ...o.data, name: o.name, view: { colour: o.colour, visible: o.visible, offset_db: Math.round(o.offsetDb * 10) / 10, predicted: o.predicted } };
}

/** A name that is not taken yet: "name (2)", "name (3)", … */
export function uniqueName(name: string, taken: readonly string[]): string {
  const used = new Set(taken);
  if (!used.has(name)) return name;
  for (let i = 2; ; i++) {
    const next = `${name} (${i})`;
    if (!used.has(next)) return next;
  }
}

/** A file name → an overlay name: no directory, no extension. */
export function overlayNameFromFile(fileName: string): string {
  const base = fileName.split(/[\\/]/).pop() ?? fileName;
  const stem = base.replace(/\.[^.]*$/, "").trim();
  return stem || "Imported overlay";
}

/** Parse what a person types into the reference-frequency field. */
export function parseRefHz(text: string): number | null {
  const v = parseNumber(text);
  if (v === null || !(v >= 10) || !(v <= 20000)) return null;
  return Math.round(v);
}
