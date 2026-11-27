import type { CutoffMode } from "../api/generated/CutoffMode";
// Pure axis-mapping and data-slicing helpers for the Upsampling response plots.

export const ZOOM_MIN_FACTOR = 0.73;
export const ZOOM_MAX_FACTOR = 1.18;
export const UPS_Y_MIN_DB = -150;
export const UPS_Y_MAX_DB = 0;

export function zoomFreqRange(sourceRate: number): { nyquistIn: number; fMin: number; fMax: number } {
  const nyquistIn = Math.max(1, sourceRate / 2);
  return {
    nyquistIn,
    fMin: ZOOM_MIN_FACTOR * nyquistIn,
    fMax: ZOOM_MAX_FACTOR * nyquistIn,
  };
}

/** Linear X mapping for the Nyquist zoom plot. */
export function zoomXOf(f: number, fMin: number, fMax: number, left: number, plotW: number): number {
  const span = Math.max(1e-6, fMax - fMin);
  return left + ((f - fMin) / span) * plotW;
}

/** Log X mapping for the full-range magnitude plot. */
export function logXOf(f: number, fMin: number, fMax: number, left: number, plotW: number): number {
  const lo = Math.max(1, fMin);
  const hi = Math.max(lo * 1.01, fMax);
  const ff = Math.max(lo, Math.min(hi, f));
  return left + (Math.log10(ff / lo) / Math.log10(hi / lo)) * plotW;
}

/** Linear dB Y mapping for 0..-150 dB (with optional small headroom above 0 dB). */
export function responseYOf(
  db: number,
  top: number,
  plotH: number,
  minDb = UPS_Y_MIN_DB,
  maxDb = UPS_Y_MAX_DB,
): number {
  const safe = Number.isFinite(db) ? db : minDb;
  const clamped = Math.max(minDb, Math.min(maxDb + 4, safe));
  const span = Math.max(1e-6, maxDb - minDb);
  return top + ((maxDb - clamped) / span) * plotH;
}

/** Choose clean round kHz tick frequencies inside [fMin, fMax]. */
export function zoomTicks(fMin: number, fMax: number): number[] {
  const span = fMax - fMin;
  const step = span <= 12000 ? 2000 : span <= 24000 ? 4000 : 5000;
  const start = Math.ceil(fMin / step) * step;
  const ticks: number[] = [];
  for (let f = start; f <= fMax + 1e-6; f += step) {
    ticks.push(f);
  }
  return ticks;
}

/** Slice a sorted frequency/magnitude pair to [fMin, fMax], keeping 1 neighbour on each side for edge clipping. */
export function sliceFreqRange(
  freqs: readonly number[],
  mags: readonly number[],
  fMin: number,
  fMax: number,
): { freqs: number[]; mags: number[] } {
  const n = Math.min(freqs.length, mags.length);
  if (n === 0) return { freqs: [], mags: [] };
  let start = 0;
  while (start < n - 1 && freqs[start + 1] < fMin) start++;
  let end = n - 1;
  while (end > 0 && freqs[end - 1] > fMax) end--;
  if (end < start) return { freqs: [], mags: [] };
  return {
    freqs: freqs.slice(start, end + 1),
    mags: mags.slice(start, end + 1),
  };
}

/** Most points the UI asks for per zoom window (the daemon allows 4096). */
export const WINDOW_POINTS = 1200;

/**
 * The window to fetch for a zoomed view, or null when the base points are
 * dense enough (a log view spanning more than ~1.5 decades).
 */
export function windowRequestFor(
  view: { x0: number; x1: number },
  scale: "lin" | "log",
  nyquistOut: number,
  points = WINDOW_POINTS,
): { f_lo_hz: number; f_hi_hz: number; points: number } | null {
  const lo = Math.max(0, Math.min(view.x0, view.x1));
  const hi = Math.min(nyquistOut, Math.max(view.x0, view.x1));
  if (!(hi > lo) || !Number.isFinite(lo) || !Number.isFinite(hi)) return null;
  if (scale === "log" && hi / Math.max(1e-9, lo) > 30) return null;
  return { f_lo_hz: lo, f_hi_hz: hi, points };
}

/** "23,999.937" with a fixed number of decimals and thousands separators. */
export function fmtHzStat(hz: number, digits: number): string {
  if (!Number.isFinite(hz)) return "-";
  return hz.toLocaleString("en-US", { minimumFractionDigits: digits, maximumFractionDigits: digits });
}

/** Passband ripple with enough decimals to show it (down to 0.000001 dB). */
export function fmtRipple(db: number): string {
  if (!Number.isFinite(db) || db <= 0) return "0 dB";
  if (db >= 0.01) return `${db.toFixed(3)} dB`;
  if (db >= 0.000001) return `${db.toFixed(Math.min(6, Math.ceil(-Math.log10(db)) + 1))} dB`;
  return "< 0.000001 dB";
}

const kHz = (hz: number) => `${Number((hz / 1000).toFixed(hz >= 10000 ? 2 : 3))} kHz`;
const rateText = (rate: number) => `${Number((rate / 1000).toFixed(1))} kHz`;

/** Apodizing cutoff as a fraction of the source Nyquist. */
export const APODIZING_CUTOFF = 0.95;

/**
 * Plain-language tooltips for the three cutoff choices with the numbers for
 * `sourceRate`. When the measured response `r` is for that rate, its real
 * crossings replace the estimates for the option it was measured with.
 */
export function cutoffTips(
  sourceRate: number,
  r: {
    ratio: number;
    /** The running ratio; `ratio` is 0 for a fractional one. */
    rate_ratio?: { up: number; down: number };
    source_rate: number;
    effective_cutoff?: number;
    crossings: { level_db: number; freq_hz: number }[];
    minus_3_db_hz: number;
  } | null,
  cutoffMode?: CutoffMode,
): { full: string; nyquistStop: string; apod: string } {
  const nyq = sourceRate / 2;
  const at = (level: number) => r?.crossings.find((c) => c.level_db === level)?.freq_hz;
  const resampling = r ? (r.rate_ratio ? r.rate_ratio.up !== r.rate_ratio.down : r.ratio > 1) : false;
  const measured = !!(r && resampling && r.source_rate === sourceRate);
  const eff = r?.effective_cutoff;
  const isApod = measured && (eff !== undefined ? eff <= 0.96 : (at(-6) ?? r.minus_3_db_hz) < nyq * 0.99);
  const isNyqStop =
    measured &&
    !isApod &&
    (cutoffMode === "nyquist_stop" || (eff !== undefined && eff < 0.999999));
  const isFull = measured && !isApod && !isNyqStop;
  const apodEdge = nyq * APODIZING_CUTOFF;
  const measuredSummary = () => {
    const a = at(-0.1);
    const b = at(-100) ?? at(-60);
    if (a === undefined || b === undefined) return "";
    return ` Measured now: −0.1 dB at ${fmtHzStat(a, 2)} Hz, ${b === at(-100) ? "−100" : "−60"} dB at ${fmtHzStat(b, 2)} Hz.`;
  };

  let full =
    `Full band: flat right up to the source Nyquist (${kHz(nyq)} at ${rateText(sourceRate)}). ` +
    `The filter is centred on Nyquist (−6 dB exactly there), so with long filters the whole transition is only a few Hz wide and straddles it.`;
  if (isFull) {
    full += measuredSummary();
  }

  let nyquistStop =
    `Stop at Nyquist: automatically shifts the −6 dB cutoff just below the source Nyquist (${kHz(nyq)} at ${rateText(sourceRate)}) ` +
    `by half the Kaiser transition width, so attenuation reaches the full stopband target at Nyquist. ` +
    `Zero imaging while staying as close to full band as possible (gives up exact original samples).`;
  if (isNyqStop && r) {
    const fc = (eff ?? 1) * nyq;
    const shiftHz = Math.max(0, nyq - fc);
    nyquistStop += ` Cutoff now: ${fmtHzStat(fc, 2)} Hz (${fmtHzStat(shiftHz, 2)} Hz below Nyquist).` + measuredSummary();
  }

  let apod =
    `Apodizing: the cutoff sits at 95% of the source Nyquist (about ${kHz(apodEdge)} at ${rateText(sourceRate)}), so the response is fully attenuated before Nyquist and there is no imaging at all. ` +
    `It also suppresses the pre-ringing left by the recording's own steep anti-alias filter. ` +
    `The cost is the top 5% of the band: content above about ${kHz(apodEdge)} is removed.`;
  if (isApod) {
    apod += measuredSummary();
  }
  return { full, nyquistStop, apod };
}

/**
 * Top of the response plots: the Nyquist the engine measures to, the
 * intermediate grid's (source × up) at a fractional ratio, which equals the
 * output Nyquist for an integer one. Zoom windows may reach it too.
 */
export function responsePlotTop(r: { source_rate: number; output_rate: number; rate_ratio: { up: number; down: number } }): number {
  return (r.source_rate * Math.max(1, r.rate_ratio.up)) / 2;
}

type WindowReq = { f_lo_hz: number; f_hi_hz: number; points: number };
type WindowRecord = { key: string; req: WindowReq } | null;

function sameWindow(a: WindowReq | null | undefined, b: WindowReq | null | undefined): boolean {
  return !!a && !!b && a.f_lo_hz === b.f_lo_hz && a.f_hi_hz === b.f_hi_hz && a.points === b.points;
}

/**
 * Which plot's zoom window to fetch next: one that is wanted, not in yet for
 * this design (`baseKey`), and not refused for exactly this selection (a
 * refused window is tried again only once the view or the design changes).
 */
export function nextWindowToFetch<K extends string>(
  wanted: Record<K, WindowReq | null>,
  have: Record<K, WindowRecord>,
  failed: Record<K, WindowRecord>,
  baseKey: string,
): K | null {
  for (const id of Object.keys(wanted) as K[]) {
    const w = wanted[id];
    if (w === null) continue;
    const h = have[id];
    if (h && h.key === baseKey && sameWindow(h.req, w)) continue;
    const f = failed[id];
    if (f && f.key === baseKey && sameWindow(f.req, w)) continue;
    return id;
  }
  return null;
}
