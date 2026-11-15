// Pure view-range maths for the zoomable response plots: zoom around a
// point, pan, box zoom, limits, ticks and precision-aware labels.

export type XScale = "lin" | "log";

/** Visible data range: x in Hz, y in dB. */
export interface ViewRange {
  x0: number;
  x1: number;
  y0: number;
  y1: number;
}

/** Hard limits plus the smallest spans a view may shrink to. */
export interface ViewLimits {
  x0: number;
  x1: number;
  y0: number;
  y1: number;
  /** Narrowest x span in Hz (lin) or as a ratio x1/x0 (log). */
  minSpanX: number;
  minSpanY: number;
}

export const DEFAULT_MIN_SPAN_X_HZ = 0.002;
export const DEFAULT_MIN_SPAN_Y_DB = 0.00002;

const toAxis = (v: number, scale: XScale) => (scale === "log" ? Math.log10(Math.max(1e-9, v)) : v);
const fromAxis = (v: number, scale: XScale) => (scale === "log" ? Math.pow(10, v) : v);

/** Fraction 0..1 of `v` across [lo, hi] on the given scale. */
export function axisFrac(v: number, lo: number, hi: number, scale: XScale = "lin"): number {
  const a = toAxis(lo, scale);
  const b = toAxis(hi, scale);
  return (toAxis(v, scale) - a) / Math.max(1e-300, b - a);
}

/** Inverse of {@link axisFrac}. */
export function axisValue(frac: number, lo: number, hi: number, scale: XScale = "lin"): number {
  const a = toAxis(lo, scale);
  const b = toAxis(hi, scale);
  return fromAxis(a + frac * (b - a), scale);
}

function clampSpan(lo: number, hi: number, minLo: number, maxHi: number, minSpan: number): [number, number] {
  let span = Math.max(minSpan, hi - lo);
  const full = maxHi - minLo;
  if (span >= full) return [minLo, maxHi];
  let a = lo;
  if (hi - lo < minSpan) a = (lo + hi) / 2 - span / 2;
  if (a < minLo) a = minLo;
  if (a + span > maxHi) a = maxHi - span;
  span = Math.min(span, full);
  return [a, a + span];
}

/** Keep a view inside the limits and at least the minimum spans. */
export function clampView(v: ViewRange, lim: ViewLimits, scale: XScale = "lin"): ViewRange {
  let x0: number;
  let x1: number;
  if (scale === "log") {
    const [a, b] = clampSpan(
      toAxis(Math.min(v.x0, v.x1), "log"),
      toAxis(Math.max(v.x0, v.x1), "log"),
      toAxis(lim.x0, "log"),
      toAxis(lim.x1, "log"),
      Math.log10(Math.max(1.000001, lim.minSpanX)),
    );
    x0 = Math.pow(10, a);
    x1 = Math.pow(10, b);
  } else {
    [x0, x1] = clampSpan(Math.min(v.x0, v.x1), Math.max(v.x0, v.x1), lim.x0, lim.x1, lim.minSpanX);
  }
  const [y0, y1] = clampSpan(Math.min(v.y0, v.y1), Math.max(v.y0, v.y1), lim.y0, lim.y1, lim.minSpanY);
  return { x0, x1, y0, y1 };
}

/**
 * Zoom by `factor` (< 1 zooms in) around the point at fractions (`fx`, `fy`)
 * of the plot (0 = left/bottom). `axis` picks which axes move.
 */
export function zoomAt(
  v: ViewRange,
  factor: number,
  fx: number,
  fy: number,
  axis: "x" | "y" | "xy",
  lim: ViewLimits,
  scale: XScale = "lin",
): ViewRange {
  let { x0, x1, y0, y1 } = v;
  if (axis !== "y") {
    const a = toAxis(x0, scale);
    const b = toAxis(x1, scale);
    const c = a + fx * (b - a);
    x0 = fromAxis(c - (c - a) * factor, scale);
    x1 = fromAxis(c + (b - c) * factor, scale);
  }
  if (axis !== "x") {
    const c = y0 + fy * (y1 - y0);
    y0 = c - (c - y0) * factor;
    y1 = c + (y1 - c) * factor;
  }
  return clampView({ x0, x1, y0, y1 }, lim, scale);
}

/** Pan by fractions of the plot size (positive dx moves the data right). */
export function panBy(v: ViewRange, dxFrac: number, dyFrac: number, lim: ViewLimits, scale: XScale = "lin"): ViewRange {
  const a = toAxis(v.x0, scale);
  const b = toAxis(v.x1, scale);
  const shiftX = -dxFrac * (b - a);
  const shiftY = -dyFrac * (v.y1 - v.y0);
  // Pan keeps the span: clamp the offset, not the ends.
  const la = toAxis(lim.x0, scale);
  const lb = toAxis(lim.x1, scale);
  const sx = Math.max(la - a, Math.min(lb - b, shiftX));
  const sy = Math.max(lim.y0 - v.y0, Math.min(lim.y1 - v.y1, shiftY));
  return clampView(
    { x0: fromAxis(a + sx, scale), x1: fromAxis(b + sx, scale), y0: v.y0 + sy, y1: v.y1 + sy },
    lim,
    scale,
  );
}

/** Zoom to a dragged box (data units); `axis` "x" keeps the y range. */
export function boxZoom(v: ViewRange, box: ViewRange, axis: "x" | "xy", lim: ViewLimits, scale: XScale = "lin"): ViewRange {
  return clampView(
    {
      x0: Math.min(box.x0, box.x1),
      x1: Math.max(box.x0, box.x1),
      y0: axis === "xy" ? Math.min(box.y0, box.y1) : v.y0,
      y1: axis === "xy" ? Math.max(box.y0, box.y1) : v.y1,
    },
    lim,
    scale,
  );
}

export function sameView(a: ViewRange, b: ViewRange): boolean {
  const eq = (p: number, q: number) => Math.abs(p - q) <= 1e-9 * Math.max(1, Math.abs(p), Math.abs(q));
  return eq(a.x0, b.x0) && eq(a.x1, b.x1) && eq(a.y0, b.y0) && eq(a.y1, b.y1);
}

/** About `target` evenly spaced round ticks (1, 2, 5 x 10^n) inside [lo, hi]. */
export function linearTicks(lo: number, hi: number, target = 6): { ticks: number[]; step: number } {
  const span = Math.abs(hi - lo);
  if (!(span > 0) || !Number.isFinite(span)) return { ticks: [lo], step: 1 };
  const raw = span / Math.max(1, target);
  const mag = Math.pow(10, Math.floor(Math.log10(raw)));
  const norm = raw / mag;
  const step = (norm < 1.5 ? 1 : norm < 3.5 ? 2 : norm < 7.5 ? 5 : 10) * mag;
  const a = Math.min(lo, hi);
  const b = Math.max(lo, hi);
  const ticks: number[] = [];
  const maxTicks = Math.max(200, Math.round(target) * 3);
  for (let k = Math.ceil(a / step - 1e-9); k * step <= b + step * 1e-9; k++) {
    ticks.push(Number((k * step).toPrecision(12)));
    if (ticks.length >= maxTicks) break;
  }
  return { ticks, step };
}

const LOG_STEPS = [1, 2, 5];

/** Decade ticks (1, 2, 5 x 10^n) inside [lo, hi]; falls back to linear ticks for narrow ranges. */
export function logTicks(lo: number, hi: number, target = 8): number[] {
  if (hi / lo < 4) return linearTicks(lo, hi, Math.min(target, 6)).ticks;
  const out: number[] = [];
  for (let e = Math.floor(Math.log10(lo)); e <= Math.ceil(Math.log10(hi)); e++) {
    for (const m of LOG_STEPS) {
      const f = m * Math.pow(10, e);
      if (f >= lo * 0.9999 && f <= hi * 1.0001) out.push(f);
    }
  }
  return out;
}

/** Decimals needed to tell ticks `step` apart. */
export function decimalsFor(step: number): number {
  if (!(step > 0) || !Number.isFinite(step)) return 0;
  return Math.max(0, Math.min(9, Math.ceil(-Math.log10(step) - 1e-9)));
}

function group(n: string): string {
  const [int, frac] = n.split(".");
  const neg = int.startsWith("-");
  const digits = neg ? int.slice(1) : int;
  const g = digits.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  return (neg ? "−" : "") + g + (frac !== undefined ? "." + frac : "");
}

/** Frequency label with as many decimals as the visible span needs. */
export function fmtHzPrecise(hz: number, spanHz: number): string {
  if (!Number.isFinite(hz)) return "—";
  if (spanHz >= 2000 && hz >= 1000) {
    const k = hz / 1000;
    const d = spanHz >= 20000 ? 0 : spanHz >= 5000 ? 1 : 2;
    return `${Number(k.toFixed(d)).toLocaleString("en-US", { maximumFractionDigits: d })}k`;
  }
  const d = decimalsFor(spanHz / 200);
  return group(hz.toFixed(d));
}

/** dB value with enough decimals for the visible span (down to 0.00001 dB). */
export function fmtDbPrecise(db: number, spanDb: number): string {
  if (!Number.isFinite(db)) return db < 0 ? "−∞" : "∞";
  const d = Math.max(1, Math.min(6, decimalsFor(spanDb / 2000)));
  const v = Math.abs(db) < 0.5 * Math.pow(10, -d) ? 0 : db;
  return group(v.toFixed(d));
}

/** Linear interpolation of a sorted (x, y) series at `x` (null outside). */
export function interpAt(xs: readonly number[], ys: readonly number[], x: number): number | null {
  const n = Math.min(xs.length, ys.length);
  if (n === 0 || x < xs[0] || x > xs[n - 1]) return null;
  let lo = 0;
  let hi = n - 1;
  while (hi - lo > 1) {
    const mid = (lo + hi) >> 1;
    if (xs[mid] <= x) lo = mid;
    else hi = mid;
  }
  if (xs[hi] === xs[lo]) return ys[lo];
  const t = (x - xs[lo]) / (xs[hi] - xs[lo]);
  return ys[lo] + t * (ys[hi] - ys[lo]);
}

/**
 * Base series with a denser window spliced in: base points inside
 * [winLo, winHi] are replaced by the window's points.
 */
export function spliceWindow(
  freqs: readonly number[],
  mags: readonly number[],
  win: { f_lo_hz: number; f_hi_hz: number; freqs_hz: readonly number[]; magnitude_db: readonly number[] } | null | undefined,
): { freqs: number[]; mags: number[] } {
  if (!win || win.freqs_hz.length < 2) return { freqs: [...freqs], mags: [...mags] };
  const outF: number[] = [];
  const outM: number[] = [];
  let i = 0;
  const n = Math.min(freqs.length, mags.length);
  while (i < n && freqs[i] < win.f_lo_hz) {
    outF.push(freqs[i]);
    outM.push(mags[i]);
    i++;
  }
  for (let k = 0; k < win.freqs_hz.length; k++) {
    outF.push(win.freqs_hz[k]);
    outM.push(win.magnitude_db[k]);
  }
  while (i < n && freqs[i] <= win.f_hi_hz) i++;
  for (; i < n; i++) {
    outF.push(freqs[i]);
    outM.push(mags[i]);
  }
  return { freqs: outF, mags: outM };
}

/** View that centres a threshold crossing with a span that shows its neighbourhood. */
export function viewForCrossing(
  crossings: readonly { level_db: number; freq_hz: number }[],
  index: number,
  lim: ViewLimits,
): ViewRange {
  const c = crossings[index];
  const prev = crossings[index - 1];
  const next = crossings[index + 1];
  const gaps = [prev ? c.freq_hz - prev.freq_hz : Infinity, next ? next.freq_hz - c.freq_hz : Infinity].filter((g) => Number.isFinite(g) && g > 0);
  const near = gaps.length ? Math.min(...gaps) : 1;
  const halfX = Math.max(0.01, near * 2);
  const level = c.level_db;
  const a = Math.abs(level);
  const halfY = a >= 1 ? Math.max(6, a * 0.25) : Math.max(0.0002, a * 1.5);
  return clampView({ x0: c.freq_hz - halfX, x1: c.freq_hz + halfX, y0: level - halfY, y1: Math.min(lim.y1, level + halfY) }, lim, "lin");
}

/** "−0.001 → −150 dB in 23.4 Hz": the span from the flattest to the deepest reported level. */
export function steepnessText(crossings: readonly { level_db: number; freq_hz: number }[]): string | null {
  const from = crossings.find((c) => c.level_db === -0.001) ?? crossings[0];
  const to = crossings[crossings.length - 1];
  if (!from || !to || to === from || to.freq_hz <= from.freq_hz) return null;
  const width = to.freq_hz - from.freq_hz;
  const w = width >= 1000 ? `${(width / 1000).toFixed(2)} kHz` : width >= 10 ? `${width.toFixed(1)} Hz` : `${width.toFixed(2)} Hz`;
  const lvl = (db: number) => (db === 0 ? "0" : "−" + String(Math.abs(db)));
  return `${lvl(from.level_db)} → ${lvl(to.level_db)} dB in ${w}`;
}
