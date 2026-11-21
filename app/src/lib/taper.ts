// Slider tapers: the volume and balance sliders work in dB, not in travel.
//
// A `Taper` maps a slider position (0..1, left to right) to a dB value and
// back. Both directions are exact inverses, so the value a person sees is the
// value they dragged to, and keyboard/wheel steps move it by whole 0.1 dB.

export interface Taper {
  /** Smallest and largest dB the slider can reach. */
  min: number;
  max: number;
  /** Position 0..1 → dB, clamped. */
  db(pos: number): number;
  /** dB → position 0..1, clamped. */
  pos(db: number): number;
}

const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, v));

/** A taper whose two straight segments meet at (pos, db). */
function twoSegment(min: number, kneeDb: number, kneePos: number, max: number): Taper {
  const loPos = clamp(kneePos, 0, 1);
  const db = (p: number) => {
    const t = clamp(p, 0, 1);
    if (t <= loPos) return loPos === 0 ? min : min + ((kneeDb - min) * t) / loPos;
    return max === kneeDb ? kneeDb : kneeDb + ((max - kneeDb) * (t - loPos)) / (1 - loPos);
  };
  const pos = (d: number) => {
    const v = clamp(d, min, max);
    if (v <= kneeDb) return kneeDb === min ? 0 : (loPos * (v - min)) / (kneeDb - min);
    return max === kneeDb ? 1 : loPos + ((1 - loPos) * (v - kneeDb)) / (max - kneeDb);
  };
  return { min, max, db, pos };
}

/**
 * Volume: -40..ceiling takes about 80% of the travel, the rest reaches the
 * engine's minimum (-80 dB by default, but follow whatever it reports).
 * When the knee lies outside `(min, max)` (e.g. a ceiling at or below -40 dB),
 * fall back to a single linear segment so `min` is 0 and `max` (ceiling) is 1.
 */
export function volumeTaper(minDb: number, maxDb = 0, kneeDb = -40, kneePos = 0.2): Taper {
  const min = Math.min(minDb, maxDb);
  const max = Math.max(minDb, maxDb);
  if (min < kneeDb && kneeDb < max) {
    return twoSegment(min, kneeDb, kneePos, max);
  }
  const db = (p: number) => min + (max - min) * clamp(p, 0, 1);
  const pos = (d: number) => (max === min ? (d >= max ? 1 : 0) : (clamp(d, min, max) - min) / (max - min));
  return { min, max, db, pos };
}

/**
 * Volume slider taper for a level and engine-reported `[minDb, maxDb]` range
 * (same range bounds as TopBar, TrayPopup, our OSD and Plasma's OSD).
 */
export function volumeSliderTaper(
  minDb: number | null | undefined,
  maxDb: number | null | undefined,
  volumeDb?: number | null,
  floorDb = -120,
  ceilDb = 0,
): Taper {
  const baseLo = Math.max(minDb ?? -80, floorDb);
  const lo = volumeDb !== null && volumeDb !== undefined && Number.isFinite(volumeDb)
    ? Math.min(baseLo, Math.max(volumeDb, floorDb))
    : baseLo;
  const hi = Math.min(maxDb ?? ceilDb, ceilDb);
  return volumeTaper(lo, hi);
}

/**
 * Balance: ±3 dB takes 70% of the travel, so the small corrections people
 * actually make are easy to hit, and the outer 15% each side reaches the
 * daemon's ±BALANCE_MAX_DB.
 */
export function balanceTaper(maxDb: number, innerDb = 3, innerFrac = 0.7): Taper {
  const max = Math.abs(maxDb);
  const inner = clamp(Math.abs(innerDb), 0, max);
  const frac = clamp(innerFrac, 0, 1);
  const pos = (d: number) => {
    const v = clamp(d, -max, max);
    const a = Math.abs(v);
    const half = frac / 2;
    const t = a <= inner ? (inner === 0 ? 0 : half * (a / inner)) : half + ((1 - frac) / 2) * ((a - inner) / (max - inner || 1));
    return 0.5 + Math.sign(v) * t;
  };
  const db = (p: number) => {
    const t = clamp(p, 0, 1) - 0.5;
    const a = Math.abs(t);
    const half = frac / 2;
    const v = a <= half ? (half === 0 ? 0 : inner * (a / half)) : inner + (max - inner) * ((a - half) / ((1 - frac) / 2 || 1));
    return Math.sign(t) * Math.min(max, v);
  };
  return { min: -max, max, db, pos };
}

/** Round a dB value to the slider's resolution and keep it in range. */
export function quantiseDb(db: number, taper: Taper, step = 0.1): number {
  const v = Math.round(db / step) * step;
  return clamp(Number(v.toFixed(4)), taper.min, taper.max);
}

/**
 * A keyboard, wheel or typed change from `value` towards `wanted`, quantised
 * and kept in range, but never against its direction: a step down never
 * ends above `value` (the range may start above it, when the daemon reports
 * a narrower range than the level it holds), and a step up never below.
 */
export function stepDb(value: number, wanted: number, taper: Taper, step = 0.1): number {
  const db = quantiseDb(wanted, { ...taper, min: Math.min(taper.min, value), max: Math.max(taper.max, value) }, step);
  if (wanted < value) return Math.min(db, value);
  if (wanted > value) return Math.max(db, value);
  return db;
}

/** Page Up / Page Down step, dB. */
export const PAGE_STEP_DB = 6;

/** The value an arrow, Page, Home or End key moves to (`null`: not a slider
 *  key). Arrows move 0.5 dB, 0.1 dB with Shift; Page Up/Down 6 dB; Home and
 *  End go to the ends. */
export function keyStepDb(key: string, shift: boolean, value: number, taper: Taper): number | null {
  const step = shift ? 0.1 : 0.5;
  if (key === "ArrowRight" || key === "ArrowUp") return stepDb(value, value + step, taper);
  if (key === "ArrowLeft" || key === "ArrowDown") return stepDb(value, value - step, taper);
  if (key === "PageUp") return stepDb(value, value + PAGE_STEP_DB, taper);
  if (key === "PageDown") return stepDb(value, value - PAGE_STEP_DB, taper);
  // Home is a step down even when the slider's minimum is above the level.
  if (key === "Home") return stepDb(value, Math.min(taper.min, value - 1e-9), taper);
  if (key === "End") return stepDb(value, Math.max(taper.max, value + 1e-9), taper);
  return null;
}

/** The value one wheel notch moves to (up = louder), as the arrows.
 *  `null` for a gesture without vertical movement (a horizontal scroll). */
export function wheelStepDb(deltaY: number, shift: boolean, value: number, taper: Taper): number | null {
  if (!(deltaY < 0 || deltaY > 0)) return null;
  return keyStepDb(deltaY < 0 ? "ArrowUp" : "ArrowDown", shift, value, taper);
}
