// The Convolution page's response plot: the engine's `conv_response` curves
// as plot series (each item's channels or routes, and the whole stage per
// output), optional 1/N-octave smoothing, and the y range that fits them.

import type { ConvResponse, ConvResponseCurve } from "../api/types";

export type ConvQuantity = "mag" | "gd";
/** Smoothing: none, or 1/N octave. */
export type ConvSmoothing = 0 | 6 | 3;

export interface ConvSeries {
  /** Stable across redraws: `out:0`, `item:2:1:0` (index, in, out; `-` for a mono file). */
  key: string;
  label: string;
  combined: boolean;
  curve: ConvResponseCurve;
}

/** A channel's short name: L, R, then "ch 3". */
export function channelName(n: number): string {
  return n === 0 ? "L" : n === 1 ? "R" : `ch ${n}`;
}

/** A curve's route: "L", "L→R", or "" for a mono file on every channel. */
export function routeName(c: ConvResponseCurve): string {
  if (c.in === undefined || c.out === undefined) return c.out === undefined ? "" : channelName(c.out);
  return c.in === c.out ? channelName(c.out) : `${channelName(c.in)}→${channelName(c.out)}`;
}

/** Every curve of a reply as a series: the items in chain order, then the combined outputs. */
export function convSeries(r: ConvResponse): ConvSeries[] {
  const out: ConvSeries[] = [];
  for (const item of r.items) {
    for (const c of item.curves) {
      const route = routeName(c);
      out.push({ key: `item:${item.index}:${c.in ?? "-"}:${c.out ?? "-"}`, label: route ? `${item.label} ${route}` : item.label, combined: false, curve: c });
    }
  }
  for (const c of r.outputs) out.push({ key: `out:${c.out ?? 0}`, label: `Combined ${channelName(c.out ?? 0)}`, combined: true, curve: c });
  return out;
}

/** Shown unless toggled: the combined outputs on, the items off. */
export function seriesShown(s: ConvSeries, toggled: Readonly<Record<string, boolean>>): boolean {
  return toggled[s.key] ?? s.combined;
}

/**
 * 1/`n`-octave smoothing of a curve on a log grid: each point the power
 * average (dB) of the points within ±1/(2n) octave. `n = 0` returns `mag`.
 */
export function smoothDb(freqs: readonly number[], mag: readonly number[], n: number): number[] {
  if (!n) return [...mag];
  const half = 1 / (2 * n);
  const out: number[] = [];
  let lo = 0;
  let hi = 0;
  let sum = 0;
  const pw = mag.map((m) => Math.pow(10, m / 10));
  for (let i = 0; i < freqs.length; i++) {
    const a = freqs[i] * Math.pow(2, -half);
    const b = freqs[i] * Math.pow(2, half);
    while (hi < freqs.length && freqs[hi] <= b) sum += pw[hi++];
    while (lo < hi && freqs[lo] < a) sum -= pw[lo++];
    const count = hi - lo;
    out.push(count > 0 && sum > 0 ? Math.max(-200, 10 * Math.log10(sum / count)) : mag[i]);
  }
  return out;
}

/**
 * Group delay smoothed the same way (a plain mean of the points that have one);
 * `null` where none in the span has one.
 */
export function smoothGd(freqs: readonly number[], gd: readonly (number | null)[], n: number): (number | null)[] {
  if (!n) return [...gd];
  const half = 1 / (2 * n);
  return freqs.map((f, i) => {
    const a = f * Math.pow(2, -half);
    const b = f * Math.pow(2, half);
    let sum = 0;
    let count = 0;
    for (let j = i; j >= 0 && freqs[j] >= a; j--) if (gd[j] !== null) ((sum += gd[j] as number), count++);
    for (let j = i + 1; j < freqs.length && freqs[j] <= b; j++) if (gd[j] !== null) ((sum += gd[j] as number), count++);
    return count ? sum / count : null;
  });
}

/** The values a series plots: magnitude or group delay, smoothed. */
export function seriesValues(freqs: readonly number[], c: ConvResponseCurve, q: ConvQuantity, n: ConvSmoothing): (number | null)[] {
  return q === "mag" ? smoothDb(freqs, c.mag_db, n) : smoothGd(freqs, c.group_delay_ms, n);
}

/** A y range that fits `values`: dB in 6 dB steps (at least −12..+6, the floor left out), ms in round steps. */
export function fitRange(values: readonly (number | null)[][], q: ConvQuantity): { y0: number; y1: number } {
  let lo = Infinity;
  let hi = -Infinity;
  for (const v of values)
    for (const x of v) {
      if (x === null || !Number.isFinite(x)) continue;
      if (q === "mag" && x <= -150) continue;
      lo = Math.min(lo, x);
      hi = Math.max(hi, x);
    }
  if (q === "mag") {
    if (!Number.isFinite(lo)) return { y0: -24, y1: 12 };
    const y0 = Math.max(-120, Math.min(-12, Math.floor((lo - 3) / 6) * 6));
    const y1 = Math.min(60, Math.max(6, Math.ceil((hi + 3) / 6) * 6));
    return { y0, y1 };
  }
  if (!Number.isFinite(lo)) return { y0: 0, y1: 10 };
  const span = Math.max(1, hi - lo);
  const step = Math.pow(10, Math.floor(Math.log10(span)));
  const y0 = Math.floor((lo - span * 0.1) / step) * step;
  const y1 = Math.ceil((hi + span * 0.1) / step) * step;
  return { y0: Math.min(y0, 0), y1: Math.max(y1, y0 + step) };
}
