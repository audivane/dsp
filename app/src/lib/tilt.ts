// The EQ tilt (`EqProfile.tilt`, plus `tilt_high` for a two-section tilt) as
// the engine defines its target (crates/dsp-core/src/tilt.rs): 0 dB below `start_hz`, a constant dB/octave
// slope up to the effective end (`min(end_hz, 0.45 * fs)`), flat above,
// shifted so the pivot sits at 0 dB. The engine's minimum-phase design stays
// within 0.1 dB of it away from the corners (0.16 dB at 6 dB/oct inside
// them), so the graph and the auto preamp use the target itself.

import type { EqProfile, TiltConfig, TiltPivot } from "../api/types";
import { fmtFreq } from "./format";

export const TILT_MAX_SLOPE = 6;
export const TILT_MAX_TOTAL = 24;
export const TILT_MIN_START_HZ = 20;

/** A new tilt: −1 dB/oct over the whole band, 0 dB at 1 kHz. */
export function defaultTilt(): TiltConfig {
  return { enabled: true, slope_db_per_oct: -1, total_db: null, start_hz: 20, end_hz: 20000, pivot: { hz: 1000 } };
}

/** The end the engine uses at `fs` (clamped to 0.45 fs). */
export function effectiveEndHz(t: TiltConfig, fs: number): number {
  return Math.min(t.end_hz, 0.45 * fs);
}

/** Slope in dB/oct: given, or derived from `total_db` over the requested band. */
export function tiltSlope(t: TiltConfig): number | null {
  const oct = Math.log2(t.end_hz / t.start_hz);
  if (t.slope_db_per_oct != null && t.total_db == null) return t.slope_db_per_oct;
  if (t.total_db != null && t.slope_db_per_oct == null) return Number.isFinite(oct) && oct > 0 ? t.total_db / oct : null;
  return null;
}

/** Change from start to end over the requested band, dB. */
export function tiltTotal(t: TiltConfig): number | null {
  const s = tiltSlope(t);
  if (s === null) return null;
  return s * Math.log2(t.end_hz / t.start_hz);
}

function sharp(slope: number, start: number, end: number, f: number): number {
  if (f <= start) return 0;
  return slope * Math.min(Math.log2(f / start), Math.log2(end / start));
}

/** Why the engine would refuse this tilt at `fs` (its rules, in plain words), or null. */
export function tiltProblem(t: TiltConfig, fs: number): string | null {
  if (!t.enabled) return null;
  if ((t.slope_db_per_oct == null) === (t.total_db == null)) return "Set either a slope or a total, not both.";
  const s = tiltSlope(t);
  const total = tiltTotal(t);
  if (s === null || total === null || !Number.isFinite(s)) return "The band needs a start below its end.";
  if (Math.abs(s) > TILT_MAX_SLOPE + 1e-9) return `The slope must stay within ±${TILT_MAX_SLOPE} dB per octave.`;
  if (Math.abs(total) > TILT_MAX_TOTAL + 1e-9) return `The total must stay within ±${TILT_MAX_TOTAL} dB.`;
  if (!(t.start_hz >= TILT_MIN_START_HZ) || !(t.end_hz <= TILT_MAX_END_HZ)) return "The band must lie within 20 Hz–20 kHz.";
  const end = effectiveEndHz(t, fs);
  if (!(t.end_hz > t.start_hz) || !(end > t.start_hz) || Math.log2(end / t.start_hz) < 1 / 6) {
    return "The band must span at least 1/6 octave (the end is capped at 0.45 × the sample rate).";
  }
  const p = t.pivot;
  if (typeof p === "object" && (!Number.isFinite(p.hz) || p.hz < t.start_hz || p.hz > t.end_hz)) {
    return "A pivot in Hz must lie inside the band.";
  }
  return null;
}

/** Pivot offset: the target's value at the pivot before normalization. */
function pivotOffset(pivot: TiltPivot, slope: number, start: number, end: number): number {
  if (pivot === "start") return 0;
  if (pivot === "end") return sharp(slope, start, end, end);
  if (pivot === "center") return sharp(slope, start, end, Math.sqrt(start * end));
  // Above the effective end the target is flat: the `end` pivot.
  return sharp(slope, start, end, Math.min(pivot.hz, end));
}

/**
 * The tilt's response in dB at each frequency, at sample rate `fs`. Zero
 * for a disabled tilt, and for one the engine would refuse (it would not
 * run either).
 */
export function tiltDb(t: TiltConfig | null | undefined, fs: number, freqs: readonly number[]): number[] {
  if (!t || !t.enabled || tiltProblem(t, fs) !== null) return freqs.map(() => 0);
  const slope = tiltSlope(t) as number;
  const start = t.start_hz;
  const end = effectiveEndHz(t, fs);
  if (Math.abs(slope * Math.log2(end / start)) < 0.001) return freqs.map(() => 0);
  const off = pivotOffset(t.pivot, slope, start, end);
  return freqs.map((f) => sharp(slope, start, end, f) - off);
}

/**
 * Why the engine would refuse a two-section tilt (`check_tilt_pair`), or
 * null. It applies with either section off too.
 */
export function tiltPairProblem(low: TiltConfig, high: TiltConfig): string | null {
  if (!(low.end_hz <= high.start_hz)) return "The bass corner must be below the treble corner.";
  if (low.pivot !== "end" || high.pivot !== "start") return "With two sections, the bass pivots at its end and the treble at its start.";
  return null;
}

/** The sections of `eq`'s tilt stage: `tilt`, then `tilt_high`. */
export function tiltSections(eq: EqProfile | null | undefined): TiltConfig[] {
  return [eq?.tilt, eq?.tilt_high].filter((t): t is TiltConfig => !!t);
}

/** True if any tilt section is on. */
export function tiltStageOn(eq: EqProfile | null | undefined): boolean {
  return tiltSections(eq).some((t) => t.enabled);
}

/** The whole tilt stage in dB: both sections summed, or 0 when the engine would refuse the pair. */
export function tiltStageDb(eq: EqProfile | null | undefined, fs: number, freqs: readonly number[]): number[] {
  if (eq?.tilt && eq.tilt_high && tiltPairProblem(eq.tilt, eq.tilt_high) !== null) return freqs.map(() => 0);
  const out = freqs.map(() => 0);
  for (const t of tiltSections(eq)) tiltDb(t, fs, freqs).forEach((v, i) => (out[i] += v));
  return out;
}

/** Largest boost of the tilt anywhere, dB (≥ 0). */
export function tiltPeakDb(t: TiltConfig | null | undefined, fs: number): number {
  if (!t || !t.enabled) return 0;
  const v = tiltDb(t, fs, [1, t.start_hz, effectiveEndHz(t, fs), 0.5 * fs]);
  return Math.max(0, ...v);
}

export function pivotText(p: TiltPivot): string {
  if (p === "start") return "start";
  if (p === "end") return "end";
  if (p === "center") return "centre";
  return `${Math.round(p.hz)} Hz`;
}

/** "1 kHz", "200 Hz". */
export function cornerText(hz: number): string {
  if (hz >= 1000) return `${fmtFreq(hz).replace(/k$/, "")} kHz`;
  return `${Math.round(hz)} Hz`;
}

/** Sidebar / header wording: "tilt −1.0 dB/oct", "tilt −6.0 dB above 1 kHz". */
export function tiltSummary(t: TiltConfig | null | undefined): string | null {
  if (!t || !t.enabled) return null;
  const f = (v: number) => (v > 0 ? "+" : v < 0 ? "−" : "") + Math.abs(v).toFixed(1);
  const { shape, hz } = tiltShape(t);
  const where = shape === "whole" ? "" : ` ${shape} ${cornerText(hz)}`;
  if (t.total_db != null) return `tilt ${f(t.total_db)} dB${where}`;
  if (t.slope_db_per_oct != null) return `tilt ${f(t.slope_db_per_oct)} dB/oct${where}`;
  return "tilt";
}

/** The whole stage: "tilt +2.0 dB/oct below 200 Hz, −1.0 dB/oct above 1 kHz" for two sections. */
export function tiltStageSummary(eq: EqProfile | null | undefined): string | null {
  const low = eq?.tilt ?? null;
  const high = eq?.tilt_high ?? null;
  if (!high) return tiltSummary(low);
  const f = (v: number) => (v > 0 ? "+" : v < 0 ? "−" : "") + Math.abs(v).toFixed(1);
  const part = (t: TiltConfig | null, where: string) => {
    if (!t?.enabled) return null;
    const amount = t.total_db != null ? `${f(t.total_db)} dB` : `${f(tiltSlope(t) ?? 0)} dB/oct`;
    return `${amount} ${where}`;
  };
  const parts = [part(low, `below ${cornerText(low?.end_hz ?? 0)}`), part(high, `above ${cornerText(high.start_hz)}`)].filter(Boolean);
  return parts.length ? `tilt ${parts.join(", ")}` : null;
}

// The panel's four shapes, on top of the engine's band and pivot.
// Whole band: 20 Hz to 20 kHz, any pivot. Above X: X to 20 kHz, pivot at
// the start (flat below X). Below X: 20 Hz to X, pivot at the end (flat
// above X). Both: Below X in `tilt` and Above Y in `tilt_high`, X < Y.
export type TiltShape = "whole" | "above" | "below" | "both";
export const TILT_LO_HZ = 20;
export const TILT_HI_HZ = 20000;
const MIN_OCT = 1 / 6;

/**
 * The shape a tilt reads as, and its corner (for whole band: the pivot in
 * Hz, or 1 kHz). A band that matches no shape (set from the CLI) reads as
 * the closest one.
 */
export function tiltShape(t: TiltConfig): { shape: TiltShape; hz: number } {
  const offStart = Math.abs(Math.log2(t.start_hz / TILT_LO_HZ));
  const offEnd = Math.abs(Math.log2(t.end_hz / TILT_HI_HZ));
  const whole = { shape: "whole" as const, hz: typeof t.pivot === "object" ? t.pivot.hz : 1000 };
  if (t.pivot === "start" && offStart > 0.01) return { shape: "above", hz: t.start_hz };
  if (t.pivot === "end" && offEnd > 0.01) return { shape: "below", hz: t.end_hz };
  if (Math.max(offStart, offEnd) < 0.25) return whole;
  return offStart >= offEnd ? { shape: "above", hz: t.start_hz } : { shape: "below", hz: t.end_hz };
}

/** A corner (whole Hz) kept in 20 Hz–20 kHz with at least 1/6 octave of slope on its tilted side. */
export function clampCorner(shape: Exclude<TiltShape, "both">, hz: number): number {
  const v = Number.isFinite(hz) ? hz : 1000;
  // Above: room up to 44.1 kHz's capped end (19.8 kHz).
  if (shape === "above") return Math.max(TILT_LO_HZ, Math.min(Math.floor((0.45 * 44100) / 2 ** MIN_OCT), v));
  if (shape === "below") return Math.max(Math.ceil(TILT_LO_HZ * 2 ** MIN_OCT), Math.min(TILT_HI_HZ, v));
  return Math.max(TILT_LO_HZ, Math.min(TILT_HI_HZ, v));
}

/**
 * `t` set to a shape at corner `hz` (Above/Below), keeping its amount
 * within what the new band allows. Whole band keeps a centre or Hz pivot
 * (else centre).
 */
export function withShape(t: TiltConfig, shape: Exclude<TiltShape, "both">, hz: number): TiltConfig {
  let next: TiltConfig;
  if (shape === "above") next = { ...t, start_hz: Math.round(clampCorner("above", hz)), end_hz: TILT_HI_HZ, pivot: "start" };
  else if (shape === "below") next = { ...t, start_hz: TILT_LO_HZ, end_hz: Math.round(clampCorner("below", hz)), pivot: "end" };
  else {
    const p = t.pivot;
    const pivot: TiltPivot = typeof p === "object" ? { hz: Math.round(clampCorner("whole", p.hz)) } : "center";
    next = { ...t, start_hz: TILT_LO_HZ, end_hz: TILT_HI_HZ, pivot };
  }
  return withinRange(next);
}

/** `t` with its amount kept within what its band allows. */
function withinRange(t: TiltConfig): TiltConfig {
  const next = { ...t };
  const r = tiltRanges(next);
  const clamp = (v: number, m: number) => Math.max(-m, Math.min(m, v));
  if (next.slope_db_per_oct != null) next.slope_db_per_oct = clamp(next.slope_db_per_oct, r.slope);
  if (next.total_db != null) next.total_db = clamp(next.total_db, r.total);
  return next;
}

/** A tilt stage: the plain (or bass) section and the treble section of Both. */
export interface TiltPair {
  tilt: TiltConfig;
  tilt_high: TiltConfig | null;
}

/** Corners of Both (whole Hz): bass `x` below treble `y`, each with room for its slope. */
export function clampBothCorners(x: number, y: number, moved: "bass" | "treble"): { x: number; y: number } {
  let lo = Math.round(clampCorner("below", x));
  let hi = Math.round(clampCorner("above", y));
  if (lo >= hi) {
    if (moved === "bass") lo = hi - 1;
    else hi = lo + 1;
    lo = Math.max(Math.ceil(TILT_LO_HZ * 2 ** MIN_OCT), Math.min(lo, hi - 1));
    hi = Math.max(hi, lo + 1);
  }
  return { x: lo, y: hi };
}

/** Both at corners `x` (bass) and `y` (treble), amounts kept within their bands. */
export function withBoth(low: TiltConfig, high: TiltConfig, x: number, y: number, moved: "bass" | "treble" = "bass"): TiltPair {
  const c = clampBothCorners(x, y, moved);
  return {
    tilt: withinRange({ ...low, start_hz: TILT_LO_HZ, end_hz: c.x, pivot: "end" }),
    tilt_high: withinRange({ ...high, start_hz: c.y, end_hz: TILT_HI_HZ, pivot: "start" }),
  };
}

/**
 * The stage set to `shape`. Into Both, both sections take the current
 * amount (a whole-band tilt with a flat middle); out of it, Above keeps the
 * treble section and the others the bass one.
 */
export function pairWithShape(p: TiltPair, shape: TiltShape): TiltPair {
  const cur = p.tilt_high ? "both" : tiltShape(p.tilt).shape;
  if (shape === "both") {
    if (p.tilt_high) return p;
    const { hz } = tiltShape(p.tilt);
    const [x, y] = cur === "above" ? [Math.min(200, hz / 2), hz] : cur === "below" ? [hz, Math.max(2000, hz * 2)] : [200, 2000];
    return withBoth(p.tilt, p.tilt, x, y, cur === "above" ? "treble" : "bass");
  }
  if (!p.tilt_high) {
    const hz = cur === "whole" ? (typeof p.tilt.pivot === "object" ? p.tilt.pivot.hz : 1000) : tiltShape(p.tilt).hz;
    return { tilt: withShape(p.tilt, shape, hz), tilt_high: null };
  }
  const keep = shape === "above" ? p.tilt_high : p.tilt;
  const hz = shape === "above" ? p.tilt_high.start_hz : shape === "below" ? p.tilt.end_hz : 1000;
  return { tilt: withShape({ ...keep, enabled: p.tilt.enabled || p.tilt_high.enabled }, shape, hz), tilt_high: null };
}

/** The largest `end_hz` config validation accepts (20 kHz; the engine then caps it at 0.45 fs). */
export const TILT_MAX_END_HZ = 20000;

/** A tilt with the engine's serde defaults filled in (enabled, 20 Hz–20 kHz, centre pivot). */
export function normalizeTilt(t: Partial<TiltConfig>): TiltConfig {
  return {
    enabled: t.enabled ?? true,
    slope_db_per_oct: t.slope_db_per_oct ?? null,
    total_db: t.total_db ?? null,
    start_hz: t.start_hz ?? 20,
    end_hz: t.end_hz ?? 20000,
    pivot: t.pivot ?? "center",
  };
}

/**
 * The rate-independent rules config validation applies to every stored
 * tilt, on or off (`validate_eq_tilt` in the daemon), or null.
 */
export function tiltConfigError(t: TiltConfig): string | null {
  if (!Number.isFinite(t.start_hz) || t.start_hz < TILT_MIN_START_HZ || t.start_hz > TILT_MAX_END_HZ) {
    return `tilt start_hz must be between ${TILT_MIN_START_HZ} and ${TILT_MAX_END_HZ} Hz (got ${t.start_hz})`;
  }
  if (!Number.isFinite(t.end_hz) || t.end_hz <= t.start_hz || t.end_hz > TILT_MAX_END_HZ) {
    return `tilt end_hz must be between ${t.start_hz} and ${TILT_MAX_END_HZ} Hz (got ${t.end_hz})`;
  }
  if (Math.log2(t.end_hz / t.start_hz) < 1 / 6) return "tilt: band must span at least 1/6 octave";
  const hasS = t.slope_db_per_oct != null;
  const hasT = t.total_db != null;
  if (hasS === hasT) return "tilt: specify exactly one of slope_db_per_oct or total_db";
  const v = (hasS ? t.slope_db_per_oct : t.total_db) as number;
  if (!Number.isFinite(v)) return "tilt: slope_db_per_oct / total_db must be finite";
  const s = tiltSlope(t);
  const total = tiltTotal(t);
  if (s !== null && Math.abs(s) > TILT_MAX_SLOPE + 1e-9) return `tilt: effective slope ${s.toFixed(3)} dB/oct exceeds ±${TILT_MAX_SLOPE} dB/oct`;
  if (total !== null && Math.abs(total) > TILT_MAX_TOTAL + 1e-9) return `tilt: effective total ${total.toFixed(3)} dB exceeds ±${TILT_MAX_TOTAL} dB`;
  const p = t.pivot;
  if (typeof p === "object" && (!Number.isFinite(p.hz) || p.hz < t.start_hz || p.hz > t.end_hz)) return `tilt pivot_hz must be between ${t.start_hz} and ${t.end_hz} Hz`;
  return null;
}

/** Largest |slope| and |total| the current band allows (both limits hold at once). */
export function tiltRanges(t: TiltConfig): { slope: number; total: number } {
  const oct = Math.log2(t.end_hz / t.start_hz);
  if (!(oct > 0) || !Number.isFinite(oct)) return { slope: TILT_MAX_SLOPE, total: TILT_MAX_TOTAL };
  return { slope: Math.min(TILT_MAX_SLOPE, TILT_MAX_TOTAL / oct), total: Math.min(TILT_MAX_TOTAL, TILT_MAX_SLOPE * oct) };
}

/** The same tilt stated as a slope or as a total, at full precision. */
export function tiltAs(t: TiltConfig, as: "slope" | "total"): TiltConfig {
  if (as === "slope") return t.slope_db_per_oct != null && t.total_db == null ? t : { ...t, total_db: null, slope_db_per_oct: tiltSlope(t) ?? 0 };
  return t.total_db != null && t.slope_db_per_oct == null ? t : { ...t, slope_db_per_oct: null, total_db: tiltTotal(t) ?? 0 };
}
