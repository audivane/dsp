// The engine's named upsampling filters (`FilterPreset`), decomposed into a
// family and up to three choices (length, shape, phase) so the Upsampling
// view can offer them as "family, then variant". One table entry per preset
// name: `filterPresets.test.ts` checks it against the generated type, so a
// new engine preset fails the test instead of silently missing from the UI.
//
// Lengths, cutoffs and the ratio rules follow `dsp-core::presets`
// (`check_ratio`, `rational::check`).

import type { FilterPhase, FilterPreset, RateRatio, RateTarget } from "../api/types";
import { ALLOWED_SOURCE_RATES } from "./upsamplerLimits";
import { pastTermLimit, resolveRateRatio } from "./upsamplerRate";
import { FALLBACK_LIMITS } from "./engineLimits";

export type FamilyId =
  | "gaussian"
  | "flattop"
  | "chebyshev"
  | "kaiser"
  | "halfband"
  | "lanczos"
  | "nuttall"
  | "hann"
  | "leaky"
  | "craven"
  | "cascade"
  | "interp"
  | "block";

export type LengthId = "tiny" | "small" | "short" | "compact" | "medium" | "long" | "xlong" | "huge" | "mega" | `a${number}`;

/** Where a kernel's -6 dB point sits (README "Cutoffs"), as a fraction of the source Nyquist. */
export type CutoffId = "nyquist" | "wide" | "steep" | "std" | "steep_short" | "apod" | "craven" | "compact" | "leaky" | "early" | "lanczos" | "none";

export type Dim = "length" | "shape" | "phase";

/** How the engine runs it: a polyphase kernel or its own runtime. */
type Runtime = "fir" | "cascade" | "interp" | "block";
/** What it accepts beyond integer upsampling (`rational::Class`). */
type RationalClass = "integer" | "up" | "any";

export interface PresetEntry {
  name: FilterPreset;
  family: FamilyId;
  length: LengthId | null;
  /** The family's second choice: roll-off, window, stage, method or variant. */
  shape: string | null;
  phase: FilterPhase;
  /** Taps per input sample (null: rate-dependent, see `lengthNote`). */
  taps: number | null;
  lengthNote?: string;
  cutoff: CutoffId;
  /** Short name for the sidebar summary ("Flat-top xlong apod"). */
  label: string;
}

export interface FamilyInfo {
  id: FamilyId;
  label: string;
  /** One line: what it sounds like / what it trades. */
  blurb: string;
  /** Label of the shape row ("Roll-off", "Window"...), when it has one. */
  shapeLabel?: string;
  shapes?: Record<string, { label: string; tip: string }>;
  lengthLabel?: string;
  /** Preset picked when the family is chosen and nothing was used before. */
  default: FilterPreset;
}

const CUTOFF: Record<CutoffId, { fraction: number | null; text: string }> = {
  nyquist: { fraction: 1, text: "cutoff exactly at the source Nyquist (keeps the input samples, −6 dB there)" },
  wide: { fraction: 1 - 256.25 / 22050, text: "wide passband: −6 dB at 21.79 kHz for a 44.1 kHz source" },
  steep: { fraction: 253 / 256, text: "straddles Nyquist: −6 dB at 21.79 kHz for a 44.1 kHz source, only about 34 dB down at Nyquist itself" },
  std: { fraction: 21025 / 22050, text: "standard roll-off: −6 dB at 21.03 kHz for a 44.1 kHz source" },
  steep_short: { fraction: 125 / 133, text: "−6 dB at 20.72 kHz for a 44.1 kHz source" },
  apod: { fraction: 20512.5 / 22050, text: "apodizing: −6 dB at 20.51 kHz for a 44.1 kHz source, fully down before Nyquist, replaces the recording's own pre-ringing" },
  craven: { fraction: (20000 / 22050 + 1) / 2, text: "apodizing after Craven: flat to 20 kHz at every source rate and fully down (−150 dB) by the source Nyquist, so the recording's own brick-wall ringing at Nyquist is removed" },
  compact: { fraction: null, text: "cutoff set by the source rate: 0.88 of Nyquist below 50 kHz (19.4 kHz at 44.1 kHz), 0.725 below 100 kHz, 0.6 above" },
  leaky: { fraction: 380 / 441, text: "early, leaky roll-off: −6 dB at 19 kHz for a 44.1 kHz source, only 66 dB down at Nyquist" },
  early: { fraction: 27 / 32, text: "early roll-off: −0.1 dB at 0.80 of Nyquist, −100 dB by about 0.92 (−6 dB at 18.60 kHz for a 44.1 kHz source)" },
  lanczos: { fraction: 1, text: "−13 dB at the source Nyquist (some imaging), does not keep the input samples" },
  none: { fraction: null, text: "" },
};

const ROLL_OFF = (keys: CutoffId[]) =>
  Object.fromEntries(
    keys.map((k) => [
      k,
      {
        label: { nyquist: "Nyquist", wide: "Wide", steep: "Steep", std: "Standard", steep_short: "Steep", apod: "Apodizing", craven: "Craven", compact: "By rate", leaky: "Leaky", early: "Early", lanczos: "Lanczos", none: "" }[k],
        tip: CUTOFF[k].text.charAt(0).toUpperCase() + CUTOFF[k].text.slice(1) + ".",
      },
    ]),
  );

export const FAMILIES: readonly FamilyInfo[] = [
  {
    id: "gaussian",
    label: "Gaussian",
    blurb: "Smooth fade-out instead of ringing at one frequency; soft at short lengths, steeper when long.",
    shapeLabel: "Roll-off",
    shapes: ROLL_OFF(["nyquist", "wide", "std", "apod", "compact", "early"]),
    default: "gaussian_long",
  },
  {
    id: "flattop",
    label: "Flat-top",
    blurb: "Deepest stopband for its length (−196 to −248 dB sidelobes), with a slower transition.",
    shapeLabel: "Roll-off",
    shapes: ROLL_OFF(["wide", "std", "apod", "compact", "early"]),
    default: "flattop_long",
  },
  {
    id: "chebyshev",
    label: "Chebyshev",
    blurb: "Short kernels with an even stopband floor at −193 dB (the 32-bit level).",
    default: "chebyshev",
  },
  {
    id: "kaiser",
    label: "Kaiser (steep / compact)",
    blurb: "Steep: the widest passband here, straddling Nyquist. Compact: shorter, cutoff set by the source rate.",
    default: "kaiser_steep",
  },
  {
    id: "halfband",
    label: "Half-band",
    blurb: "Cutoff exactly at Nyquist: keeps every input sample, −6 dB at Nyquist, so some imaging there.",
    shapeLabel: "Window",
    shapes: {
      chebyshev: { label: "Chebyshev", tip: "Dolph-Chebyshev window: an even stopband floor (−193 dB large, −145 medium, −120 small, −96 tiny)." },
      kaiser: { label: "Kaiser", tip: "Kaiser window (β = 7π): sidelobes that keep falling with frequency." },
    },
    default: "halfband_medium",
  },
  {
    id: "lanczos",
    label: "Lanczos",
    blurb: "A fixed number of sinc lobes and no other window; −13 dB at Nyquist, little ringing with few lobes.",
    lengthLabel: "Lobes",
    default: "lanczos2047",
  },
  {
    id: "nuttall",
    label: "Nuttall",
    blurb: "A short, light filter (117 taps per input sample) that rolls off a little before Nyquist.",
    default: "nuttall",
  },
  {
    id: "hann",
    label: "Hann (Nyquist)",
    blurb: "Very long (16,383 taps per input sample) with the cutoff at Nyquist: keeps every input sample.",
    default: "hann_nyquist",
  },
  {
    id: "leaky",
    label: "Short leaky",
    blurb: "Short, little ringing, early leaky roll-off that lets some images through. A filter only, no decoding.",
    default: "short_leaky",
  },
  {
    id: "craven",
    label: "Apodizing (Craven)",
    blurb: "Peter Craven's apodizing filter: fully down by Nyquist, so the recording's own pre-ringing goes, a gentle transition from 20 kHz at every source rate, and minimum phase, so nothing rings before a transient.",
    default: "craven_apod",
  },
  {
    id: "cascade",
    label: "Cascade",
    blurb: "Built from ×2 and ×3 stages; even ratios made of 2 and 3 only (×2, ×4, ×6, ×8, ×12, ×16…).",
    shapeLabel: "Stage",
    shapes: {
      fir: { label: "FIR", tip: "255-tap windowed-sinc stages: wide transition, only about 41 dB down at the source Nyquist." },
      fft: { label: "FFT", tip: "512-tap frequency-sampled stages: the steepest of the cascades." },
      elliptic: { label: "Elliptic", tip: "One recursive (IIR) elliptic stage per step: 0.001 dB ripple, −100 dB, minimum phase." },
      inverse_chebyshev: { label: "Inverse Chebyshev", tip: "Recursive (IIR) stages with a monotone passband, −100 dB, minimum phase." },
    },
    default: "cascade_fir",
  },
  {
    id: "interp",
    label: "Simple interpolators",
    blurb: "Almost no latency or CPU, but weak image rejection: NOS (sample hold), cosine, Catmull-Rom or a table sinc.",
    shapeLabel: "Method",
    shapes: {
      nos: { label: "NOS (sample hold)", tip: "Zero-order sample hold with no reconstruction filter: half-sample delay, sinc roll-off and full imaging." },
      cosine: { label: "Cosine", tip: "Raised-cosine blend of the two nearest samples: the shortest possible, the most imaging." },
      catmull_rom: { label: "Catmull-Rom", tip: "4-point cubic through the nearest samples." },
      table_sinc: { label: "Table sinc", tip: "127-tap windowed sinc read from a table: much better image rejection, still short." },
    },
    default: "table_sinc",
  },
  {
    id: "block",
    label: "Block FFT",
    blurb: "Brick wall at Nyquist inside each block, but not time-invariant (the response at one block position has images 60 to 80 dB down; a steady tone's are far lower) and slow to respond.",
    shapeLabel: "Variant",
    shapes: {
      standard: { label: "Standard", tip: "16,384-sample blocks, averaged: about 0.37 s of latency." },
      fast: { label: "Fast", tip: "Keeps the middle half of each block: less work, about 0.46 s of latency." },
      large: { label: "Large", tip: "2^20 / ratio samples per block: changes less across a block (images about 90 dB down at one block position), 0.37 s (×64) to 11.9 s (×2) of latency." },
    },
    default: "block_fft",
  },
];

const FAMILY_BY_ID = Object.fromEntries(FAMILIES.map((f) => [f.id, f])) as Record<FamilyId, FamilyInfo>;

export function familyInfo(id: FamilyId): FamilyInfo {
  return FAMILY_BY_ID[id];
}

const COMPACT_TAPS = "167, 83 or 63 taps per input sample by source rate (below 50 kHz, below 100 kHz, above)";
const HUGE_TAPS = "constant time: about 1.5 s of filter at any output rate";

function e(
  family: FamilyId,
  length: LengthId | null,
  shape: string | null,
  phase: FilterPhase,
  taps: number | string,
  cutoff: CutoffId,
  label: string,
): Omit<PresetEntry, "name"> {
  return typeof taps === "number"
    ? { family, length, shape, phase, taps, cutoff, label }
    : { family, length, shape, phase, taps: null, lengthNote: taps, cutoff, label };
}

const L = "linear" as const;
const IP = "intermediate" as const;
const MP = "minimum" as const;

/** Every engine preset. `Record` makes the compiler insist on each name. */
const TABLE: Record<FilterPreset, Omit<PresetEntry, "name">> = {
  gaussian_short: e("gaussian", "short", "std", L, 127, "std", "Gaussian short"),
  gaussian_nyquist_short: e("gaussian", "short", "nyquist", L, 191, "nyquist", "Gaussian Nyquist short"),
  gaussian_compact: e("gaussian", "compact", "compact", L, COMPACT_TAPS, "compact", "Gaussian compact"),
  gaussian_medium: e("gaussian", "medium", "apod", L, 255, "apod", "Gaussian medium"),
  gaussian_early: e("gaussian", "medium", "early", L, 255, "early", "Gaussian early"),
  gaussian_early_ip: e("gaussian", "medium", "early", IP, 255, "early", "Gaussian early"),
  gaussian_early_mp: e("gaussian", "medium", "early", MP, 255, "early", "Gaussian early"),
  gaussian_long: e("gaussian", "long", "apod", L, 511, "apod", "Gaussian long"),
  gaussian_nyquist: e("gaussian", "long", "nyquist", L, 451, "nyquist", "Gaussian Nyquist"),
  gaussian_xlong: e("gaussian", "xlong", "wide", L, 4095, "wide", "Gaussian xlong"),
  gaussian_xlong_apod: e("gaussian", "xlong", "apod", L, 4095, "apod", "Gaussian xlong apod"),
  gaussian_huge: e("gaussian", "huge", "wide", L, HUGE_TAPS, "wide", "Gaussian huge"),
  gaussian_huge_apod: e("gaussian", "huge", "apod", L, HUGE_TAPS, "apod", "Gaussian huge apod"),

  flattop_short: e("flattop", "short", "std", L, 127, "std", "Flat-top short"),
  flattop_compact: e("flattop", "compact", "compact", L, COMPACT_TAPS, "compact", "Flat-top compact"),
  flattop_medium: e("flattop", "medium", "apod", L, 255, "apod", "Flat-top medium"),
  flattop_early: e("flattop", "medium", "early", L, 255, "early", "Flat-top early"),
  flattop_early_ip: e("flattop", "medium", "early", IP, 255, "early", "Flat-top early"),
  flattop_early_mp: e("flattop", "medium", "early", MP, 255, "early", "Flat-top early"),
  flattop: e("flattop", "long", "std", L, 511, "std", "Flat-top"),
  flattop_long: e("flattop", "long", "apod", L, 511, "apod", "Flat-top long"),
  flattop_xlong: e("flattop", "xlong", "wide", L, 4095, "wide", "Flat-top xlong"),
  flattop_xlong_std: e("flattop", "xlong", "std", L, 4095, "std", "Flat-top xlong std"),
  flattop_xlong_apod: e("flattop", "xlong", "apod", L, 4095, "apod", "Flat-top xlong apod"),
  flattop_huge: e("flattop", "huge", "std", L, HUGE_TAPS, "std", "Flat-top huge"),
  flattop_mega: e("flattop", "mega", "std", L, "2^20 / ratio − 1 taps per input sample (about 1 M taps at the output rate)", "std", "Flat-top mega"),

  chebyshev_short: e("chebyshev", "short", null, L, 71, "apod", "Chebyshev short"),
  chebyshev_short_mp: e("chebyshev", "short", null, MP, 71, "apod", "Chebyshev short"),
  chebyshev: e("chebyshev", "medium", null, L, 117, "std", "Chebyshev"),
  chebyshev_mp: e("chebyshev", "medium", null, MP, 117, "std", "Chebyshev"),
  chebyshev_long: e("chebyshev", "long", null, L, 207, "apod", "Chebyshev long"),
  chebyshev_long_ip: e("chebyshev", "long", null, IP, 207, "apod", "Chebyshev long"),
  chebyshev_long_mp: e("chebyshev", "long", null, MP, 207, "apod", "Chebyshev long"),

  kaiser_steep_short: e("kaiser", "short", null, L, 255, "steep_short", "Kaiser steep short"),
  kaiser_steep_short_mp: e("kaiser", "short", null, MP, 255, "steep_short", "Kaiser steep short"),
  kaiser_compact: e("kaiser", "compact", null, L, COMPACT_TAPS, "compact", "Kaiser compact"),
  kaiser_steep: e("kaiser", "long", null, L, 577, "steep", "Kaiser steep"),
  kaiser_steep_mp: e("kaiser", "long", null, MP, 577, "steep", "Kaiser steep"),

  halfband_tiny: e("halfband", "tiny", "chebyshev", L, 35, "nyquist", "Half-band tiny"),
  halfband_small: e("halfband", "small", "chebyshev", L, 63, "nyquist", "Half-band small"),
  halfband_medium: e("halfband", "medium", "chebyshev", L, 127, "nyquist", "Half-band medium"),
  halfband_large: e("halfband", "long", "chebyshev", L, 255, "nyquist", "Half-band large"),
  halfband_kaiser: e("halfband", "long", "kaiser", L, 255, "nyquist", "Half-band Kaiser"),

  lanczos23: e("lanczos", "a23", null, L, 47, "lanczos", "Lanczos 23"),
  lanczos23_mp: e("lanczos", "a23", null, MP, 47, "lanczos", "Lanczos 23"),
  lanczos511: e("lanczos", "a511", null, L, 1023, "lanczos", "Lanczos 511"),
  lanczos2047: e("lanczos", "a2047", null, L, 4095, "lanczos", "Lanczos 2047"),
  lanczos8191: e("lanczos", "a8191", null, L, 16383, "lanczos", "Lanczos 8191"),
  lanczos32767: e("lanczos", "a32767", null, L, 65535, "lanczos", "Lanczos 32767"),
  lanczos65535: e("lanczos", "a65535", null, L, 131071, "lanczos", "Lanczos 65535"),

  nuttall: e("nuttall", null, null, L, 117, "std", "Nuttall"),
  hann_nyquist: e("hann", null, null, L, 16383, "nyquist", "Hann Nyquist"),
  short_leaky: e("leaky", null, null, L, 47, "leaky", "Short leaky"),
  short_leaky_mp: e("leaky", null, null, MP, 47, "leaky", "Short leaky"),
  craven_apod: e("craven", null, null, MP, "the shortest that keeps 150 dB from Nyquist: about 239 taps per input sample at 44.1 kHz, shorter as the rate rises (its 20 kHz passband stays put)", "craven", "Craven apodizing"),

  cascade_fir: e("cascade", null, "fir", L, 255, "none", "Cascade FIR"),
  cascade_fir_asym: e("cascade", null, "fir", IP, "289 taps per ×2 stage, 292 per ×3 stage (cut where the tail falls below 2^-32)", "none", "Cascade FIR asym"),
  cascade_fir_mp: e("cascade", null, "fir", MP, 255, "none", "Cascade FIR"),
  cascade_fft: e("cascade", null, "fft", L, 512, "none", "Cascade FFT"),
  cascade_elliptic: e("cascade", null, "elliptic", MP, "order 16 (×2) or 17 (×3) recursive stages", "none", "Cascade elliptic"),
  cascade_inverse_chebyshev: e("cascade", null, "inverse_chebyshev", MP, "order 30 (×2) or 24 (×3) recursive stages", "none", "Cascade inverse Chebyshev"),

  nos: e("interp", null, "nos", L, 1, "none", "NOS (sample hold)"),
  cosine: e("interp", null, "cosine", L, 2, "none", "Cosine"),
  catmull_rom: e("interp", null, "catmull_rom", L, 4, "none", "Catmull-Rom"),
  table_sinc: e("interp", null, "table_sinc", L, 127, "none", "Table sinc"),

  block_fft: e("block", null, "standard", L, "16,384-sample blocks", "none", "Block FFT"),
  block_fft_fast: e("block", null, "fast", L, "16,384-sample blocks", "none", "Block FFT fast"),
  block_fft_large: e("block", null, "large", L, "2^20 / ratio-sample blocks", "none", "Block FFT large"),
};

/** All entries, in display order (family, then length). */
export const PRESETS: readonly PresetEntry[] = Object.entries(TABLE).map(([name, v]) => ({ name: name as FilterPreset, ...v }));

const BY_NAME = new Map(PRESETS.map((p) => [p.name, p]));

export function presetEntry(name: FilterPreset): PresetEntry | undefined {
  return BY_NAME.get(name);
}

export function familyPresets(family: FamilyId): PresetEntry[] {
  return PRESETS.filter((p) => p.family === family);
}

const LENGTH_ORDER = ["tiny", "small", "short", "compact", "medium", "long", "xlong", "huge", "mega"];
const LENGTH_LABEL: Record<string, string> = {
  tiny: "Tiny",
  small: "Small",
  short: "Short",
  compact: "Compact",
  medium: "Medium",
  long: "Long",
  xlong: "X-long",
  huge: "Huge",
  mega: "Mega",
};

function lengthRank(l: string): number {
  const i = LENGTH_ORDER.indexOf(l);
  return i >= 0 ? i : LENGTH_ORDER.length + Number(l.slice(1));
}

export function lengthLabel(l: string, family?: FamilyId): string {
  if (l.startsWith("a")) return l.slice(1);
  if (family === "halfband" && l === "long") return "Large";
  return LENGTH_LABEL[l] ?? l;
}

const PHASE_ORDER: FilterPhase[] = ["linear", "intermediate", "minimum"];
export const PHASE_LABEL: Record<FilterPhase, string> = { linear: "Linear", intermediate: "Intermediate", minimum: "Minimum" };

function valueOf(p: PresetEntry, dim: Dim): string | null {
  return dim === "length" ? p.length : dim === "shape" ? p.shape : p.phase;
}

/** A preset's value on one row (null: the family has no such row). */
export const presetDimValue = valueOf;

const DIMS: Dim[] = ["length", "shape", "phase"];

/** The values a family offers on one dimension, in display order. */
export function dimValues(family: FamilyId, dim: Dim): string[] {
  const vals = [...new Set(familyPresets(family).map((p) => valueOf(p, dim)).filter((v): v is string => v !== null))];
  if (dim === "length") return vals.sort((a, b) => lengthRank(a) - lengthRank(b));
  if (dim === "phase") return vals.sort((a, b) => PHASE_ORDER.indexOf(a as FilterPhase) - PHASE_ORDER.indexOf(b as FilterPhase));
  const order = Object.keys(familyInfo(family).shapes ?? {});
  return vals.sort((a, b) => order.indexOf(a) - order.indexOf(b));
}

/**
 * The dimensions a family shows as choices: a row only when, for some
 * choice of the rows before it, it has more than one value (Chebyshev's
 * roll-off follows from its length, so it is not a row of its own).
 */
export function familyDims(family: FamilyId): Dim[] {
  const list = familyPresets(family);
  return DIMS.filter((dim, i) => {
    const before = DIMS.slice(0, i);
    const groups = new Map<string, Set<string | null>>();
    for (const p of list) {
      const k = before.map((d) => valueOf(p, d)).join("|");
      if (!groups.has(k)) groups.set(k, new Set());
      groups.get(k)!.add(valueOf(p, dim));
    }
    return [...groups.values()].some((s) => s.size > 1);
  });
}

export function dimLabel(family: FamilyId, dim: Dim): string {
  const f = familyInfo(family);
  if (dim === "length") return f.lengthLabel ?? "Length";
  if (dim === "shape") return f.shapeLabel ?? "Shape";
  return "Phase";
}

export function dimValueLabel(family: FamilyId, dim: Dim, value: string): string {
  if (dim === "length") return lengthLabel(value, family);
  if (dim === "phase") return PHASE_LABEL[value as FilterPhase] ?? value;
  return familyInfo(family).shapes?.[value]?.label ?? value;
}

// ------------------------------------------------------------ ratio rules

/**
 * Why `name` cannot run at `ratio` ("a power-of-two ratio"), or null.
 * Mirrors `check_rate_ratio`, with the engine's kernel limit
 * (`status.limits.max_kernel_taps`, output-rate taps).
 */
export function presetRatioNeed(name: FilterPreset, ratio: RateRatio, maxKernelTaps = FALLBACK_LIMITS.max_kernel_taps): string | null {
  const p = presetEntry(name);
  if (!p || ratio.up === ratio.down) return null;
  const runtime: Runtime = p.family === "cascade" ? "cascade" : p.family === "interp" ? "interp" : p.family === "block" ? "block" : "fir";
  if (ratio.down === 1) {
    const r = ratio.up;
    if (r < 2 || r > 64) return "a ratio from 2 to 64";
    // The engine's kernel limit at the output rate: only lanczos65535
    // reaches it (above ×32 with 4,194,304 taps; within 8,388,608 up to ×64).
    if (runtime === "fir" && typeof p.taps === "number" && p.taps * r > maxKernelTaps) {
      return `a ratio of at most ×${Math.floor(maxKernelTaps / p.taps)} (its kernel would be longer than ${maxKernelTaps.toLocaleString("en-US")} taps)`;
    }
    if (runtime === "cascade") {
      let m = r;
      while (m % 3 === 0) m /= 3;
      return r % 2 === 0 && (m & (m - 1)) === 0 ? null : "an even ratio made of factors 2 and 3";
    }
    if (runtime === "block") return (r & (r - 1)) === 0 ? null : "a power-of-two ratio";
    return null;
  }
  const cls = rationalClass(p, runtime);
  if (cls === "integer") return "an integer upsampling ratio";
  if (cls === "up" && ratio.up < ratio.down) return "an output rate at or above the source rate";
  return null;
}

const INTEGER_ONLY = new Set<FilterPreset>(["gaussian_huge", "gaussian_huge_apod", "flattop_xlong_std", "flattop_huge", "flattop_mega", "lanczos32767", "lanczos65535"]);
const UPSAMPLING_ONLY = new Set<FilterPreset>([
  "gaussian_compact",
  "flattop_compact",
  "kaiser_compact",
  "lanczos23",
  "lanczos23_mp",
  "lanczos511",
  "lanczos2047",
  "lanczos8191",
  "nuttall",
  "short_leaky",
  "short_leaky_mp",
  "halfband_kaiser",
  "halfband_large",
  "halfband_medium",
  "halfband_small",
  "halfband_tiny",
  "hann_nyquist",
]);

function rationalClass(p: PresetEntry, runtime: Runtime): RationalClass {
  if (runtime !== "fir" || INTEGER_ONLY.has(p.name)) return "integer";
  return UPSAMPLING_ONLY.has(p.name) ? "up" : "any";
}

const khz = (r: number) => `${Number((r / 1000).toFixed(1))} kHz`;
const ratioStr = (r: RateRatio) => (r.down === 1 ? `×${r.up}` : `${r.up}/${r.down}`);

/** Rational runtime limits: `MAX_RATIONAL_GRID_TAPS` and `MAX_RATIONAL_MACS_PER_SECOND` (per channel). */
export const RATIONAL_MAX_GRID_TAPS = 1_048_576;
export const RATIONAL_MAX_MACS = 800e6;
/** Pipelines that can run at once (`rational_pipelines`): a swap runs two; A/B doubles it. */
export const RATIONAL_PIPELINES = 2;
export const rationalPipelines = (ab: boolean) => RATIONAL_PIPELINES * (ab ? 2 : 1);

/** `rational::decimation_rule`: the base length that replaces the recipe's, and how an even length is made odd. */
function decimationRule(name: FilterPreset, taps: number): [number, "down" | "up"] {
  if (name === "chebyshev" || name === "chebyshev_mp") return [191, "up"];
  if (name === "chebyshev_short" || name === "chebyshev_short_mp") return [127, "up"];
  if (name.startsWith("kaiser_steep")) return [taps, "up"];
  return [taps, "down"];
}

/**
 * Taps per input sample of a single-kernel preset at the non-integer `r`
 * from `rate`, like `rational::recipe`: the recipe at `L = r.up`, and when
 * decimating `ceil(base × M / L)`, made odd by the family's rule.
 */
export function presetRationalTaps(name: FilterPreset, rate: number, r: RateRatio): number {
  let t = presetTapsPerInput(name, rate, r.up, rate * r.up);
  if (r.up < r.down) {
    const [base, odd] = decimationRule(name, t);
    t = Math.ceil((base * r.down) / r.up);
    if (t % 2 === 0) t = odd === "down" ? t - 1 : t + 1;
  }
  return t;
}

/**
 * Length and cost of a preset on the rational runtime from `rate` at the
 * fractional or decimating `r`, as `rational::grid_taps` and
 * `check_rational_budget` compute them: `taps × L` grid taps (four times
 * that for intermediate phase), and the output rate times the taps per
 * branch times the pipelines that can run at once.
 */
export function presetRationalCost(name: FilterPreset, rate: number, r: RateRatio, pipelines = RATIONAL_PIPELINES): { gridTaps: number; macs: number } {
  const n = presetRationalTaps(name, rate, r) * r.up;
  const gridTaps = presetEntry(name)?.phase === "intermediate" ? 4 * n : n;
  const outRate = (rate * r.up) / r.down;
  return { gridTaps, macs: outRate * Math.ceil(gridTaps / Math.max(1, r.up)) * Math.max(1, pipelines) };
}

/**
 * Why the daemon would refuse `name` with this rate target, or null. Like
 * `validate_upsampler`: integer ratios are checked at every engine rate,
 * fractional and decimating ones (ratio rules, then the rational length and
 * cost limits with `pipelines`, 4 with A/B) only at the allowed rates.
 */
export function presetBlockReason(name: FilterPreset, target: RateTarget, allowedRates: readonly number[], pipelines = RATIONAL_PIPELINES, maxKernelTaps = FALLBACK_LIMITS.max_kernel_taps): string | null {
  for (const rate of ALLOWED_SOURCE_RATES) {
    const r = resolveRateRatio(target, rate);
    if (r.up === r.down) continue;
    if (r.down !== 1 && !allowedRates.includes(rate)) continue;
    // Refused for its terms whatever the preset: the term note says so.
    if (r.down !== 1 && pastTermLimit(r)) continue;
    const need = presetRatioNeed(name, r, maxKernelTaps);
    if (need) return `Not with this rate setting: from ${khz(rate)} it would resample by ${ratioStr(r)}, and this preset needs ${need}.`;
    if (r.down === 1) continue;
    const { gridTaps, macs } = presetRationalCost(name, rate, r, pipelines);
    if (gridTaps > RATIONAL_MAX_GRID_TAPS || macs > RATIONAL_MAX_MACS) {
      const out = Math.round((rate * r.up) / r.down);
      const ab = pipelines > RATIONAL_PIPELINES ? " with A/B on" : "";
      return `Too long for this rate setting${ab}: from ${khz(rate)} to ${khz(out)} (${ratioStr(r)}) it is more than a fractional conversion can run. Use a shorter preset, or turn ${khz(rate)} off on the Device page.`;
    }
  }
  return null;
}

// ------------------------------------------------------------ picking

export interface DimOption {
  value: string;
  label: string;
  tip: string;
  /** The preset a click selects, or null when the option is disabled. */
  pick: FilterPreset | null;
}

/** Plain description of one preset: length, roll-off, phase. */
export function presetDetail(p: PresetEntry): string {
  const len =
    p.taps !== null
      ? p.family === "interp" && p.taps <= 4
        ? `${p.taps} input sample${p.taps === 1 ? "" : "s"} of support`
        : `${p.taps.toLocaleString("en-US")} taps per input sample (${fmtMsShort((p.taps / 44100) * 1000)} at 44.1 kHz)`
      : (p.lengthNote ?? "");
  const cut = CUTOFF[p.cutoff].text;
  const phase =
    p.phase === "linear"
      ? "linear phase (symmetric ringing, adds delay)"
      : p.phase === "minimum"
        ? "minimum phase (no pre-ringing, little delay)"
        : p.name === "cascade_fir_asym"
          ? "asymmetric (between linear and minimum phase)"
          : "intermediate phase (mostly linear, some pre-ringing moved after the peak)";
  return [len, cut, phase].filter(Boolean).join("; ") + ".";
}

function fmtMsShort(ms: number): string {
  return ms >= 1000 ? `${(ms / 1000).toFixed(2)} s` : ms >= 10 ? `${ms.toFixed(0)} ms` : `${ms.toFixed(1)} ms`;
}

/**
 * The options of one row for the current preset. Rows are hierarchical:
 * a row's options keep the rows above it, and a click keeps as many of the
 * rows below as exist (else the first that does, linear phase first).
 * `blocked` says why a preset cannot run now (rate rules), or null.
 */
export function dimOptions(current: PresetEntry, dim: Dim, blocked: (name: FilterPreset) => string | null): DimOption[] {
  const family = current.family;
  const list = familyPresets(family);
  const idx = DIMS.indexOf(dim);
  const above = DIMS.slice(0, idx);
  const below = DIMS.slice(idx + 1);
  return dimValues(family, dim).map((value) => {
    const label = dimValueLabel(family, dim, value);
    const cands = list.filter((p) => valueOf(p, dim) === value && above.every((d) => valueOf(p, d) === valueOf(current, d)));
    if (cands.length === 0) {
      const where = above
        .map((d) => valueOf(current, d))
        .map((v, i) => (v === null ? "" : dimValueLabel(family, above[i], v)))
        .filter(Boolean)
        .join(" ");
      return { value, label, tip: `There is no ${where ? where.toLowerCase() + " " : ""}${familyInfo(family).label} preset with this ${dimLabel(family, dim).toLowerCase()}.`, pick: null };
    }
    const score = (p: PresetEntry) => below.reduce((s, d, i) => s + (valueOf(p, d) === valueOf(current, d) ? 2 ** (below.length - i) : 0), 0) + (p.phase === "linear" ? 0.5 : 0);
    const ranked = [...cands].sort((a, b) => score(b) - score(a));
    const ok = ranked.find((p) => blocked(p.name) === null);
    const best = ok ?? ranked[0];
    const why = ok ? null : blocked(best.name);
    const head = dim === "shape" ? (familyInfo(family).shapes?.[value]?.tip ?? "") : "";
    const tip = [head, `${best.label}: ${presetDetail(best)}`, why].filter(Boolean).join(" ");
    return { value, label, tip, pick: why ? null : best.name };
  });
}

/** The preset a family click selects: the one remembered for it, else its default, else its first that can run. */
export function familyPick(family: FamilyId, remembered: FilterPreset | null | undefined, blocked: (name: FilterPreset) => string | null): FilterPreset | null {
  const list = familyPresets(family);
  const order = [remembered, familyInfo(family).default, ...list.map((p) => p.name)].filter((n): n is FilterPreset => !!n && presetEntry(n)?.family === family);
  return order.find((n) => blocked(n) === null) ?? null;
}

/** Short summary text: "Flat-top xlong apod · linear". */
export function presetSummary(name: FilterPreset): string {
  const p = presetEntry(name);
  if (!p) return name.replace(/_/g, " ");
  return `${p.label} · ${p.phase}`;
}

/** A preset's −6 dB cutoff as a fraction of the source Nyquist at `sourceRate` (1 when it has none of its own). */
export function presetCutoffFraction(name: FilterPreset, sourceRate: number): number {
  const p = presetEntry(name);
  if (!p) return 1;
  if (p.cutoff === "compact") return sourceRate < 50000 ? 0.88 : sourceRate < 100000 ? 0.725 : 0.6;
  // The apodizing band is flat to 20 kHz at every rate: the -6 dB point sits
  // midway between it and the Nyquist.
  if (p.cutoff === "craven") return (20000 / (sourceRate / 2) + 1) / 2;
  return CUTOFF[p.cutoff].fraction ?? 1;
}

/**
 * Taps per input sample of a preset at `sourceRate` upsampled by `up`
 * (README "Conventions"; the recursive and block presets get a nominal
 * length). For the mock's estimates and the rational cost check.
 */
export function presetTapsPerInput(name: FilterPreset, sourceRate: number, up: number, outputRate: number): number {
  const p = presetEntry(name);
  if (!p) return 255;
  if (name === "cascade_fir_asym") return 290;
  if (p.taps !== null) return p.taps;
  if (p.length === "compact") return sourceRate < 50000 ? 167 : sourceRate < 100000 ? 83 : 63;
  if (p.length === "huge") {
    // `constant_time_length` of the rate the recipe is built for, over L, minus one.
    const nRate = outputRate < 50000 ? 65536 : outputRate < 100000 ? 131072 : outputRate < 250000 ? 262144 : outputRate < 500000 ? 524288 : outputRate < 1e6 ? 1048576 : outputRate < 2e6 ? 2097152 : 4194304;
    return Math.max(1, Math.floor(nRate / Math.max(1, up)) - 1);
  }
  if (p.length === "mega") return Math.max(1, Math.floor(2 ** 20 / Math.max(1, up)) - 1);
  if (p.family === "block") return p.shape === "large" ? Math.floor(2 ** 20 / Math.max(1, up)) : 16384;
  return 64;
}
