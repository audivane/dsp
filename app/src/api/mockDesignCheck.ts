// The mock engine's `check_design`: the per-field bounds of
// `crates/dsp-core/src/design/check.rs` (same paths and messages), a ratio
// rule and tap budget per source rate, and a plausible plan (taps, memory,
// CPU on the mock's machine). Mock only: the real engine's plan is priced
// with the machine's measured kernel rates.

import type { DesignAdvice, DesignCheck, DesignFix, DesignPlan, FieldError, FilterDesign, Length, RateNote, RateRatio, Window } from "./types";
import { catalogueDesign } from "./mockCatalogue";
import { polyphaseSeconds } from "../lib/polyphasePlan";
import { kaiserAttenuationDb } from "../lib/upsamplerLimits";
import { bandPassbandEnd, bandPinMisfit, bandRate, type PinnedBand } from "../lib/filterDesign";

export { bandPassbandEnd };

type Kernel = Extract<FilterDesign, { kind: "windowed_sinc" | "lanczos" | "optimal" }>;

const MAX_PER_INPUT = 65535;
const BAND_RANGE: [number, number] = [55, 225];
const REFLECT_BETA: [number, number] = [2, 20];
const NW_RANGE: [number, number] = [1, 10];
/** `window_model::DPSS_SHORTFALL_DB`: the DPSS model is Kaiser's at β = π·NW, this much shallower. */
const DPSS_SHORTFALL_DB = 12;
const REFLECT_MAX_DB = 190;
const OPTIMAL_MAX_DB = { equiripple: 150, least_squares: 135 } as const;
const OPTIMAL_MAX_TAPS = { equiripple: 4095, least_squares: 65535 } as const;
/** `optimal::EQUIRIPPLE_LEVEL_DB`, `LEAST_SQUARES_LEVEL_DB`: the deepest estimated depth of a fixed length. */
const OPTIMAL_LEVEL_DB = { equiripple: 160, least_squares: 200 } as const;

/** `optimal::estimate_db`: Kaiser's depth estimate of `n` taps over a transition of `df` cycles per sample. */
const optimalDepthDb = (n: number, df: number) => 7.95 + 14.36 * df * (Math.max(1, n) - 1);

/** `general::optimal_dc_floor_db`: the estimated depth an optimal design's L branches need for their DC gains. */
function optimalDcFloorDb(k: Extract<Kernel, { kind: "optimal" }>, L: number): number {
  if (L < 2) return -Infinity;
  const tol = 10 ** ((k.phase.type === "linear" ? 0.01 : 0.1) / 20) - 1;
  return k.method === "equiripple" ? 20 * Math.log10((L - 1) / tol) : 20 * Math.log10(1 / tol) + 6;
}

class Checker {
  errors: FieldError[] = [];
  fail(path: string, message: string) {
    this.errors.push({ path: `design.${path}`, message });
  }
  finite(path: string, v: number) {
    if (Number.isFinite(v)) return true;
    this.fail(path, `must be finite (got ${v})`);
    return false;
  }
  range(path: string, v: number, [lo, hi]: [number, number]) {
    if (this.finite(path, v) && !(v >= lo && v <= hi)) this.fail(path, `must be between ${lo} and ${hi} (got ${v})`);
  }
  count(path: string, v: number, lo: number, hi: number) {
    if (!(Number.isInteger(v) && v >= lo && v <= hi)) this.fail(path, `must be between ${lo} and ${hi} (got ${v})`);
  }
}

const reflected = (k: Kernel) => k.phase.type !== "linear" && k.phase.method === "reflect";

/**
 * `design::band_pin_note`: the check's note at source rate `rate` where the
 * pin falls back, the band being of the Nyquist at `bandRate`.
 */
export function bandPinNote(b: PinnedBand, rate: number, bandRate = rate): string | null {
  const f = bandPinMisfit(b, bandRate);
  if (f === null || b.passband_end_hz == null) return null;
  return `at ${rate} Hz, the passband end ${b.passband_end_hz} Hz (${f.toFixed(3)} of the Nyquist) does not fit below the stopband start (${b.stopband_start}): the passband ends at ${b.passband_end} of the Nyquist (${Math.round((b.passband_end * bandRate) / 2)} Hz) instead`;
}

function checkWindow(c: Checker, path: string, w: Window, k: Kernel) {
  const p = (f: string) => `${path}.${f}`;
  const standard = k.numerics.path === "standard";
  const reflect = reflected(k);
  switch (w.type) {
    case "kaiser":
      if (w.beta !== null) {
        c.range(p("beta"), w.beta, [0, 40]);
        if (reflect && Number.isFinite(w.beta)) c.range(p("beta"), w.beta, REFLECT_BETA);
      }
      break;
    case "gaussian":
      if ("alpha" in w) {
        c.range(p("alpha"), w.alpha, [0.05, 0.5]);
        if (standard) c.fail(p("alpha"), "the standard path's Gaussian takes a stopband, not alpha");
        if (k.numerics.path === "double_double" && w.sampling !== "integer_centre") c.fail(p("sampling"), "the double-double path builds the integer-centre Gaussian only");
        if (w.sampling === "integer_centre") {
          if (reflect) c.fail(p("sampling"), "a reflected phase needs a linear-phase prototype: symmetric sampling");
          return;
        }
      } else {
        if (w.stopband_db !== null) c.range(p("stopband_db"), w.stopband_db, BAND_RANGE);
        if (!standard) c.fail(p("type"), "the stopband Gaussian is the standard path's");
      }
      break;
    case "flattop":
    case "nuttall":
    case "hann":
      if (standard) c.fail(p("type"), "this window is not on the standard path: use a preset path");
      break;
    case "chebyshev": {
      if (c.finite(p("sidelobe_ratio"), w.sidelobe_ratio)) {
        const db = w.sidelobe_ratio > 0 ? -20 * Math.log10(w.sidelobe_ratio) : Infinity;
        if (!(db >= 40 && db <= 220)) c.fail(p("sidelobe_ratio"), `must be between 10^(-220/20) and 10^(-40/20) (got ${w.sidelobe_ratio})`);
        else if (reflect && db > 190) c.fail(p("sidelobe_ratio"), `a reflected phase takes sidelobes down to -190 dB (got ${db.toFixed(1)} dB)`);
      }
      if (standard) c.fail(p("type"), "this window is not on the standard path: use a preset path");
      break;
    }
    case "dpss":
      if (w.nw !== null) {
        c.range(p("nw"), w.nw, NW_RANGE);
        if (reflect && Number.isFinite(w.nw) && kaiserAttenuationDb(Math.PI * w.nw) - DPSS_SHORTFALL_DB > REFLECT_MAX_DB) {
          let nw = NW_RANGE[1];
          for (let lo = NW_RANGE[0], i = 0; i < 60; i++) {
            const mid = (lo + nw) / 2;
            if (kaiserAttenuationDb(Math.PI * mid) - DPSS_SHORTFALL_DB > REFLECT_MAX_DB) nw = mid;
            else lo = mid;
          }
          c.fail(p("nw"), `a reflected phase keeps at most ${REFLECT_MAX_DB} dB: NW up to ${nw.toFixed(2)} (got ${w.nw})`);
        }
      }
      if (standard) c.fail(p("type"), "this window is not on the standard path: use a preset path");
      if (w.sampling !== "symmetric") {
        c.fail(p("sampling"), "the DPSS window is sampled symmetric only");
        return;
      }
      break;
    case "tukey":
      if (c.finite(p("taper"), w.taper) && !(w.taper > 0 && w.taper <= 1)) c.fail(p("taper"), `must be in (0, 1] (got ${w.taper})`);
      if (k.phase.type !== "linear") c.fail(p("type"), "the Tukey window has no calibrated stopband: linear phase only");
      break;
  }
  if (w.sampling === "integer_centre") c.fail(p("sampling"), "integer_centre is the Gaussian's sampling");
  else if (reflect && !standard && w.sampling !== "symmetric") c.fail(p("sampling"), "a reflected phase needs a linear-phase prototype: symmetric sampling");
}

function stepsShape(c: Checker, path: string, bounds: (number | null)[]) {
  const ok = bounds.length > 0 && bounds[bounds.length - 1] === null && bounds.slice(0, -1).every((b, i, a) => b !== null && (i === 0 || (a[i - 1] as number) < b));
  if (!ok) c.fail(path, "the steps need increasing bounds and an unbounded last step");
}

/** `check_all`: every per-field bound of the design. */
export function checkAll(d: FilterDesign, maxKernelTaps: number): FieldError[] {
  const c = new Checker();
  if (d.kind === "block_fft") {
    if ("input" in d.block) {
      const n = d.block.input;
      if (!(Number.isInteger(Math.log2(n)) && n >= 1024 && n <= 65536)) c.fail("block", `must be a power of two from 1024 to 65536 (got ${n})`);
    } else {
      const n = d.block.output;
      if (!(Number.isInteger(Math.log2(n)) && n >= 2048 && n <= 4194304)) c.fail("block", `must be a power of two from 2048 to 4194304 (got ${n})`);
    }
    return c.errors;
  }
  if (d.kind !== "windowed_sinc" && d.kind !== "lanczos" && d.kind !== "optimal") return c.errors;
  const k = d;
  const path = k.numerics.path;
  const reflect = reflected(k);
  if (k.phase.type === "intermediate") c.range("phase.mix", k.phase.mix, [0, 1]);
  if (reflect && !["fir", "fir_after", "standard"].includes(path)) c.fail("phase.method", "a reflected phase is built on the fir paths or the standard path");
  if (k.decimation.length === "scale" && k.decimation.base_taps !== null) c.count("decimation.base_taps", k.decimation.base_taps, 1, MAX_PER_INPUT);
  if (path === "standard" && k.decimation.length !== "keep") c.fail("decimation.length", "the standard path counts frames: keep only");
  if (path !== "standard" && k.numerics.grid_cutoff === "design_ratio" && k.decimation.length !== "keep") {
    c.fail("decimation.length", "the design-ratio grid cutoff designs the kernel at the decimation ratio itself: keep only");
  }
  const fb = k.rates?.fallback_window;
  if (fb) {
    if (fb.window.type === "tukey") c.fail("rates.fallback_window", "the Tukey window cannot be a fallback");
    checkWindow(c, "rates.fallback_window.window", fb.window, k);
  }
  if (k.kind === "lanczos") {
    c.count("lobes", k.lobes, 1, MAX_PER_INPUT);
    if (path !== "fir" && path !== "fir_after") c.fail("numerics.path", "the Lanczos kernel is built on the fir paths");
    if (k.preserve_samples) c.fail("preserve_samples", "a Lanczos kernel's zeros are off the input grid");
    return c.errors;
  }
  if (k.kind === "optimal") {
    const { passband_end: pe, stopband_start: se } = k;
    if (Number.isFinite(pe) && Number.isFinite(se) && (pe < 0.5 || pe > 1 || se <= pe || se > 1.5)) {
      c.fail("passband_end", `needs 0.5 <= passband end (${pe}) <= 1 and passband end < stopband start (${se}) <= 1.5`);
    }
    const l = k.length;
    if (l.rule === "fixed") c.count("length.taps", l.taps, 1, maxKernelTaps);
    else if (l.rule === "per_input") {
      if (typeof l.taps === "number") c.count("length.taps", l.taps, 1, MAX_PER_INPUT);
      else {
        stepsShape(c, "length.taps", l.taps.by_source_rate.map((s) => s[0]));
        l.taps.by_source_rate.forEach(([, t], i) => c.count(`length.taps[${i}]`, t, 1, MAX_PER_INPUT));
      }
    } else if (l.rule === "auto") {
      if (l.budget !== "match_output") c.range("length.budget", l.budget.db, [55, OPTIMAL_MAX_DB[k.method]]);
    } else c.fail("length.rule", "an optimal design takes a fixed, per-input or automatic length");
    if (path !== "fir" && path !== "fir_after") c.fail("numerics.path", "an optimal design is built on the fir paths");
    if (k.preserve_samples) c.fail("preserve_samples", "an optimal low-pass is not a Nyquist filter: its zeros are off the input grid");
    if (fb) c.fail("rates.fallback_window", "an optimal design has no window to fall back from");
    return c.errors;
  }
  const w = k.window;
  checkWindow(c, "window", w, k);
  const l = k.length;
  switch (l.rule) {
    case "fixed":
      c.count("length.taps", l.taps, 1, maxKernelTaps);
      break;
    case "per_input":
      if (typeof l.taps === "number") c.count("length.taps", l.taps, 1, MAX_PER_INPUT);
      else {
        stepsShape(c, "length.taps", l.taps.by_source_rate.map((s) => s[0]));
        l.taps.by_source_rate.forEach(([, t], i) => c.count(`length.taps[${i}]`, t, 1, MAX_PER_INPUT));
      }
      break;
    case "output_span":
      if (l.taps !== "ladder" && !(Number.isInteger(Math.log2(l.taps)) && l.taps >= 4096 && l.taps <= 4194304)) {
        c.fail("length.taps", `must be "ladder" or a power of two from 4096 to 4194304 (got ${l.taps})`);
      }
      break;
    case "auto":
      if (l.budget !== "match_output") {
        c.range("length.budget", l.budget.db, [55, 200]);
        if (w.type === "tukey") c.fail("length.rule", "the Tukey window has no calibrated stopband");
      }
      break;
    case "band":
      break;
  }
  const searched = l.rule === "band" || l.rule === "auto";
  if (searched !== (k.cutoff.mode === "band")) c.fail("cutoff.mode", "a band or automatic length goes with a band cutoff, and only with one");
  const cut = k.cutoff;
  if (cut.mode === "fraction") {
    const values = typeof cut.value === "number" ? [cut.value] : cut.value.by_source_rate.map((s) => s[1]);
    if (typeof cut.value !== "number") stepsShape(c, "cutoff.value", cut.value.by_source_rate.map((s) => s[0]));
    for (const v of values) if (c.finite("cutoff.value", v) && !(v > 0 && v <= 1)) c.fail("cutoff.value", `must be in (0, 1] (got ${v})`);
    if (k.preserve_samples && values.some((v) => v !== 1)) c.fail("preserve_samples", "needs the cutoff fraction 1.0");
  } else if (cut.mode === "nyquist_stop") {
    if (w.type === "tukey") c.fail("cutoff.mode", "the Tukey window has no calibrated stopband");
    if (k.preserve_samples) c.fail("preserve_samples", "needs the cutoff fraction 1.0");
  } else {
    const { passband_end: pe, stopband_start: se, attenuation_db: a } = cut;
    if (Number.isFinite(pe) && Number.isFinite(se) && (pe < 0.5 || pe > 1 || se <= pe || se > 1.5)) {
      c.fail("cutoff.passband_end", `needs 0.5 <= passband end (${pe}) <= 1 and passband end < stopband start (${se}) <= 1.5`);
    }
    // An absolute passband end replaces the fraction above; whether it
    // fits the band is checked once the source rate is known
    // (`bandPassbandEnd`).
    if (cut.passband_end_hz != null && !(Number.isFinite(cut.passband_end_hz) && cut.passband_end_hz > 0)) {
      c.fail("cutoff.passband_end_hz", `must be a frequency above 0 Hz (got ${cut.passband_end_hz})`);
    }
    if (l.rule !== "auto") {
      c.range("cutoff.attenuation_db", a, BAND_RANGE);
      if (reflect && a > 190) c.fail("cutoff.attenuation_db", `a reflected phase promises at most 190 dB (got ${a})`);
    }
    if (w.type === "tukey") c.fail("cutoff.mode", "the Tukey window has no calibrated stopband");
    if (k.preserve_samples) c.fail("preserve_samples", "needs the cutoff fraction 1.0");
  }
  if (k.preserve_samples && k.phase.type !== "linear") c.fail("preserve_samples", "needs linear phase");
  return c.errors;
}

const stepAt = <T>(v: T | { by_source_rate: [number | null, T][] }, rate: number): T => {
  if (typeof v !== "object" || v === null || !("by_source_rate" in v)) return v as T;
  for (const [b, x] of v.by_source_rate) if (b === null || rate < b) return x;
  return v.by_source_rate[v.by_source_rate.length - 1][1];
};

function ladder(out: number): number {
  const steps: [number, number][] = [[50000, 16], [100000, 17], [250000, 18], [500000, 19], [1000000, 20], [2000000, 21]];
  for (const [b, e] of steps) if (out < b) return 2 ** e;
  return 2 ** 22;
}

/** Grid taps of a kernel at a rate, and whether that is exact. */
function kernelTaps(k: Kernel, rate: number, r: RateRatio, autoDb: number): { taps: number; exact: boolean } {
  const L = r.up;
  const M = r.down;
  const out = (rate * L) / M;
  let n: number;
  let exact = true;
  if (k.kind === "lanczos") n = (2 * k.lobes + 1) * L;
  else if (k.kind === "optimal") {
    const l = k.length;
    if (l.rule === "fixed") n = l.taps * M;
    else if (l.rule === "per_input") n = stepAt(l.taps, rate) * L;
    else {
      // Kaiser's equiripple estimate, a quarter longer for least squares;
      // from the depth its branches' DC needs when deeper.
      const budget = l.rule === "auto" && l.budget !== "match_output" ? l.budget.db : Math.min(autoDb, OPTIMAL_MAX_DB[k.method]);
      const atten = Math.max(budget, optimalDcFloorDb(k, L));
      const df = Math.max(0.005, k.stopband_start - k.passband_end) / (2 * Math.max(L, M));
      n = Math.ceil(((atten - 7.95) / (14.36 * df) + 1) * (k.method === "least_squares" ? 1.25 : 1));
      exact = false;
    }
    n = Math.max(3, n % 2 === 0 ? n - 1 : n);
  } else {
    const l = k.length;
    switch (l.rule) {
      case "fixed":
        n = l.taps * M;
        break;
      case "per_input":
        n = stepAt(l.taps, rate) * L;
        break;
      case "output_span": {
        const span = l.taps === "ladder" ? ladder(out) : l.taps;
        n = Math.max(1, Math.floor(span / L) - 1) * L;
        break;
      }
      default: {
        const band = k.cutoff.mode === "band" ? k.cutoff : { passband_end: 0.9, stopband_start: 1, attenuation_db: 150 };
        const atten = l.rule === "auto" ? (l.budget === "match_output" ? autoDb : l.budget.db) : band.attenuation_db;
        // The band is of the lower Nyquist (`design::band_rate`).
        const delta = Math.max(0.005, band.stopband_start - bandPassbandEnd(band, bandRate(rate, r)));
        const gauss = k.window.type === "gaussian" ? 2 : 1;
        n = 2 * Math.ceil(((atten - 7.95) / (14.36 * delta)) * gauss) * Math.max(L, M) + 1;
        exact = l.rule === "band";
      }
    }
  }
  if (k.phase.type === "intermediate") {
    n *= 4;
    if (k.phase.method === "reflect") exact = false;
  }
  return { taps: n, exact };
}

export interface MockCheckContext {
  rates: readonly number[];
  ratioAt: (rate: number) => RateRatio;
  maxKernelTaps: number;
  /** `match_output` as a level in dB. */
  autoDb: number;
  /** The chain block at a rate (frames). */
  block: (rate: number) => number;
  /** Rational pipelines that can run at once (2, or 4 with A/B). */
  pipelines: number;
}

/** The rational kernel's grid size and multiply-add budgets (the daemon's `check_rational_budget`). */
const RATIONAL_MAX_GRID = 1_048_576;
const RATIONAL_MAX_MACS = 800e6;

const ratioText = (r: RateRatio) => (r.down === 1 ? `x${r.up}` : `${r.up}/${r.down}`);
const isPow2 = (n: number) => Number.isInteger(Math.log2(n));

/** Why a design does not run at a ratio, as (path, message), or null. */
function ratioRule(d: FilterDesign, r: RateRatio): [string, string] | null {
  const integer = r.down === 1;
  switch (d.kind) {
    case "cascade": {
      let x = r.up;
      while (x % 3 === 0) x /= 3;
      while (x % 2 === 0) x /= 2;
      return integer && r.up % 2 === 0 && x === 1 ? null : ["design.rates", `the cascade runs at even integer ratios made of 2 and 3, not ${ratioText(r)}`];
    }
    case "interpolator":
      return integer ? null : ["design.rates", `the interpolators run at integer ratios, not ${ratioText(r)}`];
    case "block_fft":
      return integer && isPow2(r.up) ? null : ["design.rates", `the block FFT runs at power-of-two ratios, not ${ratioText(r)}`];
    default: {
      const k = d as Kernel;
      const derived = k.kind === "lanczos" || k.preserve_samples ? "upsampling" : k.length.rule === "output_span" ? "integer" : "any";
      const accept = k.rates?.accept ?? derived;
      if (accept === "integer" && !integer) return ["design.rates", `takes integer ratios only, not ${ratioText(r)}`];
      if (accept === "upsampling" && r.up < r.down) return ["design.rates", `takes upsampling ratios only, not ${ratioText(r)}`];
      if (k.preserve_samples && r.up < r.down) return ["design.preserve_samples", `needs an output rate at or above the input rate (${ratioText(r)})`];
      return null;
    }
  }
}

function plan(d: FilterDesign, rate: number, r: RateRatio, ctx: MockCheckContext): DesignPlan {
  const kernel = d.kind === "windowed_sinc" || d.kind === "lanczos" || d.kind === "optimal";
  const { taps, exact } = kernel ? kernelTaps(d as Kernel, rate, r, ctx.autoDb) : { taps: 0, exact: true };
  const out = (rate * r.up) / r.down;
  let cpu: number;
  if (!kernel) cpu = (2 * out * (d.kind === "interpolator" && d.method === "table_sinc" ? 127 : 40)) / 5e9;
  else if (r.down === 1) {
    const block = ctx.block(rate);
    cpu = 2 * polyphaseSeconds(taps, r.up, Math.min(1024, block), block, rate);
  } else cpu = (2 * out * Math.ceil(taps / r.up)) / 5e9;
  return {
    grid_up: r.up,
    grid_down: r.down,
    grid_taps: taps,
    grid_taps_exact: exact,
    workspace_bytes: Math.max(65536, taps * 8 * (kernel && d.phase.type !== "linear" ? 64 : 4)),
    resident_bytes: Math.max(32768, taps * 8 * 2 * 2),
    cpu_load: cpu,
  };
}

/** Why `d` does not build from `rate` at `r`, as (path, message), or null. */
function rateRefusal(d: FilterDesign, rate: number, r: RateRatio, ctx: MockCheckContext): [string, string] | null {
  const rule = ratioRule(d, r);
  if (rule) return rule;
  const p = plan(d, rate, r, ctx);
  const field = d.kind === "lanczos" ? "design.lobes" : "design.length";
  if (d.kind === "optimal" && p.grid_taps / (d.phase.type === "intermediate" ? 4 : 1) > OPTIMAL_MAX_TAPS[d.method]) {
    const [name, instead] = d.method === "equiripple" ? ["an equiripple design is built (its cost grows with the square of the length)", "least squares or a windowed sinc"] : ["a least-squares design is built", "a windowed sinc"];
    return ["design.length", `${name} up to ${OPTIMAL_MAX_TAPS[d.method]} taps on the grid (this one needs ${p.grid_taps}): use ${instead} for longer kernels`];
  }
  if (d.kind === "optimal" && d.length.rule !== "auto") {
    const n = p.grid_taps / (d.phase.type === "intermediate" ? 4 : 1);
    const est = optimalDepthDb(n, (d.stopband_start - d.passband_end) / (2 * Math.max(r.up, r.down)));
    const level = OPTIMAL_LEVEL_DB[d.method];
    const floor = optimalDcFloorDb(d, r.up);
    if (est > level) {
      return [
        "design.length",
        d.method === "equiripple"
          ? `an equiripple design of ${n} taps over this band at ${ratioText(r)} would be about ${est.toFixed(0)} dB deep, past the ${level} dB f64 levels its ripple to: shorten it, narrow the transition or use an automatic length`
          : `a least-squares design of ${n} taps over this band at ${ratioText(r)} would be about ${est.toFixed(0)} dB deep, far past its 137 to 153 dB floor, where the solve no longer holds the passband: shorten it or use an automatic length`,
      ];
    }
    if (est < floor) {
      return [
        "design.length",
        d.method === "equiripple"
          ? `an equiripple design of ${n} taps over this band at ${ratioText(r)} is about ${est.toFixed(0)} dB deep, but its flat stopband adds up in each of the ${r.up} polyphase branches, whose DC gains need about ${floor.toFixed(0)} dB here: lengthen it, widen the transition or use an automatic length`
          : `a least-squares design of ${n} taps over this band at ${ratioText(r)} is about ${est.toFixed(0)} dB deep, too shallow for the ${r.up} polyphase branches' DC gains (about ${floor.toFixed(0)} dB): lengthen it, widen the transition or use an automatic length`,
      ];
    }
  }
  if (p.grid_taps > ctx.maxKernelTaps) return [field, `the design does not run at ${ratioText(r)}: its kernel would have ${p.grid_taps} taps, more than ${ctx.maxKernelTaps}; use a lower ratio or a shorter design`];
  if (r.down > 1 && p.grid_taps > 0) {
    const macs = ((rate * r.up) / r.down) * Math.ceil(p.grid_taps / r.up) * ctx.pipelines;
    if (p.grid_taps > RATIONAL_MAX_GRID) return [field, `the ${ratioText(r)} kernel needs ${p.grid_taps} taps, more than ${RATIONAL_MAX_GRID}`];
    if (macs > RATIONAL_MAX_MACS) return [field, `the ${ratioText(r)} kernel needs ${Math.round(macs / 1e6)} M multiply-adds per second with the ${ctx.pipelines} pipelines that can run at once, more than the ${RATIONAL_MAX_MACS / 1e6} M budget`];
  }
  return null;
}

// ------------------------------------------------------------ plain words
// The mock's `design::explain`: each refusal classified by trying other
// lengths, the rates grouped by reason, fixes checked at every rate.

interface Case {
  rate: number;
  r: RateRatio;
  refusal: [string, string] | null;
}

type Problem = { kind: "short"; need: number } | { kind: "long"; why: string } | { kind: "ratio" } | { kind: "other" };

type Knob = { rule: "fixed"; v: number } | { rule: "per_input"; v: number } | null;

function knob(d: FilterDesign): Knob {
  if (d.kind !== "windowed_sinc" && d.kind !== "optimal") return null;
  const l = d.length;
  if (l.rule === "fixed") return { rule: "fixed", v: l.taps };
  if (l.rule === "per_input" && typeof l.taps === "number") return { rule: "per_input", v: l.taps };
  return null;
}

/** `catalogue::RECOMMENDED`: what a rate the design does not build at plays. */
const RECOMMENDED_PRESET = "recommended";

function withLength(d: FilterDesign, length: Length): FilterDesign {
  return { ...d, length } as FilterDesign;
}

const fixedOf = (d: FilterDesign) => (v: number) => withLength(d, { rule: "fixed", taps: v });
const perInputOf = (d: FilterDesign) => (v: number) => withLength(d, { rule: "per_input", taps: v });

/** The lengths `make(v)` builds with at a case, as one interval found from `start`; null when none. */
function interval(c: Case, make: (v: number) => FilterDesign, start: number, min: number, max: number, ctx: MockCheckContext): [number, number] | null {
  const ok = (v: number) => rateRefusal(make(v), c.rate, c.r, ctx) === null;
  const s = Math.min(max, Math.max(min, start));
  let anchor: number | null = ok(s) ? s : null;
  for (let v = s; anchor === null && v < max; ) if (ok((v = Math.min(max, v * 2)))) anchor = v;
  for (let v = s; anchor === null && v > min; ) if (ok((v = Math.max(min, Math.floor(v / 2))))) anchor = v;
  if (anchor === null) return null;
  let lo = min;
  if (!ok(min)) {
    let [bad, good] = [min, anchor];
    while (good - bad > 1) {
      const mid = bad + Math.floor((good - bad) / 2);
      if (ok(mid)) good = mid;
      else bad = mid;
    }
    lo = good;
  }
  let hi = max;
  if (!ok(max)) {
    let [good, bad] = [anchor, max];
    while (bad - good > 1) {
      const mid = good + Math.floor((bad - good) / 2);
      if (ok(mid)) good = mid;
      else bad = mid;
    }
    hi = good;
  }
  return [lo, hi];
}

function classify(d: FilterDesign, c: Case, ctx: MockCheckContext): Problem {
  const [path, message] = c.refusal!;
  if (path === "design.rates" || path === "design.preserve_samples") return { kind: "ratio" };
  const k = knob(d);
  const range = k ? interval(c, k.rule === "fixed" ? fixedOf(d) : perInputOf(d), k.v, k.rule === "fixed" ? 3 : 1, k.rule === "fixed" ? ctx.maxKernelTaps : MAX_PER_INPUT, ctx) : null;
  if (k && range && k.v < range[0]) return { kind: "short", need: range[0] };
  if (k && range && k.v > range[1]) {
    const why = message.includes("multiply-adds") || /kernel needs \d+ taps/.test(message)
      ? "too heavy for a fractional ratio"
      : message.includes("kernel would have")
        ? "over the engine's longest kernel"
        : d.kind === "optimal"
          ? `too long for ${d.method === "equiripple" ? "an equiripple" : "a least-squares"} design`
          : "too long";
    return { kind: "long", why };
  }
  return { kind: "other" };
}

const thousands = (n: number) => n.toLocaleString("en-US");
const about = (n: number) => {
  if (n < 100) return n;
  let step = 1;
  while (n / step >= 100) step *= 10;
  return Math.ceil(n / step) * step;
};
const rateLabel = (rate: number) => `${Number((rate / 1000).toFixed(3))}k`;
const ratioLabel = (r: RateRatio) => (r.down === 1 ? `×${r.up}` : `×${r.up}/${r.down}`);
const joinAnd = (xs: string[]) => (xs.length < 2 ? xs.join("") : `${xs.slice(0, -1).join(", ")} and ${xs[xs.length - 1]}`);
const joinOr = (xs: string[]) => (xs.length < 2 ? xs.join("") : `${xs.slice(0, -1).join(", ")}, or ${xs[xs.length - 1]}`);

function phrase(p: Problem, at: string, needs: number[]): string {
  switch (p.kind) {
    case "short":
      return `too short ${at} (needs about ${joinAnd([...new Set(needs.map(about))].sort((a, b) => a - b).map(thousands))})`;
    case "long":
      return `${p.why} ${at}`;
    case "ratio":
      return `not made for the ratio ${at}`;
    default:
      return `not buildable ${at}`;
  }
}

export interface MockExplanation {
  message: string;
  rates: { source_rate: number; ratio: RateRatio; path: string; short: string; detail: string }[];
  fixes: DesignFix[];
}

/** `design::explain`: plain words for the cases that do not build, and fixes that build at all of them. */
export function explain(d: FilterDesign, cases: Case[], applied: boolean, ctx: MockCheckContext): MockExplanation {
  const failing = cases.filter((c) => c.refusal).map((c) => ({ c, p: classify(d, c, ctx) }));
  const k = knob(d);
  const subject = k?.rule === "fixed" ? `${thousands(k.v)} taps` : k?.rule === "per_input" ? `${thousands(k.v)} taps per input sample` : "This design";
  const unit = k?.rule === "per_input" ? " per input sample" : " taps";
  const rates = failing.map(({ c, p }) => {
    let short = phrase(p, "here", p.kind === "short" ? [p.need] : []);
    if (p.kind === "short") short = short.replace(/\)$/, `${unit})`);
    return { source_rate: c.rate, ratio: c.r, path: c.refusal![0], short, detail: c.refusal![1] };
  });
  const groups = new Map<string, typeof failing>();
  for (const f of failing) {
    const key = f.p.kind === "long" ? `long ${f.p.why}` : f.p.kind;
    groups.set(key, [...(groups.get(key) ?? []), f]);
  }
  const parts = [...groups.values()].map((g) =>
    phrase(g[0].p, `at ${joinAnd(g.map(({ c }) => `${rateLabel(c.rate)} ${ratioLabel(c.r)}`))}`, g.flatMap(({ p }) => (p.kind === "short" ? [p.need] : []))),
  );
  const fixes = mockFixes(d, cases, failing.map((f) => f.p), ctx);
  const todo = fixes.map((f) => f.label[0].toLowerCase() + f.label.slice(1));
  if (todo.length === 0) {
    const short = failing.some((f) => f.p.kind === "short");
    const long = failing.some((f) => f.p.kind === "long");
    todo.push(short && !long ? "lengthen it" : long && !short ? "shorten it or pick a lighter design" : "change the length");
  }
  todo.push(applied ? "allow fewer rates" : "change the rate target");
  const t = joinOr(todo);
  return { message: `${subject} is ${joinAnd(parts)}. ${t[0].toUpperCase()}${t.slice(1)}.`, rates, fixes };
}

function mockFixes(d: FilterDesign, cases: Case[], problems: Problem[], ctx: MockCheckContext): DesignFix[] {
  const out: DesignFix[] = [];
  if (d.kind !== "windowed_sinc" && d.kind !== "optimal") return out;
  const everywhere = (x: FilterDesign) => cases.every((c) => rateRefusal(x, c.rate, c.r, ctx) === null);
  if (d.length.rule !== "auto" && (d.kind === "optimal" || d.cutoff.mode === "band")) {
    const auto = withLength(d, { rule: "auto", budget: "match_output" });
    if (everywhere(auto)) out.push({ label: "Use automatic length", design: auto });
  }
  const k = knob(d);
  const odd = d.kind === "optimal";
  const up = cases.find((c) => c.refusal)?.r.up ?? 1;
  const [fixedStart, perStart] = k?.rule === "fixed" ? [k.v, Math.max(1, Math.floor(k.v / up))] : k?.rule === "per_input" ? [k.v * up, k.v] : [1023, 64];
  const common = (make: (v: number) => FilterDesign, start: number, min: number, max: number) => {
    let [lo, hi] = [min, max];
    for (const c of cases) {
      const iv = interval(c, make, start, min, max, ctx);
      if (!iv) return null;
      lo = Math.max(lo, iv[0]);
      hi = Math.min(hi, iv[1]);
    }
    return lo <= hi ? ([lo, hi] as const) : null;
  };
  const pick = (cur: number, lo: number, hi: number, wantOdd: boolean) => {
    const v = Math.min(hi, Math.max(lo, cur));
    if (!wantOdd || v % 2 === 1) return v;
    return v < hi ? v + 1 : v > lo ? v - 1 : null;
  };
  const fixed = common(fixedOf(d), fixedStart, 3, ctx.maxKernelTaps);
  const fv = fixed && pick(fixedStart, fixed[0], fixed[1], odd);
  if (fv && !(k?.rule === "fixed" && k.v === fv) && everywhere(fixedOf(d)(fv))) out.push({ label: `Set ${thousands(fv)} taps`, design: fixedOf(d)(fv) });
  const per = common(perInputOf(d), perStart, 1, MAX_PER_INPUT);
  const pv = per && pick(perStart, per[0], per[1], false);
  if (pv && !(k?.rule === "per_input" && k.v === pv) && everywhere(perInputOf(d)(pv))) {
    out.push({ label: `Set ${thousands(pv)} taps per input sample`, design: perInputOf(d)(pv) });
  }
  const pe = d.kind === "optimal" ? d.passband_end : d.cutoff.mode === "band" && d.cutoff.passband_end_hz == null ? d.cutoff.passband_end : null;
  if (pe !== null && problems.every((p) => p.kind === "short")) {
    const set = (x: number): FilterDesign => (d.kind === "optimal" ? { ...d, passband_end: x } : { ...d, cutoff: { ...(d.cutoff as Extract<typeof d.cutoff, { mode: "band" }>), passband_end: x } });
    if (everywhere(set(pe * 0.5))) {
      let [good, bad] = [pe * 0.5, pe];
      for (let i = 0; i < 20; i++) {
        const mid = (good + bad) / 2;
        if (everywhere(set(mid))) good = mid;
        else bad = mid;
      }
      const x = Math.floor(good * 1000) / 1000;
      if (x < pe && everywhere(set(x))) out.push({ label: `Widen the transition: pass to ${x.toFixed(3)}`, design: set(x) });
    }
  }
  return out;
}

/** Where `d` does not build at the rates of `ctx` (each with a few plain words), when it builds at some. */
export function designFallbacks(d: FilterDesign, ctx: MockCheckContext): { source_rate: number; reason: string }[] {
  const check = checkDesign(d, ctx);
  return check.notes.filter((n) => n.plays).map((n) => ({ source_rate: n.source_rate, reason: n.message }));
}

/**
 * `check_design`: the bounds, then each rate's ratio rule and budgets. A
 * design that builds at none of the rates is refused in plain words; one
 * that builds at some plays Recommended at the others (`notes`), with the
 * same advice.
 */
export function checkDesign(d: FilterDesign, ctx: MockCheckContext): DesignCheck {
  const errors = checkAll(d, ctx.maxKernelTaps);
  if (errors.length > 0) return { errors, plans: [], notes: [] };
  const cases: Case[] = ctx.rates
    .map((rate) => ({ rate, r: ctx.ratioAt(rate) }))
    .filter(({ r }) => r.up !== r.down)
    .map(({ rate, r }) => ({ rate, r, refusal: rateRefusal(d, rate, r, ctx) }));
  // A pinned passband end that falls back at a rate the design builds at
  // (the band is of the lower Nyquist, on every path).
  const pins: RateNote[] = [];
  if (d.kind === "windowed_sinc" && d.cutoff.mode === "band") {
    for (const c of cases) {
      const note = c.refusal ? null : bandPinNote(d.cutoff, c.rate, bandRate(c.rate, c.r));
      if (note !== null) pins.push({ source_rate: c.rate, path: "design.cutoff.passband_end_hz", message: note });
    }
  }
  if (!cases.some((c) => c.refusal)) return { errors, plans: cases.map((c) => ({ source_rate: c.rate, ratio: c.r, plan: plan(d, c.rate, c.r, ctx) })), notes: pins };
  const applied = cases.some((c) => !c.refusal);
  const x = explain(d, cases, applied, ctx);
  const advice: DesignAdvice = {
    message: x.message,
    applied,
    details: x.rates.map((r) => ({ source_rate: r.source_rate, ratio: r.ratio, path: r.path, message: r.detail })),
    fixes: x.fixes,
  };
  if (!applied) return { errors: [{ path: x.rates[0]?.path ?? "design", message: x.message }], plans: [], notes: [], advice };
  const rec = catalogueDesign(RECOMMENDED_PRESET);
  return {
    errors,
    plans: cases.map((c) => ({ source_rate: c.rate, ratio: c.r, plan: plan(c.refusal && rec ? rec : d, c.rate, c.r, ctx) })),
    notes: [...pins, ...x.rates.map((r) => ({ source_rate: r.source_rate, path: r.path, message: r.short, plays: RECOMMENDED_PRESET }))].sort((a, b) => a.source_rate - b.source_rate),
    advice,
  };
}
