// The zoomable impulse plot: its frame (x in samples from the peak, y in
// amplitude) and a min/max pyramid so a long impulse draws one column per
// pixel at any zoom without walking every sample each frame.

import { DEFAULT_MIN_SPAN_Y_DB, decimalsFor, type ViewLimits, type ViewRange } from "./plotView";

/** Home view and limits: the whole impulse, x in samples relative to the peak. */
export function impulseFrame(impulse: readonly number[], peak: number): { home: ViewRange; limits: ViewLimits } {
  const n = impulse.length;
  const p = Math.min(Math.max(0, n - 1), Math.max(0, peak));
  let lo = -0.3;
  let hi = 1.05;
  for (const v of impulse) {
    if (v < lo) lo = v * 1.1;
    if (v > hi) hi = v * 1.05;
  }
  const x0 = -p - 0.5;
  const x1 = Math.max(0, n - 1) - p + 0.5;
  const span = hi - lo;
  return {
    home: { x0, x1, y0: lo, y1: hi },
    limits: { x0, x1, y0: lo - span, y1: hi + span, minSpanX: Math.min(8, x1 - x0), minSpanY: DEFAULT_MIN_SPAN_Y_DB },
  };
}

/** Min/max of blocks of 2^k samples, level 0 being the samples themselves. */
export interface Envelope {
  readonly n: number;
  readonly mins: readonly Float64Array[];
  readonly maxs: readonly Float64Array[];
}

export function buildEnvelope(values: readonly number[]): Envelope {
  const n = values.length;
  const base = Float64Array.from(values);
  const mins: Float64Array[] = [base];
  const maxs: Float64Array[] = [base];
  while (mins[mins.length - 1].length > 1) {
    const pm = mins[mins.length - 1];
    const px = maxs[maxs.length - 1];
    const len = Math.ceil(pm.length / 2);
    const m = new Float64Array(len);
    const x = new Float64Array(len);
    for (let i = 0; i < len; i++) {
      const a = 2 * i;
      const b = Math.min(a + 1, pm.length - 1);
      m[i] = Math.min(pm[a], pm[b]);
      x[i] = Math.max(px[a], px[b]);
    }
    mins.push(m);
    maxs.push(x);
  }
  return { n, mins, maxs };
}

/**
 * Min and max of samples [a, b) (clamped to the data; null when empty). Uses
 * the coarsest level whose blocks fit the range, so a block at either end
 * may reach a little past it: under one pixel when a range is one column.
 */
export function rangeMinMax(env: Envelope, a: number, b: number): { min: number; max: number } | null {
  const lo = Math.max(0, Math.floor(a));
  const hi = Math.min(env.n, Math.ceil(b));
  if (hi <= lo) return null;
  const level = Math.min(env.mins.length - 1, Math.max(0, Math.floor(Math.log2(hi - lo)) - 1));
  const bs = 1 << level;
  const mins = env.mins[level];
  const maxs = env.maxs[level];
  let min = Infinity;
  let max = -Infinity;
  for (let k = Math.floor(lo / bs); k <= Math.floor((hi - 1) / bs); k++) {
    if (mins[k] < min) min = mins[k];
    if (maxs[k] > max) max = maxs[k];
  }
  return { min, max };
}

/** Sample offset from the peak: "0", "+12", "−1,024". */
export function fmtSampleOffset(v: number, step = 1): string {
  const d = decimalsFor(step);
  const r = Number(v.toFixed(d));
  const s = Math.abs(r).toLocaleString("en-US", { minimumFractionDigits: d, maximumFractionDigits: d });
  return r === 0 ? s : (r < 0 ? "−" : "+") + s;
}

/** Amplitude with enough decimals for the visible span. */
export function fmtAmplitude(v: number, span: number): string {
  const d = Math.max(2, Math.min(9, decimalsFor(span / 2000)));
  const s = (Math.abs(v) < 0.5 * Math.pow(10, -d) ? 0 : v).toFixed(d);
  return s.replace(/^-/, "−");
}
