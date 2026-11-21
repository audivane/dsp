// The mock engine's `check_design`: the per-field bounds of
// `crates/dsp-core/src/design/check.rs` (same paths and messages), a ratio
// rule and tap budget per source rate, and a plausible plan (taps, memory,
// CPU on the mock's machine). Mock only: the real engine's plan is priced
// with the machine's measured kernel rates.

import type { DesignPlan, FieldError, FilterDesign, RatePlan, RateRatio, Window } from "./types";

type Kernel = Extract<FilterDesign, { kind: "windowed_sinc" | "lanczos" }>;

const MAX_PER_INPUT = 65535;
const BAND_RANGE: [number, number] = [55, 225];
const REFLECT_BETA: [number, number] = [2, 20];

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
  if (d.kind !== "windowed_sinc" && d.kind !== "lanczos") return c.errors;
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
  else {
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
        const delta = Math.max(0.005, band.stopband_start - band.passband_end);
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
  /** The engine block at a rate (frames). */
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
  const kernel = d.kind === "windowed_sinc" || d.kind === "lanczos";
  const { taps, exact } = kernel ? kernelTaps(d as Kernel, rate, r, ctx.autoDb) : { taps: 0, exact: true };
  const out = (rate * r.up) / r.down;
  let cpu: number;
  if (!kernel) cpu = (2 * out * (d.kind === "interpolator" && d.method === "table_sinc" ? 127 : 40)) / 5e9;
  else if (r.down === 1) {
    const b = ctx.block(rate);
    const parts = Math.max(1, Math.ceil(Math.ceil(taps / r.up) / b));
    cpu = 2 * (((rate / b) * parts * r.up * (b + 1)) / 0.5e9) * 1.15;
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

/** `check_design`: the bounds, then the ratio rule, budget and plan at each rate. */
export function checkDesign(d: FilterDesign, ctx: MockCheckContext): { errors: FieldError[]; plans: RatePlan[] } {
  const errors = checkAll(d, ctx.maxKernelTaps);
  if (errors.length > 0) return { errors, plans: [] };
  const plans: RatePlan[] = [];
  const seen = new Set<string>();
  const push = (path: string, message: string) => {
    const key = `${path}|${message.replace(/^at \d+ Hz \([^)]*\), /, "")}`;
    if (seen.has(key)) return;
    seen.add(key);
    errors.push({ path, message });
  };
  for (const rate of ctx.rates) {
    const r = ctx.ratioAt(rate);
    if (r.up === r.down) continue;
    const at = `at ${rate} Hz (${ratioText(r)}), `;
    const rule = ratioRule(d, r);
    if (rule) {
      push(rule[0], at + rule[1]);
      continue;
    }
    const p = plan(d, rate, r, ctx);
    if (p.grid_taps > ctx.maxKernelTaps) {
      const field = d.kind === "lanczos" ? "design.lobes" : "design.length";
      push(field, `${at}its kernel would have ${p.grid_taps} taps, more than ${ctx.maxKernelTaps}`);
      continue;
    }
    if (r.down > 1 && p.grid_taps > 0) {
      const field = d.kind === "lanczos" ? "design.lobes" : "design.length";
      const macs = ((rate * r.up) / r.down) * Math.ceil(p.grid_taps / r.up) * ctx.pipelines;
      if (p.grid_taps > RATIONAL_MAX_GRID) {
        push(field, `${at}the design needs ${p.grid_taps} taps on the ${r.up}x grid, more than ${RATIONAL_MAX_GRID}: count the length per input sample, or use a shorter design`);
        continue;
      }
      if (macs > RATIONAL_MAX_MACS) {
        push(field, `${at}the kernel needs ${Math.round(macs / 1e6)} M multiply-adds per second with the ${ctx.pipelines} pipelines that can run at once, more than the ${RATIONAL_MAX_MACS / 1e6} M budget: use a shorter design`);
        continue;
      }
    }
    plans.push({ source_rate: rate, ratio: r, plan: p });
  }
  return { errors, plans };
}

