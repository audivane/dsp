// Pure helpers for live spectrum display: bin-to-frequency mapping, dBFS
// axis projection, asymmetric attack/release smoothing, peak hold, and
// persisted viewer preferences.

export const SPEC_DB_MIN = -90;
export const SPEC_DB_MAX = 0;
export const SPEC_ATTACK_S = 0.025;
export const SPEC_RELEASE_S = 0.3;
export const SPEC_PEAK_HOLD_MS = 2500;
export const SPEC_PEAK_FALL_DB_PER_S = 10;
export const SPEC_STALE_MS = 400;
export const SPEC_STALE_FADE_MS = 500;

export type SpectrumTap = "post" | "pre" | "both";
/** Fractional-octave smoothing: 1/N octave. */
export type SpectrumSmoothing = 3 | 6 | 12 | 24;
export const SPECTRUM_SMOOTHINGS: readonly SpectrumSmoothing[] = [3, 6, 12, 24];

export interface SpectrumPrefs {
  enabled: boolean;
  tap: SpectrumTap;
  peakHold: boolean;
  smoothing: SpectrumSmoothing;
}

export const DEFAULT_SPECTRUM_PREFS: SpectrumPrefs = {
  enabled: true,
  tap: "post",
  peakHold: false,
  smoothing: 12,
};

const STORAGE_KEY = "avdsp-spectrum";

export function loadSpectrumPrefs(): SpectrumPrefs {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return { ...DEFAULT_SPECTRUM_PREFS };
    const p = JSON.parse(raw) as Partial<SpectrumPrefs>;
    return {
      enabled: typeof p.enabled === "boolean" ? p.enabled : DEFAULT_SPECTRUM_PREFS.enabled,
      tap: p.tap === "pre" || p.tap === "post" || p.tap === "both" ? p.tap : DEFAULT_SPECTRUM_PREFS.tap,
      peakHold: typeof p.peakHold === "boolean" ? p.peakHold : DEFAULT_SPECTRUM_PREFS.peakHold,
      smoothing: SPECTRUM_SMOOTHINGS.includes(p.smoothing as SpectrumSmoothing) ? (p.smoothing as SpectrumSmoothing) : DEFAULT_SPECTRUM_PREFS.smoothing,
    };
  } catch {
    return { ...DEFAULT_SPECTRUM_PREFS };
  }
}

export function saveSpectrumPrefs(prefs: SpectrumPrefs): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(prefs));
  } catch {
    /* storage unavailable */
  }
}

/**
 * 1/`fraction`-octave smoothing of log bins (dB in, dB out): each bin becomes
 * the mean power over a 1/`fraction`-octave window centred on it (bins
 * partly inside count by their overlap). Averaging power keeps broadband
 * levels where they are and can only lower a narrow peak, never raise it.
 */
export function smoothFractionalOctave(bins: readonly number[], binsPerOctave: number, fraction: number, floorDb = SPEC_DB_MIN): number[] {
  const n = bins.length;
  const width = binsPerOctave / Math.max(1, fraction);
  if (n === 0 || width <= 1 + 1e-9) return bins.map((v) => (Number.isFinite(v) ? v : floorDb));
  const half = width / 2;
  const reach = Math.ceil(half + 0.5);
  const weights: number[] = [];
  for (let k = -reach; k <= reach; k++) {
    weights.push(Math.max(0, Math.min(k + 0.5, half) - Math.max(k - 0.5, -half)));
  }
  const pow = bins.map((v) => Math.pow(10, (Number.isFinite(v) ? v : -200) / 10));
  const out: number[] = new Array(n);
  for (let i = 0; i < n; i++) {
    let sum = 0;
    let wsum = 0;
    for (let k = -reach; k <= reach; k++) {
      const j = i + k;
      if (j < 0 || j >= n) continue;
      const w = weights[k + reach];
      sum += pow[j] * w;
      wsum += w;
    }
    out[i] = wsum > 0 && sum > 0 ? 10 * Math.log10(sum / wsum) : floorDb;
  }
  return out;
}

/**
 * Tangents for a monotone cubic (Fritsch-Carlson) through the points: the
 * curve passes through every point and never overshoots between two of them,
 * so smooth drawing cannot invent peaks.
 */
export function monotoneTangents(xs: readonly number[], ys: readonly number[]): number[] {
  const n = Math.min(xs.length, ys.length);
  const m = new Array<number>(n).fill(0);
  if (n < 2) return m;
  const d: number[] = [];
  for (let i = 0; i < n - 1; i++) {
    const dx = xs[i + 1] - xs[i];
    d.push(dx !== 0 ? (ys[i + 1] - ys[i]) / dx : 0);
  }
  m[0] = d[0];
  m[n - 1] = d[n - 2];
  for (let i = 1; i < n - 1; i++) {
    m[i] = d[i - 1] * d[i] <= 0 ? 0 : (d[i - 1] + d[i]) / 2;
  }
  for (let i = 0; i < n - 1; i++) {
    if (d[i] === 0) {
      m[i] = 0;
      m[i + 1] = 0;
      continue;
    }
    const a = m[i] / d[i];
    const b = m[i + 1] / d[i];
    const s = a * a + b * b;
    if (s > 9) {
      const t = 3 / Math.sqrt(s);
      m[i] = t * a * d[i];
      m[i + 1] = t * b * d[i];
    }
  }
  return m;
}

/** Point on the monotone cubic between points `i` and `i + 1` at `t` in 0..1 (for tests). */
export function monotoneAt(xs: readonly number[], ys: readonly number[], m: readonly number[], i: number, t: number): number {
  const h = xs[i + 1] - xs[i];
  const t2 = t * t;
  const t3 = t2 * t;
  return (2 * t3 - 3 * t2 + 1) * ys[i] + (t3 - 2 * t2 + t) * h * m[i] + (-2 * t3 + 3 * t2) * ys[i + 1] + (t3 - t2) * h * m[i + 1];
}

/** Center frequency in Hz for log spectrum bin `i`. */
export function spectrumBinFreq(index: number, fMinHz: number, binsPerOctave: number): number {
  return fMinHz * Math.pow(2, index / binsPerOctave);
}

/** Map a spectrum dBFS value onto the plot's vertical pixel range. */
export function spectrumDbToY(
  db: number,
  top: number,
  plotH: number,
  minDb = SPEC_DB_MIN,
  maxDb = SPEC_DB_MAX,
): number {
  const safe = Number.isFinite(db) ? db : minDb;
  const clamped = Math.max(minDb, Math.min(maxDb, safe));
  const span = Math.max(1e-6, maxDb - minDb);
  return top + ((maxDb - clamped) / span) * plotH;
}

/**
 * Exponential smoothing over `dtSec` with fast attack and ~300 ms release.
 * Returns true while any bin remains active or is still converging.
 */
export function smoothSpectrumBins(
  current: Float64Array,
  target: readonly number[],
  dtSec: number,
  floorDb = SPEC_DB_MIN,
  attackS = SPEC_ATTACK_S,
  releaseS = SPEC_RELEASE_S,
  ceilDb = SPEC_DB_MAX,
): boolean {
  const n = Math.min(current.length, target.length);
  const dt = Math.max(0, dtSec);
  let active = false;
  for (let i = 0; i < n; i++) {
    const raw = target[i];
    const tgt = Math.max(floorDb, Math.min(ceilDb, Number.isFinite(raw) ? raw : floorDb));
    const cur = current[i];
    const tau = tgt > cur ? attackS : releaseS;
    const alpha = dt <= 0 ? 0 : tau <= 0 ? 1 : 1 - Math.exp(-dt / tau);
    const next = cur + alpha * (tgt - cur);
    current[i] = next;
    if (next > floorDb + 0.1 || Math.abs(tgt - next) > 0.05) {
      active = true;
    }
  }
  return active;
}

/**
 * Exponentially decay all bins toward `floorDb` using `releaseS`.
 * Returns true while any bin remains meaningfully above `floorDb`.
 */
export function decaySpectrumBins(
  current: Float64Array,
  dtSec: number,
  floorDb = SPEC_DB_MIN,
  releaseS = SPEC_RELEASE_S,
): boolean {
  const dt = Math.max(0, dtSec);
  const alpha = dt <= 0 ? 0 : releaseS <= 0 ? 1 : 1 - Math.exp(-dt / releaseS);
  let active = false;
  for (let i = 0; i < current.length; i++) {
    const cur = current[i];
    if (cur <= floorDb) {
      current[i] = floorDb;
      continue;
    }
    const next = cur + alpha * (floorDb - cur);
    if (next <= floorDb + 0.05) {
      current[i] = floorDb;
    } else {
      current[i] = next;
      if (next > floorDb + 0.1) active = true;
    }
  }
  return active;
}

/**
 * Per-bin peak hold with hold timer and linear dB/s release.
 * Returns true while any bin remains above the floor.
 */
export function updatePeakHoldBins(
  peaks: Float64Array,
  holdUntil: Float64Array,
  target: readonly number[],
  nowMs: number,
  dtSec: number,
  floorDb = SPEC_DB_MIN,
  holdMs = SPEC_PEAK_HOLD_MS,
  fallDbPerS = SPEC_PEAK_FALL_DB_PER_S,
  ceilDb = SPEC_DB_MAX,
): boolean {
  const n = Math.min(peaks.length, holdUntil.length, target.length);
  const dt = Math.max(0, dtSec);
  let active = false;
  for (let i = 0; i < n; i++) {
    const raw = target[i];
    const tgt = Math.max(floorDb, Math.min(ceilDb, Number.isFinite(raw) ? raw : floorDb));
    if (tgt >= peaks[i]) {
      peaks[i] = tgt;
      holdUntil[i] = nowMs + holdMs;
    } else if (nowMs >= holdUntil[i]) {
      peaks[i] = Math.max(tgt, Math.max(floorDb, peaks[i] - fallDbPerS * dt));
    }
    if (peaks[i] > floorDb + 0.1) active = true;
  }
  return active;
}

export interface SpectrumAnimStep {
  stale: boolean;
  animating: boolean;
  peakAlpha: number;
  clearFrame: boolean;
}

/**
 * Advance smoothed pre/post bins and peak hold for one animation frame.
 * When no frame arrives for more than `staleMs`, decays all active curves to
 * `floorDb`, fades the peak-hold trace over `fadeMs`, and requests a clear once settled.
 */
export function stepSpectrumAnimation(opts: {
  frameAgeMs: number;
  newFrame: boolean;
  tap: SpectrumTap;
  peakHold: boolean;
  preTarget: readonly number[];
  postTarget: readonly number[];
  smoothPre: Float64Array;
  smoothPost: Float64Array;
  peakVals: Float64Array;
  peakUntil: Float64Array;
  nowMs: number;
  dtSec: number;
  floorDb?: number;
  /** The highest level a target is taken at (default 0 dBFS). */
  ceilDb?: number;
  staleMs?: number;
  fadeMs?: number;
}): SpectrumAnimStep {
  const floorDb = opts.floorDb ?? SPEC_DB_MIN;
  const ceilDb = opts.ceilDb ?? SPEC_DB_MAX;
  const staleMs = opts.staleMs ?? SPEC_STALE_MS;
  const fadeMs = opts.fadeMs ?? SPEC_STALE_FADE_MS;
  const stale = opts.frameAgeMs > staleMs;
  const usePre = (opts.tap === "pre" || opts.tap === "both") && opts.preTarget.length >= 2;
  const usePost = (opts.tap === "post" || opts.tap === "both") && opts.postTarget.length >= 2;
  const primaryTarget = opts.tap === "pre" ? opts.preTarget : opts.postTarget;
  const dt = Math.max(0, opts.dtSec);

  if (stale) {
    const staleOverMs = opts.frameAgeMs - staleMs;
    const peakAlpha = opts.peakHold ? Math.max(0, 1 - staleOverMs / Math.max(1, fadeMs)) : 0;
    const preMoving = usePre ? decaySpectrumBins(opts.smoothPre, dt, floorDb) : false;
    const postMoving = usePost ? decaySpectrumBins(opts.smoothPost, dt, floorDb) : false;
    let peakMoving = false;
    if (opts.peakHold && opts.peakVals.length >= 2) {
      for (let i = 0; i < opts.peakVals.length; i++) {
        if (opts.peakVals[i] > floorDb) {
          opts.peakVals[i] = Math.max(floorDb, opts.peakVals[i] - SPEC_PEAK_FALL_DB_PER_S * dt);
          if (opts.peakVals[i] > floorDb + 0.1) peakMoving = true;
        }
      }
      if (peakAlpha > 0) peakMoving = true;
    }
    const stillDecaying = preMoving || postMoving || peakMoving;
    return {
      stale: true,
      animating: stillDecaying,
      peakAlpha,
      clearFrame: !stillDecaying,
    };
  }

  let moving = opts.newFrame;
  if (usePre) {
    if (smoothSpectrumBins(opts.smoothPre, opts.preTarget, dt, floorDb, SPEC_ATTACK_S, SPEC_RELEASE_S, ceilDb)) moving = true;
  }
  if (usePost) {
    if (smoothSpectrumBins(opts.smoothPost, opts.postTarget, dt, floorDb, SPEC_ATTACK_S, SPEC_RELEASE_S, ceilDb)) moving = true;
  }
  if (opts.peakHold && primaryTarget.length >= 2) {
    if (updatePeakHoldBins(opts.peakVals, opts.peakUntil, primaryTarget, opts.nowMs, dt, floorDb, SPEC_PEAK_HOLD_MS, SPEC_PEAK_FALL_DB_PER_S, ceilDb)) moving = true;
  }
  // Keep animating while frame is within the stale threshold so stale decay starts promptly at > staleMs.
  return {
    stale: false,
    animating: true || moving,
    peakAlpha: 1,
    clearFrame: false,
  };
}

/**
 * One analyser's animation state: the displayed (ballistic) levels of its
 * two traces and the peak hold, stepped once per drawn frame toward the
 * latest targets. The PEQ analyser and the Harmonics page's live spectrum
 * each keep one.
 */
export class SpectrumAnimator {
  pre = new Float64Array(0);
  post = new Float64Array(0);
  peak = new Float64Array(0);
  peakUntil = new Float64Array(0);
  private lastTickMs = 0;
  private lastKey: unknown = undefined;

  reset(): void {
    this.pre = new Float64Array(0);
    this.post = new Float64Array(0);
    this.peak = new Float64Array(0);
    this.peakUntil = new Float64Array(0);
    this.lastTickMs = 0;
    this.lastKey = undefined;
  }

  /** True once a step has run since the last reset (the buffers hold levels). */
  get started(): boolean {
    return this.lastKey !== undefined;
  }

  /**
   * Advance to `nowMs`. `frameKey` changes with each new frame (its targets),
   * `frameAtMs` is when that frame arrived. New buffers start at the floor.
   */
  step(opts: {
    frameKey: unknown;
    frameAtMs: number;
    nowMs: number;
    tap: SpectrumTap;
    peakHold: boolean;
    preTarget: readonly number[];
    postTarget: readonly number[];
    floorDb?: number;
    ceilDb?: number;
    staleMs?: number;
  }): SpectrumAnimStep {
    const floor = opts.floorDb ?? SPEC_DB_MIN;
    const peakLen = opts.tap === "pre" ? opts.preTarget.length : opts.postTarget.length;
    if (this.pre.length !== opts.preTarget.length) this.pre = new Float64Array(opts.preTarget.length).fill(floor);
    if (this.post.length !== opts.postTarget.length) this.post = new Float64Array(opts.postTarget.length).fill(floor);
    if (this.peak.length !== peakLen) {
      this.peak = new Float64Array(peakLen).fill(floor);
      this.peakUntil = new Float64Array(peakLen).fill(0);
    }
    const dt = this.lastTickMs > 0 ? Math.min(0.1, Math.max(0, (opts.nowMs - this.lastTickMs) / 1000)) : 1 / 60;
    this.lastTickMs = opts.nowMs;
    const newFrame = opts.frameKey !== this.lastKey;
    this.lastKey = opts.frameKey;
    return stepSpectrumAnimation({
      frameAgeMs: Math.max(0, opts.nowMs - opts.frameAtMs),
      newFrame,
      tap: opts.tap,
      peakHold: opts.peakHold,
      preTarget: opts.preTarget,
      postTarget: opts.postTarget,
      smoothPre: this.pre,
      smoothPost: this.post,
      peakVals: this.peak,
      peakUntil: this.peakUntil,
      nowMs: opts.nowMs,
      dtSec: dt,
      floorDb: opts.floorDb,
      ceilDb: opts.ceilDb,
      staleMs: opts.staleMs,
    });
  }
}

/**
 * The points of a log-binned trace on a plot: bin `i` at its centre frequency,
 * up to one bin past `xEnd` (where a clip cuts it). The last bin stops short of
 * the tap's Nyquist (whole bins and a guard); with `topHz` its level carries on
 * to it, so the trace meets an "above Nyquist" shading, only across a gap under
 * two bins.
 */
export function spectrumCurvePoints(
  bins: ArrayLike<number>,
  fMinHz: number,
  binsPerOctave: number,
  xOf: (f: number) => number,
  yOf: (db: number) => number,
  xEnd: number,
  topHz?: number,
): { xs: number[]; ys: number[] } {
  const xs: number[] = [];
  const ys: number[] = [];
  for (let i = 0; i < bins.length; i++) {
    const x = xOf(spectrumBinFreq(i, fMinHz, binsPerOctave));
    xs.push(x);
    ys.push(yOf(bins[i]));
    if (x > xEnd) break;
  }
  const last = bins.length - 1;
  if (topHz !== undefined && xs.length === bins.length && last >= 0) {
    const fLast = spectrumBinFreq(last, fMinHz, binsPerOctave);
    if (topHz > fLast && topHz / fLast < Math.pow(2, 2 / binsPerOctave)) {
      xs.push(xOf(topHz));
      ys.push(ys[ys.length - 1]);
    }
  }
  return { xs, ys };
}

/**
 * A smooth curve through the points onto `ctx`'s path (a monotone cubic: no
 * overshoot, so the drawing never shows a peak the data does not have).
 * Starts with a moveTo unless `join`, which continues the path with a lineTo.
 */
export function traceMonotone(ctx: Pick<CanvasRenderingContext2D, "moveTo" | "lineTo" | "bezierCurveTo">, xs: readonly number[], ys: readonly number[], join = false): void {
  const n = Math.min(xs.length, ys.length);
  if (n === 0) return;
  const m = monotoneTangents(xs, ys);
  if (join) ctx.lineTo(xs[0], ys[0]);
  else ctx.moveTo(xs[0], ys[0]);
  for (let i = 0; i < n - 1; i++) {
    const h = (xs[i + 1] - xs[i]) / 3;
    ctx.bezierCurveTo(xs[i] + h, ys[i] + m[i] * h, xs[i + 1] - h, ys[i + 1] - m[i + 1] * h, xs[i + 1], ys[i + 1]);
  }
}

/** The same curve as an SVG path's commands ("M… C…", or "L… C…" with `join`). */
export function monotonePathD(xs: readonly number[], ys: readonly number[], join = false): string {
  const n = Math.min(xs.length, ys.length);
  if (n === 0) return "";
  const m = monotoneTangents(xs, ys);
  const f = (v: number) => v.toFixed(1);
  let d = `${join ? "L" : "M"}${f(xs[0])},${f(ys[0])}`;
  for (let i = 0; i < n - 1; i++) {
    const h = (xs[i + 1] - xs[i]) / 3;
    d += `C${f(xs[i] + h)},${f(ys[i] + m[i] * h)} ${f(xs[i + 1] - h)},${f(ys[i + 1] - m[i + 1] * h)} ${f(xs[i + 1])},${f(ys[i + 1])}`;
  }
  return d;
}
