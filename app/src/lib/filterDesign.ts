// The Custom filter editor's logic: a `FilterDesign` edited one field at a
// time. Every edit
// returns a whole new design, keeping the fields that still apply and filling
// the ones a new choice needs, so the editor never sends a design with a
// missing field. Whether the result passes is the engine's to say
// (`check_design`); the couplings fixed by the type (a band length goes with a
// band cutoff, Lanczos is built on the fir paths) are kept here.

import type {
  ArithmeticPath,
  BlockSize,
  CascadeStage,
  Cutoff,
  DesignPlan,
  FieldError,
  FilterDesign,
  FlattopSet,
  InterpMethod,
  Length,
  Phase,
  PhaseMethod,
  RatePlan,
  RateTarget,
  Sampling,
  Stepped,
  UpsamplerConfig,
  Window,
} from "../api/types";
import { fmtInt, fmtRate, fmtTaps } from "./format";

export type DesignKind = FilterDesign["kind"];
export type SincDesign = Extract<FilterDesign, { kind: "windowed_sinc" }>;
export type LanczosDesign = Extract<FilterDesign, { kind: "lanczos" }>;
export type KernelDesign = SincDesign | LanczosDesign;
export type WindowType = Window["type"];
export type LengthRule = Length["rule"];
export type CutoffMode = Cutoff["mode"];
export type PhaseType = Phase["type"];

/** The catalogue name of the Recommended design. */
export const RECOMMENDED_PRESET = "recommended";

// ------------------------------------------------------------ labels

export const KIND_LABEL: Record<DesignKind, string> = {
  windowed_sinc: "Windowed sinc",
  lanczos: "Lanczos",
  cascade: "Cascade",
  interpolator: "Interpolator",
  block_fft: "Block FFT",
};

export const WINDOW_LABEL: Record<WindowType, string> = {
  kaiser: "Kaiser",
  gaussian: "Gaussian",
  flattop: "Flat-top",
  chebyshev: "Chebyshev",
  nuttall: "Nuttall",
  hann: "Hann",
  tukey: "Tukey",
};

export const CASCADE_LABEL: Record<CascadeStage, string> = {
  fir: "FIR",
  fir_minimum: "FIR minimum",
  fir_asymmetric: "FIR asymmetric",
  frequency_sampled: "Frequency-sampled",
  elliptic: "Elliptic",
  inverse_chebyshev: "Inverse Chebyshev",
};

export const INTERP_LABEL: Record<InterpMethod, string> = {
  cosine: "Cosine",
  catmull_rom: "Catmull-Rom",
  table_sinc: "Table sinc",
};

export const PATH_LABEL: Record<ArithmeticPath, string> = {
  standard: "Standard",
  fir: "FIR",
  fir_after: "FIR, normalised after",
  exact_pi_inverse_after: "Exact (π·1/L), after",
  exact_over_ratio_before: "Exact (fc/L), before",
  exact_pi_inverse_pairwise: "Exact (π·1/L), pairwise",
  double_double: "Double-double",
};

export const FLATTOP_LABEL: Record<FlattopSet, string> = { hft196d: "HFT196D", hft223d: "HFT223D", hft248d: "HFT248D" };

// ------------------------------------------------------------ defaults

const STANDARD_NUMERICS = { path: "standard", grid_cutoff: "design_ratio" } as const;
const FIR_NUMERICS = { path: "fir", grid_cutoff: "scaled" } as const;
export const DEFAULT_BAND = { passband_end: 0.9, stopband_start: 1.0, attenuation_db: 150 } as const;
export const DEFAULT_BETA = 14;
export const DEFAULT_GAUSSIAN_DB = 150;
export const DEFAULT_ALPHA = 0.125;
export const DEFAULT_MIX = 0.75;
/** 2^-32: −192.7 dB sidelobes, the Chebyshev presets'. */
export const DEFAULT_SIDELOBE_RATIO = 2 ** -32;
/** The deepest Chebyshev sidelobes a reflected phase takes. */
export const REFLECT_MAX_SIDELOBE_DB = 190;
export const DEFAULT_FIXED_TAPS = 65536;
export const DEFAULT_PER_INPUT_TAPS = 511;
export const DEFAULT_LOBES = 23;

/** The basis of Music and Low latency: Kaiser, full band, linear phase, 64k taps. */
function baseDesign(): SincDesign {
  return {
    kind: "windowed_sinc",
    window: { type: "kaiser", beta: DEFAULT_BETA, sampling: "symmetric" },
    length: { rule: "fixed", taps: DEFAULT_FIXED_TAPS },
    decimation: { length: "keep" },
    cutoff: { mode: "fraction", value: 1 },
    phase: { type: "linear" },
    preserve_samples: false,
    rates: { accept: "any", fallback_window: null },
    numerics: { ...STANDARD_NUMERICS },
  };
}

/** A new Custom design: windowed sinc, Kaiser β 16, 4096 taps, stop at Nyquist, linear phase. */
export function defaultDesign(): SincDesign {
  return {
    ...baseDesign(),
    window: { type: "kaiser", beta: 16, sampling: "symmetric" },
    length: { rule: "fixed", taps: 4096 },
    cutoff: { mode: "nyquist_stop" },
  };
}

/** "Music": a long linear-phase Kaiser filter, full band. */
export function musicDesign(): SincDesign {
  return { ...baseDesign(), length: { rule: "fixed", taps: 262144 } };
}

/** "Low latency": a short minimum-phase Kaiser filter, full band. */
export function lowLatencyDesign(): SincDesign {
  return { ...baseDesign(), length: { rule: "fixed", taps: 16384 }, phase: { type: "minimum", method: "reflect" } };
}

// ------------------------------------------------------------ queries

export const isKernel = (d: FilterDesign): d is FilterDesign & KernelDesign => d.kind === "windowed_sinc" || d.kind === "lanczos";
export const isStandard = (d: KernelDesign) => d.numerics.path === "standard";
/** The phase is made by checked zero reflection. */
export const isReflected = (p: Phase) => p.type !== "linear" && p.method === "reflect";
const searchedLength = (l: Length) => l.rule === "band" || l.rule === "auto";

/** The windows each arithmetic path builds as the presets have them; used to pick a path when a window needs another. */
const PATH_WINDOWS: Record<ArithmeticPath, readonly WindowType[]> = {
  standard: ["kaiser", "gaussian", "tukey"],
  fir: ["kaiser", "gaussian", "flattop", "chebyshev", "nuttall", "hann"],
  fir_after: ["kaiser", "gaussian", "flattop", "chebyshev", "nuttall", "hann"],
  exact_pi_inverse_after: ["kaiser", "chebyshev", "nuttall"],
  exact_over_ratio_before: ["kaiser", "chebyshev", "nuttall"],
  exact_pi_inverse_pairwise: ["kaiser", "chebyshev", "nuttall"],
  double_double: ["gaussian"],
};

export function pathBuilds(path: ArithmeticPath, w: WindowType): boolean {
  return PATH_WINDOWS[path].includes(w);
}

// ------------------------------------------------------------ stepped values

export function isStepped<T>(v: Stepped<T>): v is { by_source_rate: Array<[number | null, T]> } {
  return typeof v === "object" && v !== null && "by_source_rate" in v;
}

/** The value a stepped value takes at a source rate. */
export function steppedAt<T>(v: Stepped<T>, rate: number): T {
  if (!isStepped(v)) return v;
  for (const [bound, x] of v.by_source_rate) if (bound === null || rate < bound) return x;
  return v.by_source_rate[v.by_source_rate.length - 1][1];
}

/** "167 below 50 kHz, 83 below 100 kHz, 63 above". */
export function steppedText<T>(v: Stepped<T>, fmt: (x: T) => string): string {
  if (!isStepped(v)) return fmt(v);
  const steps = v.by_source_rate;
  return steps
    .map(([bound, x], i) => (bound !== null ? `${fmt(x)} below ${fmtRate(bound)}` : `${fmt(x)} ${i === 0 ? "at every rate" : "above"}`))
    .join(", ");
}

// ------------------------------------------------------------ Chebyshev in dB

/** Sidelobe level in dB (positive) of a Chebyshev sidelobe ratio. */
export const sidelobeDb = (ratio: number) => -20 * Math.log10(ratio);
/** The sidelobe ratio for a level in dB (positive). */
export const sidelobeRatio = (db: number) => 10 ** (-db / 20);

// ------------------------------------------------------------ edits

/** The default window of a type for the design's path and phase. */
export function defaultWindow(type: WindowType, d: KernelDesign): Window {
  const standard = isStandard(d);
  const reflect = isReflected(d.phase);
  const searched = d.kind === "windowed_sinc" && !windowValueGiven(d);
  // The fir paths keep the presets' samplings; a reflected phase needs a symmetric prototype.
  const legacy: Sampling = standard || reflect ? "symmetric" : "periodic";
  switch (type) {
    case "kaiser":
      return { type, beta: searched ? null : DEFAULT_BETA, sampling: legacy };
    case "gaussian":
      return standard
        ? { type, stopband_db: searched ? null : DEFAULT_GAUSSIAN_DB, sampling: "symmetric" }
        : { type, alpha: DEFAULT_ALPHA, sampling: reflect ? "symmetric" : "integer_centre" };
    case "flattop":
      return { type, set: "hft248d", sampling: legacy };
    case "chebyshev":
      return { type, sidelobe_ratio: reflect ? sidelobeRatio(180) : DEFAULT_SIDELOBE_RATIO, sampling: "symmetric" };
    case "nuttall":
      return { type, sampling: "symmetric" };
    case "hann":
      return { type, sampling: legacy };
    case "tukey":
      return { type, taper: 0.25, sampling: "symmetric" };
  }
}

/** A manual length for a design that leaves a band or automatic one. */
function manualLength(d: KernelDesign): Length {
  return isStandard(d) ? { rule: "fixed", taps: DEFAULT_FIXED_TAPS } : { rule: "per_input", taps: DEFAULT_PER_INPUT_TAPS };
}

/** A window that a band or automatic length derives (β from the attenuation). */
function derivedWindow(w: Window): Window {
  if (w.type === "kaiser") return { ...w, beta: null };
  if (w.type === "gaussian" && "stopband_db" in w) return { ...w, stopband_db: null };
  return w;
}

/** A window with its own value again, for a manual length. */
function explicitWindow(w: Window): Window {
  if (w.type === "kaiser" && w.beta === null) return { ...w, beta: DEFAULT_BETA };
  if (w.type === "gaussian" && "stopband_db" in w && w.stopband_db === null) return { ...w, stopband_db: DEFAULT_GAUSSIAN_DB };
  return w;
}

/**
 * Whether the window's own value (Kaiser β, the standard Gaussian's
 * stopband) is given: a fraction or stop-at-Nyquist cutoff needs it, a band
 * cutoff derives it from the depth (or the accuracy target). The engine
 * refuses the other two combinations on every path.
 */
export function windowValueGiven(d: SincDesign): boolean {
  return d.cutoff.mode !== "band";
}

/** The window's value given or derived as the cutoff mode needs. */
export function applyBetaRule(d: SincDesign): SincDesign {
  const window = windowValueGiven(d) ? explicitWindow(d.window) : derivedWindow(d.window);
  return window === d.window ? d : { ...d, window };
}

/**
 * A number of taps as typed: a plain number (commas allowed), a power of
 * two (`2^20`), or a binary suffix (`64k` = 65,536, `1M` = 1,048,576).
 */
export function parseTaps(text: string): number | null {
  const t = text.trim().replace(/[,\s_]/g, "");
  const pow = t.match(/^2\^(\d{1,2})$/);
  if (pow) return 2 ** Number(pow[1]);
  const m = t.match(/^(\d*\.?\d+)([km])?$/i);
  if (!m) return null;
  const v = parseFloat(m[1]) * (m[2] ? (m[2].toLowerCase() === "k" ? 1024 : 1048576) : 1);
  return Number.isFinite(v) && v > 0 ? Math.round(v) : null;
}

/** The power-of-two tap counts the taps menu offers: 16k up to the engine's limit. */
export function tapPresets(maxKernelTaps: number): number[] {
  const out: number[] = [];
  for (let n = 16384; n <= Math.min(maxKernelTaps, 1 << 22); n *= 2) out.push(n);
  return out;
}

/** The arithmetic path changes, and the fields tied to it follow. */
export function setPath<D extends KernelDesign>(d: D, path: ArithmeticPath): D {
  if (d.numerics.path === path) return d;
  const toStandard = path === "standard";
  let next: D = {
    ...d,
    numerics: toStandard ? { ...STANDARD_NUMERICS } : { path, grid_cutoff: d.numerics.path === "standard" ? "scaled" : d.numerics.grid_cutoff },
  };
  // The standard path counts frames: it keeps the length when decimating.
  if (toStandard) next = { ...next, decimation: { length: "keep" } };
  if (toStandard && d.phase.type !== "linear" && d.phase.method === "cepstral") next = { ...next, phase: { ...d.phase, method: "reflect" } };
  if (next.kind === "windowed_sinc") {
    const w = next.window;
    // The two Gaussians: the standard path's takes a stopband, the others alpha.
    if (w.type === "gaussian" && toStandard !== ("stopband_db" in w)) next = { ...next, window: defaultWindow("gaussian", next) };
  }
  return next;
}

export function setWindowType(d: SincDesign, type: WindowType): SincDesign {
  if (d.window.type === type) return d;
  let next: SincDesign = d;
  if (!pathBuilds(d.numerics.path, type)) next = setPath(next, pathBuilds("standard", type) ? "standard" : "fir");
  if (type === "tukey") {
    // No calibrated stopband: linear phase and a manual length and cutoff.
    next = { ...next, phase: { type: "linear" } };
    if (next.cutoff.mode !== "fraction") next = { ...next, cutoff: { mode: "fraction", value: 1 } };
    if (searchedLength(next.length)) next = { ...next, length: manualLength(next) };
  }
  return { ...next, window: defaultWindow(type, next) };
}

/** Change one parameter of the current window (same type). */
export function setWindow(d: SincDesign, w: Window): SincDesign {
  return { ...d, window: w };
}

export function setLengthRule(d: SincDesign, rule: LengthRule, prev?: Partial<Record<LengthRule, Length>>): SincDesign {
  if (d.length.rule === rule) return d;
  const remembered = prev?.[rule];
  const length: Length =
    remembered ??
    (rule === "fixed"
      ? { rule, taps: DEFAULT_FIXED_TAPS }
      : rule === "per_input"
        ? { rule, taps: DEFAULT_PER_INPUT_TAPS }
        : rule === "output_span"
          ? { rule, taps: "ladder" }
          : rule === "auto"
            ? { rule, budget: "match_output" }
            : { rule: "band" });
  let next: SincDesign = { ...d, length };
  if (searchedLength(length)) {
    // A band or automatic length goes with a band cutoff, and β comes from it.
    if (next.cutoff.mode !== "band") next = { ...next, cutoff: { mode: "band", ...DEFAULT_BAND } };
    if (next.window.type === "tukey") next = { ...next, window: defaultWindow("kaiser", next) };
    if (next.preserve_samples) next = { ...next, preserve_samples: false };
  } else if (next.cutoff.mode === "band") next = { ...next, cutoff: { mode: "fraction", value: 1 } };
  return applyBetaRule(next);
}

export function setCutoffMode(d: SincDesign, mode: CutoffMode): SincDesign {
  if (d.cutoff.mode === mode) return d;
  if (mode === "band") {
    const length: Length = searchedLength(d.length) ? d.length : { rule: "band" };
    return applyBetaRule({ ...d, cutoff: { mode, ...DEFAULT_BAND }, length, preserve_samples: false });
  }
  const cutoff: Cutoff = mode === "fraction" ? { mode, value: 1 } : { mode };
  const next: SincDesign = { ...d, cutoff, preserve_samples: mode === "fraction" && d.preserve_samples };
  return applyBetaRule(searchedLength(d.length) ? { ...next, length: manualLength(d) } : next);
}

export function setPhaseType<D extends KernelDesign>(d: D, type: PhaseType): D {
  const cur = d.phase;
  if (cur.type === type) return d;
  if (type === "linear") return { ...d, phase: { type } };
  const method: PhaseMethod = cur.type !== "linear" ? cur.method : isStandard(d) ? "reflect" : "cepstral";
  const phase: Phase = type === "minimum" ? { type, method } : { type, method, mix: cur.type === "intermediate" ? cur.mix : DEFAULT_MIX };
  return { ...d, phase, preserve_samples: false };
}

export function setPhaseMethod<D extends KernelDesign>(d: D, method: PhaseMethod): D {
  if (d.phase.type === "linear" || d.phase.method === method) return d;
  let next: D = { ...d, phase: { ...d.phase, method } };
  // A reflected phase needs a symmetric (linear-phase) prototype.
  if (method === "reflect" && next.kind === "windowed_sinc" && next.window.sampling !== "symmetric") {
    next = { ...next, window: { ...next.window, sampling: "symmetric" } as Window };
  }
  return next;
}

export function setMix<D extends KernelDesign>(d: D, mix: number): D {
  if (d.phase.type !== "intermediate") return d;
  return { ...d, phase: { ...d.phase, mix } };
}

/** Kernel designs left behind by a kind switch, so switching back restores them. */
export type KindMemory = Partial<Record<DesignKind, FilterDesign>>;

export function setKind(d: FilterDesign, kind: DesignKind, memory: KindMemory = {}): FilterDesign {
  if (d.kind === kind) return d;
  const based = d.based_on !== undefined ? { based_on: d.based_on } : {};
  const kept = memory[kind];
  if (kept) return { ...kept, ...based };
  switch (kind) {
    case "windowed_sinc": {
      if (d.kind === "lanczos") {
        const { lobes: _lobes, kind: _k, ...rest } = d;
        const base = { ...rest, kind: "windowed_sinc", window: { type: "kaiser", beta: DEFAULT_BETA, sampling: "periodic" }, length: { rule: "per_input", taps: DEFAULT_PER_INPUT_TAPS }, cutoff: { mode: "fraction", value: 1 } } as SincDesign;
        return { ...base, window: defaultWindow("kaiser", base) };
      }
      return { ...defaultDesign(), ...based };
    }
    case "lanczos": {
      const src: KernelDesign = d.kind === "windowed_sinc" ? d : defaultDesign();
      let next: LanczosDesign = {
        ...based,
        kind: "lanczos",
        lobes: DEFAULT_LOBES,
        decimation: src.decimation,
        phase: src.phase,
        preserve_samples: false,
        rates: { accept: "upsampling", fallback_window: null },
        numerics: src.numerics,
      };
      // The Lanczos kernel is built on the fir paths.
      if (next.numerics.path !== "fir" && next.numerics.path !== "fir_after") {
        next = { ...next, numerics: { ...FIR_NUMERICS }, decimation: { length: "scale", odd: "down", base_taps: null } };
        if (next.phase.type !== "linear" && next.phase.method === "reflect") next = { ...next, phase: { ...next.phase, method: "cepstral" } };
      }
      return next;
    }
    case "cascade":
      return { ...based, kind, stage: "fir" };
    case "interpolator":
      return { ...based, kind, method: "table_sinc" };
    case "block_fft":
      return { ...based, kind, block: { input: 16384 }, overlap: "quarter_average" };
  }
}

/** Powers of two a block FFT block can be: input 2^10..2^16, output 2^11..2^22. */
export function blockChoices(which: "input" | "output"): number[] {
  const [lo, hi] = which === "input" ? [10, 16] : [11, 22];
  const out: number[] = [];
  for (let e = lo; e <= hi; e++) out.push(2 ** e);
  return out;
}

export function blockSize(b: BlockSize): { which: "input" | "output"; size: number } {
  return "input" in b ? { which: "input", size: b.input } : { which: "output", size: b.output };
}

// ------------------------------------------------------------ comparing

function canonical(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(canonical);
  if (v && typeof v === "object") {
    const o = v as Record<string, unknown>;
    return Object.fromEntries(
      Object.keys(o)
        .filter((k) => o[k] !== undefined)
        .sort()
        .map((k) => [k, canonical(o[k])]),
    );
  }
  return v;
}

/** The design's values as a stable string, `based_on` left out. */
export function designKey(d: FilterDesign): string {
  const { based_on: _b, ...rest } = d;
  return JSON.stringify(canonical(rest));
}

/** Deep equality of two designs' values (the basis name is not a value). */
export function sameDesign(a: FilterDesign, b: FilterDesign): boolean {
  return designKey(a) === designKey(b);
}

// ------------------------------------------------------------ errors

/** Where a field error shows in the editor. */
export type ErrorGroup = "kind" | "window" | "length" | "cutoff" | "phase" | "samples" | "decimation" | "rates" | "numerics" | "target" | "general";

const ADVANCED_GROUPS: readonly ErrorGroup[] = ["samples", "decimation", "rates", "numerics"];
export const isAdvancedGroup = (g: ErrorGroup) => ADVANCED_GROUPS.includes(g);

export function errorGroup(path: string): ErrorGroup {
  const p = path.replace(/^chain\.upsampler\./, "");
  if (p === "target" || p === "source_rate") return "target";
  const seg = p.startsWith("design.") ? p.slice("design.".length).split(/[.[]/)[0] : null;
  switch (seg) {
    case "kind":
      return "kind";
    case "window":
      return "window";
    case "length":
    case "lobes":
    case "block":
      return "length";
    case "cutoff":
      return "cutoff";
    case "phase":
      return "phase";
    case "preserve_samples":
      return "samples";
    case "decimation":
      return "decimation";
    case "rates":
      return "rates";
    case "numerics":
      return "numerics";
    default:
      return "general";
  }
}

/** The field a daemon refusal names (`chain.upsampler.design.window.beta: must be ...`), or null. */
export function refusalField(message: string): FieldError | null {
  // The app's error text may start with a capital and end with a full stop.
  const m = /^(?:chain\.upsampler\.)?(design(?:\.[a-z_]+(?:\[\d+\])?)*|target): (.*)$/is.exec(message.trim());
  if (!m) return null;
  const path = m[1].charAt(0).toLowerCase() + m[1].slice(1);
  return { path, message: m[2].replace(/\.$/, "") };
}

/** Errors grouped by where they show. */
export function groupErrors(errors: readonly FieldError[]): Partial<Record<ErrorGroup, FieldError[]>> {
  const out: Partial<Record<ErrorGroup, FieldError[]>> = {};
  for (const e of errors) (out[errorGroup(e.path)] ??= []).push(e);
  return out;
}

/** The field name in plain words, for an error shown away from its field. */
export function fieldLabel(path: string): string {
  const p = path.replace(/^chain\.upsampler\./, "").replace(/^design\.?/, "");
  const last = p.split(".").pop() ?? p;
  const words: Record<string, string> = {
    beta: "β",
    alpha: "α",
    stopband_db: "Stopband",
    sidelobe_ratio: "Sidelobes",
    taper: "Taper",
    sampling: "Window sampling",
    type: "Window",
    taps: "Length",
    budget: "Accuracy",
    rule: "Length",
    lobes: "Lobes",
    block: "Block",
    value: "Cutoff",
    mode: "Cutoff",
    passband_end: "Band edges",
    stopband_start: "Band edges",
    attenuation_db: "Depth",
    mix: "Blend",
    method: "Phase method",
    preserve_samples: "Original samples",
    base_taps: "Decimation base",
    length: p.startsWith("decimation") ? "Decimation" : "Length",
    fallback_window: "Fallback window",
    path: "Arithmetic",
    grid_cutoff: "Grid cutoff",
    rates: "Ratios",
    phase: "Phase",
    window: "Window",
    cutoff: "Cutoff",
    target: "Rate",
  };
  return words[last.replace(/\[\d+\]$/, "")] ?? (p || "Design");
}

// ------------------------------------------------------------ plans

export function fmtBytes(n: number): string {
  if (n >= 2 ** 30) return `${(n / 2 ** 30).toFixed(n >= 10 * 2 ** 30 ? 0 : 1)} GiB`;
  if (n >= 2 ** 20) return `${(n / 2 ** 20).toFixed(n >= 10 * 2 ** 20 ? 0 : 1)} MiB`;
  if (n >= 2 ** 10) return `${Math.round(n / 2 ** 10)} KiB`;
  return `${Math.round(n)} B`;
}

export function fmtCpu(load: number | undefined): string {
  if (load === undefined) return "—";
  const pct = load * 100;
  return `${pct < 1 ? pct.toFixed(2) : pct < 10 ? pct.toFixed(1) : Math.round(pct)}%`;
}

const ratioLabel = (r: { up: number; down: number }) => (r.down === 1 ? `×${r.up}` : `${r.up}/${r.down}`);

export interface PlanRow {
  rate: number;
  rateText: string;
  ratioText: string;
  tapsText: string;
  tapsTip: string;
  memoryText: string;
  memoryTip: string;
  cpuText: string;
}

/** One readout row per planned source rate. */
export function planRows(plans: readonly RatePlan[]): PlanRow[] {
  return [...plans]
    .sort((a, b) => a.source_rate - b.source_rate)
    .map(({ source_rate, ratio, plan }) => ({
      rate: source_rate,
      rateText: fmtRate(source_rate),
      ratioText: ratioLabel(ratio),
      tapsText: plan.grid_taps === 0 ? "fixed" : `${plan.grid_taps_exact ? "" : "≤ "}${fmtTaps(plan.grid_taps)}`,
      tapsTip:
        plan.grid_taps === 0
          ? "This kind runs its own fixed filters: no kernel to size"
          : `${fmtInt(plan.grid_taps)} taps on the ${plan.grid_up}× grid${plan.grid_taps_exact ? "" : " (an upper bound: the engine sizes it when it designs the kernel)"}`,
      memoryText: fmtBytes(plan.resident_bytes),
      memoryTip: `${fmtBytes(plan.resident_bytes)} while it runs (both channels), up to ${fmtBytes(plan.workspace_bytes)} while it is built`,
      cpuText: fmtCpu(plan.cpu_load),
    }));
}

/**
 * The delay a design adds at a source rate, ms, from its plan: half the
 * kernel for a linear-phase kernel of known length. Null when only the
 * designed kernel can tell (minimum or intermediate phase, an estimated
 * length, the runtime kinds): the measured response gives it then.
 */
export function planLatencyMs(d: FilterDesign, plan: DesignPlan, sourceRate: number): number | null {
  if (!isKernel(d) || d.phase.type !== "linear" || !plan.grid_taps_exact || plan.grid_taps <= 0) return null;
  const gridRate = sourceRate * plan.grid_up;
  return ((plan.grid_taps - 1) / 2 / gridRate) * 1000;
}

// ------------------------------------------------------------ summaries

function lengthShort(l: Length): string {
  switch (l.rule) {
    case "fixed":
      return fmtTaps(l.taps);
    case "per_input":
      return isStepped(l.taps) ? "compact" : `${fmtInt(l.taps)}/in`;
    case "output_span":
      return l.taps === "ladder" ? "constant time" : fmtTaps(l.taps);
    case "band":
      return "band";
    case "auto":
      return "auto";
  }
}

/** The design part of the sidebar line: "Kaiser 64k · linear". */
export function designSummary(d: FilterDesign): string {
  switch (d.kind) {
    case "windowed_sinc":
      return `${WINDOW_LABEL[d.window.type]} ${lengthShort(d.length)} · ${d.phase.type}`;
    case "lanczos":
      return `Lanczos ${d.lobes} · ${d.phase.type}`;
    case "cascade":
      return `Cascade ${CASCADE_LABEL[d.stage]}`;
    case "interpolator":
      return INTERP_LABEL[d.method];
    case "block_fft":
      return "Block FFT";
  }
}

// ------------------------------------------------------------ config patches

/**
 * Merge a patch into an upsampler config: a preset clears the design and a
 * design clears the preset (exactly one of them is set on the wire).
 */
export function patchUpsampler(cur: UpsamplerConfig, patch: Partial<UpsamplerConfig>): UpsamplerConfig {
  const target: RateTarget = patch.target ?? cur.target;
  if (patch.preset) return { target, preset: patch.preset, design: null };
  if (patch.design) return { target, preset: null, design: patch.design };
  return { ...cur, target };
}
