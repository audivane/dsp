// What the output device accepts (status.device_caps, probed by the engine
// when it opens the device) and the UI hints built on it. Mirrors the
// daemon's rules: io-alsa `ratio_ladder` / `plan_open` and io-pw `rate_caps`.

import type { DeviceCapabilities, SampleFormat } from "../api/types";
import { fmtRate, fmtSampleFormat } from "./format";

export const NOT_SUPPORTED_BY_DEVICE = "Not supported by this device";

/** Container width of an ALSA format: what dither is capped at. */
export function formatBits(f: SampleFormat): number {
  return f === "s16" ? 16 : f === "s32" ? 32 : 24;
}

/** Dither target after the format cap (the daemon's `effective_dither_bits`). */
export function effectiveDitherBits(configured: number, format: SampleFormat | null | undefined): number {
  return format ? Math.min(configured, formatBits(format)) : configured;
}

/** Ratios the engine tries when the device rejects source × desired. */
export function ratioLadder(desired: number): number[] {
  const out: number[] = [];
  let r = Math.max(1, Math.floor(desired));
  for (;;) {
    out.push(r);
    if (r === 1) break;
    r = (r & (r - 1)) === 0 ? r / 2 : r - 1;
  }
  return out;
}

export function formatsAt(caps: DeviceCapabilities, rate: number): SampleFormat[] {
  return caps.rates.find((r) => r.rate === rate)?.formats.map((f) => f.format) ?? [];
}

export function supports(caps: DeviceCapabilities, rate: number, format: SampleFormat): boolean {
  return formatsAt(caps, rate).includes(format);
}

/**
 * Supported at some rate at all. For a device the engine cannot use (no
 * usable rate: wrong channel count, rates outside the engine's list) this
 * falls back to the formats the device accepts at all.
 */
export function supportsFormat(caps: DeviceCapabilities, format: SampleFormat): boolean {
  if (caps.rates.length === 0) return (caps.formats ?? []).includes(format);
  return caps.rates.some((r) => r.formats.some((f) => f.format === format));
}

/** Significant bits the device reports for `format` at `rate`, when known. */
export function sbitsAt(caps: DeviceCapabilities, rate: number, format: SampleFormat): number | null {
  return caps.rates.find((r) => r.rate === rate)?.formats.find((f) => f.format === format)?.sbits ?? null;
}

/**
 * Bits the DAC converts in the format in use (its significant bits), or null
 * while unknown. "S32" is the 4-byte container; a DAC may convert only 24 of
 * those 32 bits and ignore the rest.
 */
export function usedBits(
  caps: DeviceCapabilities | null | undefined,
  rate: number | null | undefined,
  format: SampleFormat | null | undefined,
): number | null {
  if (!caps || !rate || !format) return null;
  return sbitsAt(caps, rate, format);
}

/** Dither targets the Output view offers. */
export const DITHER_TARGETS = [16, 20, 24, 32] as const;

/**
 * Dither target to suggest for the format in use: the bits the DAC converts,
 * at most 24 (no DAC's analog side resolves more, so dither below that only
 * gets closer to plain rounding). Rounded down to an offered target.
 */
export function suggestedDitherBits(format: SampleFormat | null | undefined, used: number | null): number | null {
  if (!format) return null;
  const bits = Math.min(used ?? formatBits(format), formatBits(format), 24);
  return [...DITHER_TARGETS].reverse().find((t) => t <= bits) ?? 16;
}

/** The hint next to "Dither to": a short text, its tooltip, and the target a button offers (if any). */
export interface DitherAdvice {
  text: string;
  title: string;
  offer: number | null;
}

/**
 * Advice on the dither target `current` for the format in use and the bits
 * the DAC converts. "Matches" only when the target equals the DAC's bits; on
 * a DAC that uses all 32 bits, 24 is recommended but 32 is fine too.
 */
export function ditherAdvice(current: number, format: SampleFormat | null | undefined, used: number | null): DitherAdvice | null {
  const suggested = suggestedDitherBits(format, used);
  if (suggested === null || !format) return null;
  const width = formatBits(format);
  const usesAll = used !== null && used >= width && width > suggested;
  const why =
    `Dither at ${suggested} bits adds noise near −141 dBFS, well below any DAC's own noise, ` +
    `and guards against a DAC that quietly keeps fewer bits than it reports.`;
  if (current === suggested) {
    if (used === suggested) return { text: "Matches this DAC", title: `The DAC converts ${used} bits; dither at ${used} bits matches that.`, offer: null };
    if (usesAll) return { text: `Recommended; ${width} bits also works`, title: `The DAC takes all ${width} bits. ${why} ${width} bits is fine too if it really uses them all.`, offer: null };
    return { text: "Recommended for this DAC", title: used === null ? `${suggested} bits suits most DACs.` : `The DAC converts ${used} bits; ${suggested} is the nearest target at or below that.`, offer: null };
  }
  if (usesAll && current === width) {
    return { text: `Fine: this DAC uses all ${width} bits. ${suggested} is the safer default`, title: `${why} Both sound the same.`, offer: suggested };
  }
  return {
    text: `Suggested for this DAC: ${suggested} bits`,
    title: used === null ? `${suggested} bits suits most DACs.` : used > suggested ? `The DAC takes ${used} bits. ${why}` : `The DAC converts ${used} bits; dither at ${suggested} bits matches that.`,
    offer: suggested,
  };
}

/**
 * Why the preferred format is not the one in use, for the "In use: X" hint:
 * "S32 not available at 192 kHz", or null when there is nothing to explain.
 */
export function formatUnavailableReason(
  caps: DeviceCapabilities | null | undefined,
  preferred: SampleFormat | null | undefined,
  inUse: SampleFormat | null | undefined,
  outputRate: number | null | undefined,
): string | null {
  if (!caps || !preferred || !inUse || preferred === inUse) return null;
  if (!supportsFormat(caps, preferred)) return `${fmtSampleFormat(preferred)} not supported by this device`;
  if (outputRate && !supports(caps, outputRate, preferred)) return `${fmtSampleFormat(preferred)} not available at ${fmtRate(outputRate)}`;
  return null;
}

function joinRates(rates: number[]): string {
  const names = rates.map(fmtRate);
  if (names.length <= 1) return names.join("");
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}

/**
 * Configured rates the device cannot play, and the sentence that explains
 * it, e.g. "This device supports 48 kHz only; 44.1 kHz streams are resampled
 * by PipeWire." Null when every configured rate is playable (or the engine
 * has not reported its effective rates).
 */
export function rateSupportNote(configured: number[], effective: number[] | null | undefined): { unsupported: number[]; text: string } | null {
  if (!effective || effective.length === 0) return null;
  const unsupported = configured.filter((r) => !effective.includes(r));
  if (unsupported.length === 0) return null;
  const streams =
    unsupported.length <= 2 ? `${joinRates(unsupported)} streams` : `${fmtRate(unsupported[0])}, ${fmtRate(unsupported[1])} and other streams`;
  const text =
    effective.length === 1
      ? `This device supports ${fmtRate(effective[0])} only; ${streams} are resampled by PipeWire.`
      : `This device cannot play ${joinRates(unsupported)}; those streams are resampled by PipeWire.`;
  return { unsupported, text };
}

/**
 * What Audivane Direct can do on this device (`status.direct_rates`): it is
 * bit-perfect at the rates the device plays natively, and with the engine
 * bypass resampler it also runs at rates the engine converts to a native
 * one (not bit-perfect); `caps` tells them apart. Null when Direct plays
 * every effective rate natively (or nothing is known).
 */
export function directRateNote(
  effective: number[] | null | undefined,
  direct: number[] | null | undefined,
  caps?: DeviceCapabilities | null,
): string | null {
  if (!effective || effective.length === 0) return null;
  const all = direct ?? [];
  if (all.length === 0) return "Audivane Direct cannot play on this device: none of its sample rates is native (Direct never upsamples). Use Audivane DSP.";
  const plays = (r: number) => !caps || caps.rates.some((c) => c.rate === r);
  const native = all.filter(plays);
  const converted = all.filter((r) => !plays(r));
  const everyRate = effective.every((r) => all.includes(r));
  if (everyRate && converted.length === 0) return null;
  const rest = everyRate ? "" : " A Direct stream at another rate is resampled by PipeWire.";
  if (converted.length === 0) {
    const only = native.length === 1 ? `${fmtRate(native[0])} only` : joinRates(native);
    return `Audivane Direct is bit-perfect at ${only} on this device; a Direct stream at another rate is resampled by PipeWire.`;
  }
  const conv = `at ${joinRates(converted)} the engine converts to a rate the DAC plays (not bit-perfect).`;
  if (native.length === 0) return `Audivane Direct is not bit-perfect on this device: ${conv}${rest}`;
  return `Audivane Direct is bit-perfect at ${joinRates(native)} on this device; ${conv}${rest}`;
}

/** Whether a Direct stream at `rate` is resampled (not native on the device). */
export function directResamples(rate: number | null | undefined, direct: number[] | null | undefined): boolean {
  return !!rate && !!direct && direct.length > 0 && !direct.includes(rate);
}
