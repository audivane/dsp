// Which source rate the Upsampling view designs, plots and measures for.

import type { DeviceCapabilities, EngineState, RateRatio, RateTarget, SampleFormat, SourceLanding } from "../api/types";
import { fmtSampleFormat } from "./format";

export interface ViewRateInputs {
  state: EngineState | null | undefined;
  graphRate: number | null | undefined;
  /** Device-restricted default rate from status (`effective_default_rate`). */
  effectiveDefaultRate: number | null | undefined;
  /** What this function returned the last time the engine was not switching. */
  held: number | null;
  /** The rates the view offers ("Preview at"): `previewRates`, ascending. */
  rates: readonly number[];
}

/**
 * The rate while playing; while the engine is switching (a reopen, e.g. after
 * a filter-length change) the last rate shown, so the readout and plot do not
 * flash to another rate; otherwise the device's default rate (the lowest
 * offered rate when the engine reports none: a 48k-only device never plays
 * 44.1k).
 */
export function viewSourceRate({ state, graphRate, effectiveDefaultRate, held, rates }: ViewRateInputs): number {
  const offered = (r: number | null | undefined): r is number => typeof r === "number" && rates.includes(r);
  if (state === "playing" && offered(graphRate)) return graphRate;
  if (state === "switching" && held !== null) return held;
  if (offered(effectiveDefaultRate)) return effectiveDefaultRate;
  return rates[0] ?? 44100;
}

/**
 * The rate the user picked to preview, while it is still offered and not
 * just the playing rate; otherwise null (follow the playing rate).
 */
export function chosenPreviewRate(chosen: number | null, rates: readonly number[]): number | null {
  return chosen !== null && rates.includes(chosen) ? chosen : null;
}

// ------------------------------------------------------------ rate targets

function gcd(a: number, b: number): number {
  while (b) [a, b] = [b, a % b];
  return a;
}

/** `output / input`, reduced. */
export function rateRatioOf(input: number, output: number): RateRatio {
  const i = Math.max(1, Math.round(input));
  const o = Math.max(1, Math.round(output));
  const g = gcd(i, o);
  return { up: o / g, down: i / g };
}

/**
 * The ratio a target asks for at `sourceRate`, like the protocol's
 * `resolve_rate_ratio`: without a device probe `max_rate_any` resolves like
 * `max_rate` (the device-aware choice is the engine's, shown from status).
 */
export function resolveRateRatio(target: RateTarget, sourceRate: number): RateRatio {
  if (target === "off") return { up: 1, down: 1 };
  if ("ratio" in target) return { up: Math.max(1, target.ratio), down: 1 };
  if ("rate" in target) return rateRatioOf(sourceRate, target.rate);
  const cap = "max_rate" in target ? target.max_rate : target.max_rate_any;
  let r = 1;
  while (sourceRate * r * 2 <= cap) r *= 2;
  return { up: r, down: 1 };
}

/** The four kinds of rate target the view offers. */
export type RateKind = "off" | "ratio" | "max" | "rate";

export function rateKind(t: RateTarget): RateKind {
  if (t === "off") return "off";
  if ("ratio" in t) return "ratio";
  if ("rate" in t) return "rate";
  return "max";
}

/** Engine output rates an explicit `{rate}` target may name. */
export const TARGET_RATES = [44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000] as const;

/** Caps offered for "highest rate up to". */
export const MAX_RATE_CAPS = [192000, 384000, 768000] as const;

/** DAC rates above 768 kHz the engine opens: ×32 and ×64 of 44.1 and 48 kHz. */
export const HIGH_OUTPUT_RATES = [1411200, 1536000, 2822400, 3072000] as const;

/** Highest output rate the engine opens (`MAX_OUTPUT_RATE`). */
export const MAX_OUTPUT_RATE = 3072000;

/** Largest integer ratio the engine runs (`MAX_INTEGER_RATIO`). */
export const MAX_INTEGER_RATIO = 64;

/**
 * The DAC rates above 768 kHz a target reaches from the two base rates
 * (44.1 and 48 kHz): a 1.5M cap or ×32 means 1411.2 and 1536 kHz.
 * Empty when it stays at or below 768 kHz.
 */
export function highRatesFor(target: RateTarget): number[] {
  return [44100, 48000]
    .map((rate) => {
      const r = resolveRateRatio(target, rate);
      return r.down === 1 ? rate * r.up : 0;
    })
    .filter((out) => out > 768000);
}

/** The formats the driver tries when `formats` is empty (io-alsa `FORMAT_PREFERENCE`). */
export const DRIVER_FORMATS: readonly SampleFormat[] = ["s32", "s24", "s24_3", "s16"];

/** What the connected DAC can be opened at, from its probe (`status.device_caps`). */
export interface DacRates {
  /** Rates the probe lists with any format. */
  reported: number[];
  /** Rates it takes in one of the configured formats: what the engine's
   *  open plan can land on (lifecycle `DeviceRates::playable`). */
  playable: number[];
  /** The configured formats (empty: the driver's preference). */
  formats: SampleFormat[];
}

/** [`DacRates`] for a probe and the configured `formats`; null without a probe. */
export function dacRates(caps: DeviceCapabilities | null | undefined, formats: readonly SampleFormat[]): DacRates | null {
  if (!caps || caps.rates.length === 0) return null;
  const tried = formats.length > 0 ? formats : DRIVER_FORMATS;
  return {
    reported: caps.rates.filter((c) => c.formats.length > 0).map((c) => c.rate),
    playable: caps.rates.filter((c) => c.formats.some((f) => tried.includes(f.format))).map((c) => c.rate),
    formats: [...formats],
  };
}

const kHz = (r: number) => `${Number((r / 1000).toFixed(1))} kHz`;

/**
 * Why a choice that needs one of `rates` is unavailable on this DAC: it
 * reports none of them ("Your DAC does not report 2822.4 kHz or
 * 3072 kHz"), or only in other formats than the configured ones ("Your DAC
 * does not take 3072 kHz in S16"). Null when the engine can open one, or
 * without a probe.
 */
export function dacRateBlock(rates: readonly number[], dac: DacRates | null): string | null {
  if (rates.length === 0 || !dac) return null;
  if (rates.some((r) => dac.playable.includes(r))) return null;
  const elsewhere = rates.filter((r) => dac.reported.includes(r));
  if (elsewhere.length > 0) return `Your DAC does not take ${elsewhere.map(kHz).join(" or ")} in ${dac.formats.map(fmtSampleFormat).join(" or ")}`;
  return `Your DAC does not report ${rates.map(kHz).join(" or ")}`;
}

/** The highest rate the device plays in the configured formats, or null without a probe. */
export function dacMaxRate(dac: DacRates | null): number | null {
  return dac && dac.playable.length > 0 ? Math.max(...dac.playable) : null;
}

/**
 * Why a rate above what the device plays is unavailable: "This device plays
 * up to 48 kHz", or null (it plays `rate`, or the device is not known).
 */
export function aboveDeviceBlock(rate: number, dac: DacRates | null): string | null {
  const max = dacMaxRate(dac);
  return max !== null && rate > max ? `This device plays up to ${kHz(max)}` : null;
}

/** [`dacRateBlock`] for the high rates (above 768 kHz) a cap or multiple reaches. */
export function dacHighRateBlock(target: RateTarget, dac: DacRates | null): string | null {
  return dacRateBlock(highRatesFor(target), dac);
}

/**
 * Why the DAC cannot be opened at the rate `target` itself asks for (with
 * the configured formats), or null: a fixed rate it does not take, or the
 * high rates (above 768 kHz) a cap or multiple reaches.
 */
export function targetDacBlock(target: RateTarget, dac: DacRates | null): string | null {
  if (target === "off") return null;
  if ("rate" in target) return dacRateBlock([target.rate], dac);
  return dacHighRateBlock(target, dac);
}

/**
 * A short note for a selected target the DAC cannot take: the engine lands
 * lower. `outputRate` is what it runs now (null when not playing).
 */
export function landsLowerNote(target: RateTarget, dac: DacRates | null, outputRate: number | null): string | null {
  const why = targetDacBlock(target, dac);
  if (!why) return null;
  return outputRate ? `${why}, so the engine lands lower: ${kHz(outputRate)} now.` : `${why}, so the engine lands lower.`;
}

/** Largest term of a fractional ratio the engine runs (`MAX_RATIONAL_TERM`). */
export const MAX_RATIONAL_TERM = 4096;

/** Whether `r` has a term past [`MAX_RATIONAL_TERM`]: the engine never runs it. */
export function pastTermLimit(r: RateRatio): boolean {
  return r.up > MAX_RATIONAL_TERM || r.down > MAX_RATIONAL_TERM;
}

/**
 * The allowed source rates `target` cannot be reached from: a fractional
 * ratio with a term past the limit (44.1 kHz → 3072 kHz is 10240/147). The
 * engine refuses such a config while those rates are allowed.
 */
export function unreachableSources(target: RateTarget, allowedRates: readonly number[]): { rate: number; ratio: RateRatio }[] {
  return [...allowedRates]
    .sort((a, b) => a - b)
    .map((rate) => ({ rate, ratio: resolveRateRatio(target, rate) }))
    .filter(({ ratio }) => ratio.down !== 1 && pastTermLimit(ratio));
}

/**
 * A plain note for a fixed rate some allowed source rate cannot reach,
 * with the same-family rate that works; null when every one can.
 */
export function unreachableNote(target: RateTarget, allowedRates: readonly number[]): string | null {
  const bad = unreachableSources(target, allowedRates);
  if (bad.length === 0 || target === "off" || !("rate" in target)) return null;
  const from = bad.map((b) => kHz(b.rate)).join(" or ");
  const off = bad.map((b) => kHz(b.rate)).join(" and ");
  // The same multiple in the source's family: 3072 kHz (48 k × 64) → 2822.4 kHz (44.1 k × 64).
  const fam44 = bad[0].rate % 11025 === 0;
  const twin = fam44 ? Math.round((target.rate * 147) / 160) : Math.round((target.rate * 160) / 147);
  const r = bad[0].ratio;
  return `${kHz(target.rate)} cannot be reached from ${from}: that takes a fractional ratio (${r.up}/${r.down}), and the engine caps its terms at ${MAX_RATIONAL_TERM}. Use ${kHz(twin)}, the same family, or turn ${off} off on the Device page.`;
}

/** Whether an engine refusal is the rational term limit ("… has a term above 4096"). */
export function isTermLimitMessage(message: string): boolean {
  return /has a term above \d+/.test(message);
}

/**
 * Whether a response refusal says the engine does not measure this ratio
 * (a ratio limit, not a broken design): shown as plain text under the plot.
 */
export function isUnmeasuredRatio(message: string): boolean {
  return /unsupported ratio|not measured above|target ratio must be between/i.test(message);
}

/**
 * `t` for an engine from before fractional rates (`rational` false): a
 * remembered "best for DAC" cap becomes the same cap within the source's
 * family, and a fixed rate (which such an engine cannot run) ×4.
 */
export function targetForEngine(t: RateTarget, rational: boolean): RateTarget {
  if (rational || t === "off") return t;
  if ("max_rate_any" in t) return { max_rate: t.max_rate_any };
  if ("rate" in t) return { ratio: 4 };
  return t;
}

/**
 * The target a click on a kind gives: the last one used of that kind when
 * known, else a sensible default (×4, up to 768 kHz, or the playing output
 * rate / 96 kHz for an explicit rate).
 */
export function targetForKind(kind: RateKind, last: Partial<Record<RateKind, RateTarget>>, outputRate: number | null, rational = true): RateTarget {
  const remembered = last[kind];
  if (remembered) return targetForEngine(remembered, rational);
  if (kind === "off") return "off";
  if (kind === "ratio") return { ratio: 4 };
  if (kind === "max") return { max_rate: 768000 };
  const r = outputRate && (TARGET_RATES as readonly number[]).includes(outputRate) ? outputRate : 96000;
  return { rate: r };
}

const khz = (r: number) => `${Number((r / 1000).toFixed(1))} kHz`;
const ratioStr = (r: RateRatio) => (r.down === 1 ? `×${r.up}` : `${r.up}/${r.down}`);

/** "44.1 kHz → 48 kHz (160/147)", or "48 kHz (×1)" when the rate is not changed. */
export function landingText(input: number, output: number, ratio: RateRatio): string {
  return ratio.up === ratio.down ? `${khz(input)} (×1, not resampled)` : `${khz(input)} → ${khz(output)} (${ratioStr(ratio)})`;
}

/** Why the preview at a source rate shows no upsampling on this device, from the response's `device_landings`. */
export interface DeviceLandingNote {
  /** PipeWire converts the source before the engine: nothing of this setting runs at that rate. */
  converted: boolean;
  text: string;
  /** "Best for DAC" would upsample here: its target and a short line saying what it does. */
  best: { target: RateTarget; text: string } | null;
}

/**
 * The note for `landing` (the entry for the previewed source rate) under
 * `target`: PipeWire converts the rate first, or the engine lands 1:1
 * because the DAC plays nothing higher the target reaches. Null otherwise.
 */
export function deviceLandingNote(landing: SourceLanding | null | undefined, target: RateTarget): DeviceLandingNote | null {
  if (!landing || target === "off" || "rate" in target) return null;
  const src = landing.source_rate;
  const b = landing.best_for_dac;
  const best = b ? { target: b.target, text: `Best for DAC would play it as ${khz(Math.round((src * b.ratio.up) / b.ratio.down))} (${ratioStr(b.ratio)}).` } : null;
  if (landing.converted_to) {
    return { converted: true, text: `PipeWire converts ${khz(src)} to ${khz(landing.converted_to)} before Audivane on this DAC.`, best };
  }
  const unity = (r: RateRatio) => r.up === r.down;
  if (!unity(landing.ratio) || unity(landing.requested)) return null;
  const text =
    "ratio" in target
      ? `Your DAC plays no multiple of ${khz(src)} up to ×${target.ratio}; upsampling does nothing here.`
      : "max_rate" in target
        ? `Your DAC plays nothing above ${khz(src)} in this family; upsampling does nothing here.`
        : `Your DAC plays nothing above ${khz(src)}; upsampling does nothing here.`;
  return { converted: false, text, best };
}

/**
 * A note when the running conversion is there because of the DAC: it does
 * not play the source rate itself, or `max_rate_any` crossed to the other
 * rate family for a higher rate the DAC plays. Null otherwise.
 */
export function deviceResampleNote(args: {
  graphRate: number;
  outputRate: number;
  ratio: RateRatio;
  target: RateTarget;
  /** Rates the device takes (`device_caps.rates`); null when not probed. */
  deviceRates: readonly number[] | null;
}): string | null {
  const { graphRate, outputRate, ratio, target, deviceRates } = args;
  if (ratio.up === ratio.down || graphRate <= 0) return null;
  const fractional = ratio.down !== 1;
  if (deviceRates && deviceRates.length > 0 && !deviceRates.includes(graphRate)) {
    return `This DAC cannot play ${khz(graphRate)} as it is, so the engine resamples it to ${khz(outputRate)} (${ratioStr(ratio)}), a rate the DAC plays.`;
  }
  if (fractional && target !== "off" && "max_rate_any" in target) {
    return `No higher multiple of ${khz(graphRate)} suits this DAC as well, so the engine crosses to the other rate family: ${khz(graphRate)} → ${khz(outputRate)} (${ratioStr(ratio)}).`;
  }
  return null;
}
