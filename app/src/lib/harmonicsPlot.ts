// Y axis of the Harmonics readout plots: harmonic level relative to the
// fundamental, shown in dB or as a log percentage (as Audio Precision plots
// THD+N). The two are the same axis: % = 100 · 10^(dB / 20).

export type HarmUnit = "db" | "pct";

/** Top of both plots: the fundamental's own level. */
export const HARM_Y_TOP_DB = 0;
/** Floor in dB mode (as before), and in % mode 0.0001 % (−120 dB). */
export const HARM_Y_FLOOR_DB: Record<HarmUnit, number> = { db: -140, pct: -120 };

export const HARM_UNIT_KEY = "avdsp-harm-unit";
export const HARM_PLOT_HEIGHT_KEY = "avdsp-height-harm-plot";
export const HARM_SPLIT_KEY = "avdsp-width-harm-editor";
export const HARM_PLOT_DEFAULT_HEIGHT = 340;
export const HARM_PLOT_MIN_HEIGHT = 160;
export const HARM_PLOT_MAX_HEIGHT = 1200;

export const dbToPct = (db: number): number => 100 * Math.pow(10, db / 20);

/** "0.738 %", "0.0012 %": enough digits for small values without exponents. */
export function fmtPctValue(db: number): string {
  const p = dbToPct(db);
  if (p >= 1) return `${p.toFixed(2)} %`;
  if (p >= 0.01) return `${p.toFixed(3)} %`;
  const digits = Math.min(8, Math.max(0, 1 - Math.floor(Math.log10(p))));
  return `${p.toFixed(digits)} %`;
}

export interface YTick {
  db: number;
  /** Null: a minor grid line without a label. */
  label: string | null;
  major: boolean;
}

/** "100", "0.5", "0.0001" (no exponent, no trailing zeros). */
function pctLabel(p: number): string {
  if (p >= 1) return String(Math.round(p));
  const digits = Math.max(0, -Math.floor(Math.log10(p) + 1e-9));
  return p.toFixed(digits);
}

/**
 * Y ticks for a plot area `heightPx` tall. dB: every 20 dB, or every 10 dB
 * when there is room. %: a label per decade, 1-2-5 grid lines in between
 * (labelled too when there is room, as AP does).
 */
export function harmYTicks(unit: HarmUnit, heightPx: number): YTick[] {
  const top = HARM_Y_TOP_DB;
  const floor = HARM_Y_FLOOR_DB[unit];
  const span = top - floor;
  const out: YTick[] = [];
  if (unit === "db") {
    const step = (heightPx * 10) / span >= 22 ? 10 : 20;
    for (let v = top; v >= floor; v -= step) out.push({ db: v, label: String(v), major: v % 20 === 0 });
    return out;
  }
  // Pixels per decade of %: 20 dB.
  const perDecade = (heightPx * 20) / span;
  const labelMinor = perDecade >= 90;
  for (let e = 2; e >= floor / 20 + 2; e--) {
    for (const m of [1, 0.5, 0.2]) {
      const p = m * Math.pow(10, e);
      const db = 20 * Math.log10(p / 100);
      if (db > top + 1e-9 || db < floor - 1e-9) continue;
      const major = m === 1;
      out.push({ db, label: major || labelMinor ? pctLabel(p) : null, major });
    }
  }
  return out;
}

/** Map a level (dB) to y within [top, bottom] px, clamped to the axis. */
export function harmY(unit: HarmUnit, db: number, topPx: number, bottomPx: number): number {
  const floor = HARM_Y_FLOOR_DB[unit];
  const v = Math.max(floor, Math.min(HARM_Y_TOP_DB, db));
  return topPx + ((HARM_Y_TOP_DB - v) / (HARM_Y_TOP_DB - floor)) * (bottomPx - topPx);
}

export function loadHarmUnit(): HarmUnit {
  try {
    return localStorage.getItem(HARM_UNIT_KEY) === "pct" ? "pct" : "db";
  } catch {
    return "db";
  }
}

export function saveHarmUnit(u: HarmUnit): void {
  try {
    localStorage.setItem(HARM_UNIT_KEY, u);
  } catch {
    /* storage unavailable */
  }
}

/** A percentage without exponent or trailing zeros, three significant digits: "100", "0.5", "0.00001", "0.0123". */
export function fmtPctNum(p: number): string {
  if (!(p > 0) || !Number.isFinite(p)) return "0";
  const digits = Math.min(16, Math.max(0, 2 - Math.floor(Math.log10(p) + 1e-9)));
  const s = p.toFixed(digits);
  return s.includes(".") ? s.replace(/0+$/, "").replace(/\.$/, "") : s;
}

/** A level (dB re the fundamental) as a % tick label: "100", "0.001". */
export const pctTick = (db: number): string => fmtPctNum(dbToPct(db));

/** A typed % limit ("0.001", "0.001 %") as dB; null if not a positive number. */
export function parsePctLimit(text: string): number | null {
  const m = text.trim().replace(/\s*%$/, "").match(/^\+?(\d*\.?\d+(?:e[-+]?\d+)?)$/i);
  if (!m) return null;
  const p = Number(m[1]);
  return p > 0 && Number.isFinite(p) ? 20 * Math.log10(p / 100) : null;
}

/**
 * Ticks of a log-% axis over [y0, y1] dB, at dB positions: a tick per
 * decade of % (every 20 dB), with 2 and 5 between when there is room, every
 * other decade when crowded; a narrow span (under about one decade) falls
 * back to round % values.
 */
export function pctAxisTicks(y0: number, y1: number, target: number): { ticks: number[]; step: number } {
  const span = y1 - y0;
  if (!(span > 0)) return { ticks: [y0], step: 20 };
  const perDecade = (target * 20) / span;
  const toDb = (p: number) => 20 * Math.log10(p / 100);
  if (span < 24) {
    // Round % values (1-2-5 steps in % space) inside the span.
    const p0 = dbToPct(y0);
    const p1 = dbToPct(y1);
    const raw = (p1 - p0) / Math.max(1, target);
    const mag = Math.pow(10, Math.floor(Math.log10(raw)));
    const n = raw / mag;
    const step = (n < 1.5 ? 1 : n < 3.5 ? 2 : n < 7.5 ? 5 : 10) * mag;
    const ticks: number[] = [];
    for (let k = Math.ceil(p0 / step - 1e-9); k * step <= p1 * (1 + 1e-9) && ticks.length < 60; k++) if (k > 0) ticks.push(toDb(k * step));
    return { ticks, step: 20 };
  }
  const every = perDecade >= 0.5 ? 1 : Math.ceil(0.5 / perDecade);
  const mults = perDecade >= 3 ? [1, 2, 5] : [1];
  const ticks: number[] = [];
  for (let e = Math.floor((y0 / 20) + 2) - 1; e <= Math.ceil(y1 / 20 + 2) + 1; e++) {
    if (((e % every) + every) % every !== 0) continue;
    for (const m of mults) {
      const db = toDb(m * Math.pow(10, e));
      if (db >= y0 - 1e-9 && db <= y1 + 1e-9) ticks.push(db);
    }
  }
  ticks.sort((a, b) => a - b);
  return { ticks, step: 20 * every };
}

/** Left padding a % axis needs for its widest label (at the bottom), at 11 px mono. */
export const pctAxisPad = (y0: number, min: number): number => Math.max(min, 16 + 6.7 * fmtPctNum(dbToPct(y0)).length);
