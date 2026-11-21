// Upsampler design limits, mirrored from dsp-core so the UI can disable an
// option the daemon would reject (INVALID_PARAMS) instead of failing on
// click. Keep in sync with `crates/dsp-core/src/resample.rs`
// (`nyquist_stop_*`), `crates/dsp-core/src/minphase.rs`
// (`MIN/MAX_MINIMUM_PHASE_BETA`) and `validate_upsampler` in the daemon; the
// shared `NYQUIST_STOP_MIN_TAPS_CASES` below are asserted on both sides.

import type { BandSpec } from "../api/generated/BandSpec";
import type { CutoffMode } from "../api/generated/CutoffMode";
import type { FilterPhase } from "../api/generated/FilterPhase";
import type { RateTarget } from "../api/generated/RateTarget";
import type { RateRatio } from "../api/generated/RateRatio";
import type { TapsUnit } from "../api/generated/TapsUnit";
import type { WindowKind } from "../api/generated/WindowKind";
import { pastTermLimit, resolveRateRatio } from "./upsamplerRate";
import { FALLBACK_LIMITS } from "./engineLimits";

/** Source rates the engine runs at (daemon `ALLOWED_RATES`). */
export const ALLOWED_SOURCE_RATES = [44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000] as const;

/** Kaiser beta range for minimum phase and for "Stop at Nyquist". */
export const MIN_PHASE_BETA = [2, 20] as const;
export const NYQUIST_STOP_BETA = [5, 24] as const;
/** Smallest "Stop at Nyquist" cutoff (fraction of the source Nyquist). */
export const NYQUIST_STOP_MIN_CUTOFF = 0.5;
const SHORT_GAIN = 0.7;

/** (ratio, beta, fewest feasible taps): also asserted in the Rust tests. */
export const NYQUIST_STOP_MIN_TAPS_CASES: [number, number, number][] = [
  [2, 8, 32],
  [16, 14, 433],
  [32, 20, 1254],
  [32, 24, 1653],
  [16, 5, 157],
];

export function kaiserAttenuationDb(beta: number): number {
  return beta / 0.1102 + 8.7;
}

function safetyK(beta: number): number {
  return Math.max(1.19, 1.21 + 0.03 * (beta - 20));
}

function rawCutoff(taps: number, ratio: number, beta: number): number {
  const x = (ratio * Math.max(0, kaiserAttenuationDb(beta) - 7.95)) / (14.36 * (Math.max(taps, 2) - 1));
  const y = safetyK(beta) * x;
  return 1 - y * (1 + SHORT_GAIN * y);
}

export function resolveRatio(target: RateTarget, sourceRate: number): number {
  if (target === "off") return 1;
  if ("ratio" in target) return Math.max(1, target.ratio);
  // Kernel length grows with the grid; an explicit rate's integer part is
  // the closest integer rule (fractional limits are the daemon's).
  if ("rate" in target) return Math.max(1, Math.round(target.rate / sourceRate));
  const cap = "max_rate" in target ? target.max_rate : target.max_rate_any;
  let r = 1;
  while (sourceRate * r * 2 <= cap) r *= 2;
  return r;
}

/** Largest integer ratio a target reaches over the engine rates (the daemon's `worst_case_ratio`). */
export function worstCaseRatio(target: RateTarget): number {
  return Math.max(1, ...ALLOWED_SOURCE_RATES.map((rate) => resolveRateRatio(target, rate)).filter((r) => r.down === 1).map((r) => r.up));
}

const inRange = (beta: number, [lo, hi]: readonly [number, number]) => beta >= lo && beta <= hi;

/** Fewest taps for which "Stop at Nyquist" is feasible, or null (beta out of range). */
export function nyquistStopMinTaps(ratio: number, beta: number): number | null {
  if (!inRange(beta, NYQUIST_STOP_BETA)) return null;
  if (ratio <= 1) return 1;
  let taps = 2;
  // Bisect on the monotone cutoff(taps).
  let hi = 2;
  while (rawCutoff(hi, ratio, beta) < NYQUIST_STOP_MIN_CUTOFF) hi *= 2;
  let lo = Math.max(2, Math.floor(hi / 2));
  if (rawCutoff(lo, ratio, beta) >= NYQUIST_STOP_MIN_CUTOFF) return lo;
  while (hi - lo > 1) {
    const mid = Math.floor((lo + hi) / 2);
    if (rawCutoff(mid, ratio, beta) >= NYQUIST_STOP_MIN_CUTOFF) hi = mid;
    else lo = mid;
  }
  taps = hi;
  return taps;
}

/** The "Stop at Nyquist" cutoff dsp-core uses (clamped like `nyquist_stop_cutoff`). */
export function nyquistStopCutoff(taps: number, ratio: number, beta: number): number {
  if (ratio <= 1) return 1;
  if (taps <= 1) return NYQUIST_STOP_MIN_CUTOFF;
  const b = Math.min(NYQUIST_STOP_BETA[1], Math.max(NYQUIST_STOP_BETA[0], beta));
  return Math.min(1, Math.max(NYQUIST_STOP_MIN_CUTOFF, rawCutoff(taps, ratio, b)));
}

// ------------------------------------------------------------ Stop at Nyquist per window

/** Complementary error function (dsp-core `window::erfc`). */
export function erfc(x: number): number {
  if (x < 0) return 2 - erfc(-x);
  if (x < 2) {
    const x2 = x * x;
    let term = x;
    let sum = x;
    for (let n = 1; n < 200; n++) {
      term *= -x2 / n;
      const add = term / (2 * n + 1);
      sum += add;
      if (Math.abs(add) < 1e-17 * Math.abs(sum)) break;
    }
    return 1 - (2 / Math.sqrt(Math.PI)) * sum;
  }
  const tiny = 1e-300;
  let f = x;
  let c = x;
  let d = 0;
  for (let n = 1; n < 500; n++) {
    const a = n / 2;
    d = x + a * d;
    if (Math.abs(d) < tiny) d = tiny;
    c = x + a / c;
    if (Math.abs(c) < tiny) c = tiny;
    d = 1 / d;
    const delta = c * d;
    f *= delta;
    if (Math.abs(delta - 1) < 1e-16) break;
  }
  return Math.exp(-x * x) / Math.sqrt(Math.PI) / f;
}

/** Inverse of `erfc` for 0 < y < 1 (dsp-core `window::erfc_inv`). */
export function erfcInv(y: number): number {
  const lnY = Math.log(y);
  let x = Math.max(0.1, Math.sqrt(-lnY));
  for (let i = 0; i < 60; i++) {
    const e = erfc(x);
    const g = Math.log(e) - lnY;
    const dg = (-2 * Math.exp(-x * x)) / (Math.sqrt(Math.PI) * e);
    const step = g / dg;
    x -= step;
    if (Math.abs(step) < 1e-14 * Math.max(1, Math.abs(x))) break;
  }
  return x;
}

/** dsp-core `gaussian_half_width`: the Gaussian window's half transition, fraction of the source Nyquist. */
function gaussianHalfWidth(taps: number, ratio: number, aDb: number): number {
  const half = (Math.max(taps, 2) - 1) / 2;
  const depth = ((aDb + 10) / 20) * Math.LN10;
  const sigma = half / Math.sqrt(2 * depth);
  const delta = Math.pow(10, -aDb / 20);
  const xCycles = (Math.SQRT2 * erfcInv(2 * delta)) / (2 * Math.PI * sigma);
  return 1.02 * xCycles * 2 * ratio;
}

/** dsp-core `half_transition`: Kaiser (and Tukey) by the calibrated formula, Gaussian by its own. */
export function halfTransition(window: WindowKind, taps: number, ratio: number, beta: number): number {
  if (window === "gaussian") return gaussianHalfWidth(taps, ratio, kaiserAttenuationDb(beta));
  return 1 - rawCutoff(taps, ratio, beta);
}

/** dsp-core `nyquist_stop_min_taps_for`: fewest taps for "Stop at Nyquist" with this window, or null (β out of range). */
export function nyquistStopMinTapsFor(window: WindowKind, ratio: number, beta: number): number | null {
  if (window === "kaiser") return nyquistStopMinTaps(ratio, beta);
  if (!inRange(beta, NYQUIST_STOP_BETA)) return null;
  if (ratio <= 1) return 1;
  const w = 1 - NYQUIST_STOP_MIN_CUTOFF;
  const fits = (t: number) => halfTransition(window, t, ratio, beta) <= w;
  if (fits(2)) return 2;
  let hi = 4;
  while (!fits(hi)) {
    if (hi >= 2 ** 40) return Number.MAX_SAFE_INTEGER;
    hi *= 2;
  }
  let lo = hi / 2;
  while (hi - lo > 1) {
    const mid = lo + Math.floor((hi - lo) / 2);
    if (fits(mid)) hi = mid;
    else lo = mid;
  }
  return hi;
}

/** dsp-core `nyquist_stop_check_for` on a design of `taps` at `ratio`: null when feasible, else what is wrong. */
function nyquistStopCheck(window: WindowKind, taps: number, ratio: number, beta: number): { beta: true } | { minTaps: number } | null {
  if (ratio <= 1) return null;
  if (!inRange(beta, NYQUIST_STOP_BETA)) return { beta: true };
  const cutoff = 1 - halfTransition(window, taps, ratio, beta);
  if (taps < 2 || cutoff < NYQUIST_STOP_MIN_CUTOFF) return { minTaps: nyquistStopMinTapsFor(window, ratio, beta) ?? Number.MAX_SAFE_INTEGER };
  return null;
}

const khz = (r: number) => `${Number((r / 1000).toFixed(1))} kHz`;

/**
 * Why "Stop at Nyquist" is not available for these settings, or null. Like
 * the daemon: at the largest integer ratio with `manual_taps` (input-sample
 * lengths times the ratio), and at every fractional or decimating ratio of
 * the allowed rates on its grid (`manual_taps_for`, design ratio
 * `max(L, M)`), with the window's own rule.
 */
export function nyquistStopBlockReason(
  u: { target: RateTarget; taps: number; kaiser_beta: number; taps_unit?: TapsUnit; window?: WindowKind },
  allowedRates: readonly number[] = [],
): string | null {
  const window = u.window ?? "kaiser";
  const input = u.taps_unit === "input";
  const [lo, hi] = NYQUIST_STOP_BETA;
  const betaText = `Stop at Nyquist needs a Kaiser β between ${lo} and ${hi} (now ${u.kaiser_beta}).`;
  const win = window === "gaussian" ? " with the Gaussian window" : "";
  const ratio = worstCaseRatio(u.target);
  if (ratio > 1) {
    const taps = input ? u.taps * ratio : u.taps;
    const bad = nyquistStopCheck(window, taps, ratio, u.kaiser_beta);
    if (bad && "beta" in bad) return betaText;
    if (bad) {
      const now = input ? `${u.taps.toLocaleString("en-US")} per input sample, ${taps.toLocaleString("en-US")} taps` : taps.toLocaleString("en-US");
      return `Stop at Nyquist is only guaranteed from ${bad.minTaps.toLocaleString("en-US")} taps at ×${ratio}${win} (now ${now}): below that the calibrated cutoff formula cannot promise the stopband at the source Nyquist.`;
    }
  }
  for (const rate of ALLOWED_SOURCE_RATES) {
    if (!allowedRates.includes(rate)) continue;
    const r = resolveRateRatio(u.target, rate);
    if (r.down === 1) continue;
    const grid = u.taps * (input ? r.up : r.down);
    const design = Math.max(r.up, r.down);
    const bad = nyquistStopCheck(window, grid, design, u.kaiser_beta);
    if (bad && "beta" in bad) return betaText;
    if (bad) {
      const need = Math.ceil(bad.minTaps / (input ? r.up : r.down));
      return `Stop at Nyquist is only guaranteed from ${need.toLocaleString("en-US")} taps${input ? " per input sample" : ""} at ${khz(rate)} (${r.up}/${r.down})${win} (now ${u.taps.toLocaleString("en-US")}): below that the calibrated cutoff formula cannot promise the stopband at the source Nyquist.`;
    }
  }
  return null;
}

/** Why minimum phase is not available for this beta, or null. */
export function minimumPhaseBlockReason(beta: number): string | null {
  const [lo, hi] = MIN_PHASE_BETA;
  return inRange(beta, MIN_PHASE_BETA) ? null : `Minimum phase needs a Kaiser β between ${lo} and ${hi} (now ${beta}).`;
}

export interface KaiserBetaPreset {
  readonly beta: number;
  /** Where it sits on the steep ↔ deep scale, e.g. "Deep". */
  readonly name: string;
  /** Button text: "β 17 · Deep". */
  readonly label: string;
}

export const DEFAULT_KAISER_BETA = 14;

const preset = (beta: number, name: string): KaiserBetaPreset => ({ beta, name, label: `β ${beta} · ${name}` });

/** Ordered from the steepest transition (lowest β) to the deepest stopband (highest β). */
export const KAISER_BETA_PRESETS: readonly KaiserBetaPreset[] = [
  preset(10, "Steep"),
  preset(14, "Standard"),
  preset(17, "Deep"),
  preset(20, "Deeper"),
  // Linear phase only: minimum phase is designed for β 2..20 (blocked through
  // `kaiserBetaBlockReason`); 24 is also the top of the Stop at Nyquist range.
  preset(24, "Deepest"),
];

/** One-line trade-off shown under the presets. */
export const KAISER_BETA_HINT = "Lower β: steeper transition, less stopband rejection. Higher β: deeper rejection, wider transition.";

/**
 * What the β control means with each window (the wire field stays
 * `kaiser_beta`). Kaiser: the window's own β. Gaussian: β picks the target
 * stopband (Kaiser's attenuation for that β) and the Gaussian is narrowed
 * until it is 10 dB below it at the kernel's ends, so a higher β is a
 * narrower window. Tukey: the taper shapes the window; β only sets the level
 * the reported stopband edge and depth are measured against.
 */
export function betaControl(window: WindowKind): { label: string; title: string; hint: string | null } {
  switch (window) {
    case "gaussian":
      return {
        label: "Gaussian width (β)",
        title:
          "How narrow the Gaussian window is. β sets the stopband the filter aims for (the same depth as a Kaiser window with this β); the window is narrowed until it has faded 10 dB below that at the ends. Higher β: narrower window, deeper rejection, wider transition.",
        hint: KAISER_BETA_HINT,
      };
    case "tukey":
      return {
        label: "Stopband reference (β)",
        title:
          "With the Tukey window β does not change the filter: the taper shapes it. β only sets the level the stopband edge and depth below are measured against.",
        hint: null,
      };
    default:
      return {
        label: "Filter shape (Kaiser β)",
        title: "The Kaiser window's β: it trades the transition width against the stopband depth.",
        hint: KAISER_BETA_HINT,
      };
  }
}

/** Matching preset β, or null when a custom β (e.g. from CLI) is active. */
export function selectedKaiserPreset(beta: number): number | null {
  if (!Number.isFinite(beta)) return null;
  const hit = KAISER_BETA_PRESETS.find((p) => Math.abs(p.beta - beta) < 1e-9);
  return hit ? hit.beta : null;
}

/** Plain-language tooltip for a Kaiser β preset, computed from `kaiserAttenuationDb`. */
/**
 * A β option's tooltip with the Tukey window, where β does not shape the
 * filter: it only sets the level the stopband edge and depth are measured
 * against.
 */
export function tukeyReferenceTooltip(beta: number): string {
  return `Measure the stopband against ≈ ${Math.round(kaiserAttenuationDb(beta))} dB (β = ${beta}). The filter itself does not change: with the Tukey window the taper shapes it.`;
}

/** The β row's readout with the Tukey window: only the measurement reference. */
export function tukeyReferenceReadout(beta: number): string {
  return `Stopband edge and depth are measured against ≈ ${kaiserAttenuationDb(beta).toFixed(1)} dB.`;
}

export function kaiserPresetTooltip(beta: number, name: string): string {
  const db = Math.round(kaiserAttenuationDb(beta));
  const ref = DEFAULT_KAISER_BETA;
  const audibleRefDb = Math.round(kaiserAttenuationDb(ref));
  const trade =
    beta < ref
      ? `steeper transition from passband to stopband than β ${ref} for the same tap count, trading a shallower stopband`
      : beta === ref
        ? "default balance of transition width and stopband depth"
        : `deeper stopband than β ${ref}, against a wider transition from passband to stopband for the same tap count`;
  const def = beta === ref ? ", default" : "";
  return `${name} (β = ${beta}, ≈ ${db} dB stopband${def}): ${trade}. Past about ${audibleRefDb} dB the images already sit below the analog noise of real DACs (typically 110–130 dB), so deeper settings are measurable rather than audible (ideal 24-bit quantization is about 146 dB).`;
}

/** Why a candidate Kaiser β would make the current phase or cutoff mode invalid, or null. */
export function kaiserBetaBlockReason(
  u: { target: RateTarget; taps: number; phase: FilterPhase; cutoff_mode: CutoffMode; taps_unit?: TapsUnit; window?: WindowKind },
  beta: number,
  allowedRates: readonly number[] = [],
): string | null {
  const reasons: string[] = [];
  if (u.phase === "minimum") {
    const r = minimumPhaseBlockReason(beta);
    if (r) reasons.push(r);
  }
  if (u.cutoff_mode === "nyquist_stop") {
    const r = nyquistStopBlockReason({ ...u, kaiser_beta: beta }, allowedRates);
    if (r) reasons.push(r);
  }
  return reasons.length > 0 ? reasons.join(" ") : null;
}

/** Kaiser empirical transition width Δf in Hz for `taps` at `outputRate`. */
export function kaiserTransitionWidthHz(beta: number, taps: number, outputRate: number): number | null {
  if (!Number.isFinite(beta) || !Number.isFinite(taps) || !Number.isFinite(outputRate)) return null;
  if (taps <= 1 || outputRate <= 0) return null;
  const atten = kaiserAttenuationDb(beta);
  return (Math.max(0, atten - 7.95) / (14.36 * (taps - 1))) * outputRate;
}

/** What the connected engine's upsampler config has: an older engine leaves fields out. */
export interface UpsamplerFeatures {
  preset: boolean;
  auto: boolean;
  band: boolean;
  phaseMix: boolean;
  window: boolean;
  preserve: boolean;
  tapsUnit: boolean;
}

export function upsamplerFeatures(u: object): UpsamplerFeatures {
  const has = (k: string) => k in u && (u as Record<string, unknown>)[k] !== undefined;
  return {
    preset: has("preset"),
    auto: has("auto_taps"),
    band: has("band"),
    phaseMix: has("phase_mix"),
    window: has("window"),
    preserve: has("preserve_samples"),
    tapsUnit: has("taps_unit"),
  };
}

/** Tooltip for a control the connected engine does not have yet. */
export const ENGINE_UPDATE_TIP = "The connected engine does not have this yet: update the engine to use it.";

/** Deepest band attenuation minimum and intermediate phase support, dB. */
export const MIN_PHASE_MAX_ATTEN_DB = 190.2;

// ------------------------------------------------------------ manual designs at a rational ratio

export const RATIONAL_MAX_GRID = 1_048_576;
export const RATIONAL_MAX_MACS_PER_S = 800e6;

/**
 * Daemon `max_rational_taps_per_input`: the most taps per input sample that
 * fit `r` from `rate` (never more than the engine's kernel limit).
 */
export function maxRationalTapsPerInput(rate: number, r: RateRatio, pipelines: number, intermediate: boolean, maxKernelTaps = FALLBACK_LIMITS.max_kernel_taps): number {
  const growth = intermediate ? 4 : 1;
  const bySize = RATIONAL_MAX_GRID / r.up;
  const byCost = RATIONAL_MAX_MACS_PER_S / (((rate * r.up) / r.down) * Math.max(1, pipelines));
  return Math.floor(Math.min(maxKernelTaps, Math.min(bySize, byCost) / growth));
}

export interface RationalProblem {
  rate: number;
  ratio: RateRatio;
  gridTaps: number;
  /** Longest length per input sample that fits there (0: none). */
  maxPerInput: number;
  text: string;
}

/**
 * The first allowed rate at which a manual Fraction or Stop-at-Nyquist
 * design is too long or too costly for the rational runtime, like the
 * daemon's `check_rational_at` (grid length `manual_taps_for`, ×4 for a
 * mixed intermediate phase, `check_rational_budget` with `pipelines`).
 * Automatic and band designs pick their own length: null for them.
 */
export function manualRationalProblem(
  u: { target: RateTarget; taps: number; taps_unit: TapsUnit; phase: FilterPhase; phase_mix: number; cutoff_mode: CutoffMode; auto_taps: unknown; preset?: unknown },
  allowedRates: readonly number[],
  pipelines: number,
): RationalProblem | null {
  if (u.preset || u.auto_taps || u.cutoff_mode === "band") return null;
  const mixed = u.phase === "intermediate" && u.phase_mix > 0 && u.phase_mix < 1;
  for (const rate of ALLOWED_SOURCE_RATES) {
    if (!allowedRates.includes(rate)) continue;
    const r = resolveRateRatio(u.target, rate);
    // A ratio past the term limit is refused outright: the term note says so.
    if (r.down === 1 || pastTermLimit(r)) continue;
    const design = u.taps * (u.taps_unit === "input" ? r.up : r.down);
    const grid = mixed ? design * 4 : design;
    const macs = ((rate * r.up) / r.down) * Math.ceil(grid / r.up) * Math.max(1, pipelines);
    if (design > RATIONAL_MAX_GRID || grid > RATIONAL_MAX_GRID || macs > RATIONAL_MAX_MACS_PER_S) {
      const maxPerInput = maxRationalTapsPerInput(rate, r, pipelines, mixed);
      const per = u.taps_unit === "input" ? " per input sample" : "";
      return {
        rate,
        ratio: r,
        gridTaps: grid,
        maxPerInput,
        text: `${u.taps.toLocaleString("en-US")} taps${per} is too long for ${khz(rate)} → ${khz((rate * r.up) / r.down)} (${r.up}/${r.down}): a fractional conversion takes at most ${maxPerInput.toLocaleString("en-US")} taps per input sample there.`,
      };
    }
  }
  return null;
}

/** A length that fits the fractional limits, or why none does. */
export type RationalFix = { taps_unit: "input"; taps: number } | { why: string };

/**
 * The one-click fix when a manual design does not fit `target` (a
 * fractional rate): count the length in input samples, keeping the
 * duration it has now (output taps over the current integer ratio) as far
 * as every allowed rate's limit allows, and, with Stop at Nyquist, at least
 * the window's minimum at every allowed rate. Null when the design already
 * fits; `why` when no length meets both.
 */
export function rationalLengthFix(
  u: { target: RateTarget; taps: number; taps_unit: TapsUnit; phase: FilterPhase; phase_mix: number; cutoff_mode: CutoffMode; auto_taps: unknown; preset?: unknown; kaiser_beta: number; window?: WindowKind },
  target: RateTarget,
  allowedRates: readonly number[],
  pipelines: number,
  maxKernelTaps = FALLBACK_LIMITS.max_kernel_taps,
): RationalFix | null {
  if (!manualRationalProblem({ ...u, target }, allowedRates, pipelines)) return null;
  const at = (taps: number) => ({ ...u, target, taps_unit: "input" as const, taps });
  // The longest length per input sample that fits every allowed rate.
  let max = maxKernelTaps;
  for (let i = 0; i < ALLOWED_SOURCE_RATES.length + 1 && max >= 1; i++) {
    const p = manualRationalProblem(at(max), allowedRates, pipelines);
    if (!p) break;
    max = p.maxPerInput < 1 ? 0 : Math.min(max - 1, p.maxPerInput);
  }
  if (max < 1 || manualRationalProblem(at(max), allowedRates, pipelines)) return { why: "No length fits this rate at every allowed rate: use a preset, or narrow the allowed rates." };
  const now = u.taps_unit === "input" ? u.taps : Math.ceil(u.taps / Math.max(1, Math.round(worstCaseRatio(u.target))));
  let taps = Math.max(1, Math.min(now, max));
  if (u.cutoff_mode === "nyquist_stop") {
    // Longer only helps Stop at Nyquist: find its shortest length up to `max`.
    const nyq = (n: number) => nyquistStopBlockReason(at(n), allowedRates);
    const atMax = nyq(max);
    if (atMax) {
      return {
        why: `No length fits both here: the fractional limits allow at most ${max.toLocaleString("en-US")} taps per input sample, and ${atMax} Turn Stop at Nyquist off (the Fraction cutoff) or narrow the allowed rates.`,
      };
    }
    if (nyq(taps)) {
      let lo = taps;
      let hi = max;
      while (hi - lo > 1) {
        const mid = Math.floor((lo + hi) / 2);
        if (nyq(mid)) lo = mid;
        else hi = mid;
      }
      taps = hi;
    }
  }
  return { taps_unit: "input", taps };
}

/** dsp-core `kaiser_beta_for_attenuation`. */
function kaiserBetaForAttenuation(a: number): number {
  if (a > 50) return 0.1102 * (a - 8.7);
  if (a >= 21) return 0.5842 * Math.pow(a - 21, 0.4) + 0.07886 * (a - 21);
  return 0;
}

/**
 * dsp-core `band_design_for` length: the fewest taps whose half transition
 * fits half the band's gap, at design ratio `ratio` (like
 * `min_taps_for_half_width`).
 */
export function bandDesignTaps(window: WindowKind, band: BandSpec, ratio: number): number {
  if (ratio <= 1) return 1;
  const beta = kaiserBetaForAttenuation(band.attenuation_db);
  const w = 0.5 * (band.stopband_start - band.passband_end);
  const fits = (taps: number) => halfTransition(window, taps, ratio, beta) <= w;
  if (fits(2)) return 2;
  let hi = 4;
  while (!fits(hi)) {
    if (hi >= 2 ** 40) return Number.MAX_SAFE_INTEGER;
    hi *= 2;
  }
  let lo = hi / 2;
  while (hi - lo > 1) {
    const mid = lo + Math.floor((hi - lo) / 2);
    if (fits(mid)) hi = mid;
    else lo = mid;
  }
  return hi;
}

/**
 * The first allowed rate at which a Band design is too long or too costly
 * for the rational runtime, like the daemon's `check_rational_at` for band
 * designs: the band length at the design ratio `max(up, down)` against the
 * grid cap, then (×4 for a mixed intermediate phase) the cost budget.
 */
export function bandRationalProblem(
  u: { target: RateTarget; cutoff_mode: CutoffMode; auto_taps: unknown; preset?: unknown; window: WindowKind; band: BandSpec; phase: FilterPhase; phase_mix: number },
  allowedRates: readonly number[],
  pipelines: number,
): { rate: number; ratio: RateRatio; gridTaps: number; text: string } | null {
  if (u.preset || u.auto_taps || u.cutoff_mode !== "band") return null;
  const mixed = u.phase === "intermediate" && u.phase_mix > 0 && u.phase_mix < 1;
  for (const rate of ALLOWED_SOURCE_RATES) {
    if (!allowedRates.includes(rate)) continue;
    const r = resolveRateRatio(u.target, rate);
    if (r.down === 1) continue;
    const taps = bandDesignTaps(u.window, u.band, Math.max(r.up, r.down));
    const grid = mixed ? taps * 4 : taps;
    const out = (rate * r.up) / r.down;
    const macs = out * Math.ceil(grid / r.up) * Math.max(1, pipelines);
    if (taps > RATIONAL_MAX_GRID || grid > RATIONAL_MAX_GRID || macs > RATIONAL_MAX_MACS_PER_S) {
      return {
        rate,
        ratio: r,
        gridTaps: grid,
        text: `This band needs ${grid.toLocaleString("en-US")} taps on the ${r.up}x grid at ${khz(rate)} → ${khz(out)} (${r.up}/${r.down}), more than a fractional conversion takes: widen the band's transition or lower its attenuation.`,
      };
    }
  }
  return null;
}
