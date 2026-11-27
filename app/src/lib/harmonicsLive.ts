// The Harmonics page's live analysis data: the engine's `harmonics_live`
// frames kept as a bounded history (a few points per second, the newest ten
// minutes), bucketed for the history chart, plus helpers for the spectrum
// (peak hold, dBr) and the "you are here" markers.
//
// The history lives in the app: the engine only analyses while someone
// watches, so there is nothing to keep while the page is closed. One buffer
// per session (module level), so leaving the page and coming back keeps what
// was seen.

import type { HarmonicsLive } from "../api/types";

/** One history point (a frame of music). `null` levels: a gap (silence, idle, off). */
export interface LivePoint {
  /** Wall clock, ms. */
  t: number;
  /** Added content, dBc (relative to the music out). */
  added: number | null;
  /** The loudest stretch of the frame's last second, dBc. */
  peak: number | null;
  /** The deepest sag's turn-down now, dB (≥ 0; 0 without sag). */
  sag: number | null;
  /** The music's level, dBFS (sine-equivalent). */
  music: number | null;
  /** Each part's level, dBc, by key (`h2`.., `even`, `odd`). */
  parts: Record<string, number>;
}

/** History length: ten minutes. */
export const HISTORY_MS = 10 * 60 * 1000;
/** At most this many points (10 min at 5 frames/s, with room). */
export const HISTORY_CAP = 3600;

/** Time ranges of the history chart, ms. */
export const HISTORY_RANGES = [30_000, 120_000, 600_000] as const;
export type HistoryRange = (typeof HISTORY_RANGES)[number];

/** A frame as a history point (a gap unless it is music). */
export function pointOf(f: HarmonicsLive, t: number): LivePoint {
  if (f.state !== "playing") return { t, added: null, peak: null, sag: null, music: null, parts: {} };
  const parts: Record<string, number> = {};
  for (const p of f.parts) parts[p.key] = p.level_db;
  return {
    t,
    added: f.added_db,
    peak: f.added_peak_db,
    sag: f.layers.reduce((m, l) => Math.max(m, l.sag_db), 0),
    music: f.music_level_dbfs,
    parts,
  };
}

/** A bounded history of frames: one point per new frame (by `seq`), the oldest dropped past ten minutes or the cap. */
export class LiveHistory {
  points: LivePoint[] = [];
  private lastSeq = -1;
  private lastGapAt = -Infinity;

  /** Add `f` seen at `t` (ms); false when it was not new. A non-music frame adds at most one gap point per second. */
  push(f: HarmonicsLive, t: number): boolean {
    if (f.state === "playing") {
      if (f.seq === this.lastSeq) return false;
      this.lastSeq = f.seq;
    } else {
      if (t - this.lastGapAt < 1000) return false;
      this.lastGapAt = t;
    }
    this.points.push(pointOf(f, t));
    const cut = t - HISTORY_MS;
    let drop = 0;
    while (drop < this.points.length && (this.points[drop].t < cut || this.points.length - drop > HISTORY_CAP)) drop++;
    if (drop) this.points.splice(0, drop);
    return true;
  }

  clear(): void {
    this.points = [];
    this.lastSeq = -1;
    this.lastGapAt = -Infinity;
  }
}

/** The session's history (survives leaving the page). */
export const liveHistory = new LiveHistory();

/** One bucket of the history chart: power means (dB) and maxima; `null` when it holds no music. */
export interface LiveBucket {
  t0: number;
  t1: number;
  added: number | null;
  peak: number | null;
  sag: number | null;
  music: number | null;
  parts: Record<string, number>;
  /** Points inside it. */
  n: number;
}

const toPow = (db: number) => Math.pow(10, db / 10);
const toDb = (p: number) => (p > 0 ? 10 * Math.log10(p) : -160);

/**
 * `points` in `[t1 − span, t1]` as `n` equal buckets: the added level and
 * each part as the mean power of its frames, the peak band and the sag as
 * their maxima, the music as its mean power.
 */
export function bucketHistory(points: readonly LivePoint[], t1: number, span: number, n: number): LiveBucket[] {
  const t0 = t1 - span;
  const w = span / n;
  const acc = Array.from({ length: n }, (_, i) => ({ t0: t0 + i * w, t1: t0 + (i + 1) * w, a: 0, pk: -Infinity, sg: -Infinity, m: 0, parts: {} as Record<string, number>, n: 0 }));
  for (const p of points) {
    if (p.t < t0 || p.t > t1 || p.added === null) continue;
    const i = Math.min(n - 1, Math.floor((p.t - t0) / w));
    const b = acc[i];
    b.a += toPow(p.added);
    b.pk = Math.max(b.pk, p.peak ?? p.added);
    b.sg = Math.max(b.sg, p.sag ?? 0);
    b.m += toPow(p.music ?? -160);
    for (const [k, v] of Object.entries(p.parts)) b.parts[k] = (b.parts[k] ?? 0) + toPow(v);
    b.n++;
  }
  return acc.map((b) => {
    if (!b.n) return { t0: b.t0, t1: b.t1, added: null, peak: null, sag: null, music: null, parts: {}, n: 0 };
    const parts: Record<string, number> = {};
    for (const [k, v] of Object.entries(b.parts)) parts[k] = toDb(v / b.n);
    return { t0: b.t0, t1: b.t1, added: toDb(b.a / b.n), peak: b.pk, sag: b.sg, music: toDb(b.m / b.n), parts, n: b.n };
  });
}

/** The newest point with music in `points`, if any. */
export function lastMusic(points: readonly LivePoint[]): LivePoint | null {
  for (let i = points.length - 1; i >= 0; i--) if (points[i].added !== null) return points[i];
  return null;
}

/** A peak-hold spectrum: the larger of `held` and `next` per bin (a fresh copy of `next` when the grids differ). */
export function holdSpectrum(held: readonly number[] | null, next: readonly number[]): number[] {
  if (!held || held.length !== next.length) return [...next];
  return next.map((v, i) => Math.max(v, held[i]));
}

/** The log bin centres of a frame, Hz. */
export function liveFreqs(f: Pick<HarmonicsLive, "f_min_hz" | "bins_per_octave">, count: number): number[] {
  return Array.from({ length: count }, (_, k) => f.f_min_hz * Math.pow(2, k / f.bins_per_octave));
}

/** Where the music's energy is, 20 Hz–20 kHz: per bin a weight in [0, 1] (0 dB down from its loudest bin → 1, `range` dB down → 0). */
export function musicWeights(music: readonly number[], freqs: readonly number[], range = 30): number[] {
  let top = -Infinity;
  music.forEach((v, i) => {
    if (freqs[i] >= 20 && freqs[i] <= 20000) top = Math.max(top, v);
  });
  return music.map((v, i) => (freqs[i] < 20 || freqs[i] > 20000 || !Number.isFinite(top) ? 0 : Math.max(0, Math.min(1, 1 - (top - v) / range))));
}

/** A level in dB as text with a real minus: "−38 dB". */
export const fmtLiveDb = (db: number, digits = 0) => (db < 0 ? "−" : "") + Math.abs(db).toFixed(digits);

/** The legend label of a part key. */
export function partLabel(key: string): string {
  if (key === "even") return "Even orders";
  if (key === "odd") return "Odd orders";
  const k = Number(key.slice(1));
  return Number.isFinite(k) ? `H${k}` : key;
}

/** The tooltip of a part key: what the line is. */
export function partTitle(key: string): string {
  if (key === "even") return "The even part of the stage's curve, (f(x) + f(−x)) / 2: every even order's harmonics of each note and the intermodulation between them.";
  if (key === "odd") return "The odd part of the stage's curve, (f(x) − f(−x)) / 2, less what follows the music's level.";
  const k = Number(key.slice(1));
  const nth = ["", "", "2nd", "3rd", "4th", "5th", "6th", "7th", "8th", "9th"][k] ?? `${k}th`;
  return `${nth}-order products: H${k} of every note plus the intermodulation this order makes between notes (on music they cannot be told apart; together they are exact).`;
}

/** A stable numeric key for the shared hide set (`H k` → k, even → 100, odd → 101, the total → 1). */
export function partHideKey(key: string): number {
  if (key === "even") return 100;
  if (key === "odd") return 101;
  const k = Number(key.slice(1));
  return Number.isFinite(k) ? k : 0;
}

/**
 * GedLee Gm at `levelDbfs` from a level sweep's points (sines every 2 dB,
 * −40..0 dBFS), interpolated linearly and clamped to the sweep's range;
 * null without a sweep. Gm weighs the curve over the swing the signal
 * uses, so for music the level to read it at is the music's peak.
 */
export function gmAt(points: readonly { input_dbfs: number; gedlee_gm: number }[], levelDbfs: number): number | null {
  if (points.length === 0 || !Number.isFinite(levelDbfs)) return null;
  const pts = [...points].sort((a, b) => a.input_dbfs - b.input_dbfs);
  if (levelDbfs <= pts[0].input_dbfs) return pts[0].gedlee_gm;
  const last = pts[pts.length - 1];
  if (levelDbfs >= last.input_dbfs) return last.gedlee_gm;
  for (let i = 1; i < pts.length; i++) {
    const a = pts[i - 1];
    const b = pts[i];
    if (levelDbfs <= b.input_dbfs) {
      const t = (levelDbfs - a.input_dbfs) / (b.input_dbfs - a.input_dbfs || 1);
      return a.gedlee_gm + t * (b.gedlee_gm - a.gedlee_gm);
    }
  }
  return last.gedlee_gm;
}
