// The engine's graph-rate policy as the Device page edits it: which rates are
// allowed and which one is the default. Mirrors the daemon: io-pw
// `nearest_rate` and `control::config::normalize_rates`.

import type { EngineState, RatePolicy, SourceLanding } from "../api/types";
import { fmtRate } from "./format";
import { ALLOWED_SOURCE_RATES } from "./upsamplerLimits";

/** Every graph rate the engine can run at, ascending (daemon `ALLOWED_RATES`). */
export const ENGINE_RATES: readonly number[] = ALLOWED_SOURCE_RATES;

/** At and above this, a rate is off by default (DXD and up). */
export const HIGH_RATE = 352800;

/** 44100 for the 44.1 kHz family, 48000 for the 48 kHz one, else 0. */
export function rateFamily(rate: number): number {
  if (rate % 11025 === 0) return 44100;
  if (rate % 8000 === 0) return 48000;
  return 0;
}

/** Same family first, then the closest, 48 kHz on a tie (io-pw `nearest_rate`). */
export function nearestRate(want: number, candidates: readonly number[]): number | null {
  let best: number | null = null;
  const key = (r: number): [number, number, number] => [rateFamily(r) !== rateFamily(want) ? 1 : 0, Math.abs(r - want), r !== 48000 ? 1 : 0];
  for (const r of candidates) {
    if (best === null) {
      best = r;
      continue;
    }
    const a = key(r);
    const b = key(best);
    if (a[0] < b[0] || (a[0] === b[0] && (a[1] < b[1] || (a[1] === b[1] && a[2] < b[2])))) best = r;
  }
  return best;
}

type Policy = Pick<RatePolicy, "allowed_rates" | "default_rate">;

/** Sorted, without duplicates, and a default that is one of the allowed rates. */
export function normalizeRatePolicy<P extends Policy>(p: P): P {
  const allowed = [...new Set(p.allowed_rates)].sort((a, b) => a - b);
  const default_rate = allowed.includes(p.default_rate) ? p.default_rate : (nearestRate(p.default_rate, allowed) ?? p.default_rate);
  return { ...p, allowed_rates: allowed, default_rate };
}

/**
 * Allow or block `rate`. Blocking the default moves it to the nearest allowed
 * rate. Null when that would leave no rate at all.
 */
export function toggleRate<P extends Policy>(p: P, rate: number): P | null {
  const on = p.allowed_rates.includes(rate);
  const allowed = on ? p.allowed_rates.filter((r) => r !== rate) : [...p.allowed_rates, rate];
  if (allowed.length === 0) return null;
  return normalizeRatePolicy({ ...p, allowed_rates: allowed });
}

/** Make `rate` the default (allowing it if it was not). */
export function withDefaultRate<P extends Policy>(p: P, rate: number): P {
  return normalizeRatePolicy({ ...p, allowed_rates: p.allowed_rates.includes(rate) ? p.allowed_rates : [...p.allowed_rates, rate], default_rate: rate });
}

export interface RateChip {
  rate: number;
  /** In `allowed_rates`. */
  enabled: boolean;
  /** `default_rate`. */
  isDefault: boolean;
  /** The graph runs at it right now (something is playing). */
  playing: boolean;
  /** The device does not play it with the current settings (an allowed one is kept, inactive). */
  unsupported: boolean;
  /** Clicking toggles it: always, except the last allowed rate. */
  canToggle: boolean;
  title: string;
}

export interface RateChipInputs {
  policy: Policy;
  /** `status.playable_rates`: every engine rate the device plays with the current settings (empty when unknown). */
  playable: readonly number[] | null | undefined;
  state: EngineState | null | undefined;
  graphRate: number | null | undefined;
}

export function rateChips({ policy, playable, state, graphRate }: RateChipInputs): RateChip[] {
  const known = !!playable && playable.length > 0;
  return ENGINE_RATES.map((rate) => {
    const enabled = policy.allowed_rates.includes(rate);
    const isDefault = enabled && policy.default_rate === rate;
    const playing = state === "playing" && graphRate === rate;
    const unsupported = known && !playable!.includes(rate);
    // `allowed_rates` is a preference kept across devices: always editable.
    const last = enabled && policy.allowed_rates.length === 1;
    const canToggle = !last;
    const name = fmtRate(rate);
    const parts: string[] = [];
    if (unsupported) parts.push(enabled ? "Kept, but not used on this device: PipeWire converts it to a rate the device plays." : "This device does not play it with the current settings: PipeWire would convert it.");
    if (last) parts.push(`${name} is the only allowed rate; allow another one first.`);
    else parts.push(enabled ? `Allowed. Click to stop using ${name}${unsupported ? "" : "; streams at it are then resampled by PipeWire"}.` : `Not allowed. Click to allow ${name}.`);
    if (isDefault) parts.push("Default rate: used while idle and when rate following is off.");
    if (playing) parts.push("Playing at this rate now.");
    if (rate >= HIGH_RATE && !unsupported)
      parts.push("Allowing it lets e.g. DXD (352.8 kHz) streams reach the DAC without resampling; upsampling then uses a smaller ratio at this rate.");
    if (enabled && !isDefault && !unsupported) parts.push("Right-click (or Shift+F10) to use it as the default.");
    return { rate, enabled, isDefault, playing, unsupported, canToggle, title: parts.join(" ") };
  });
}

/**
 * The source rates the Upsampling view previews: the configured ones and
 * the engine's effective graph rates (ascending). A configured rate the
 * engine does not offer on this device is marked from the response's
 * `device_landings` ([`convertedRates`]).
 */
export function previewRates(effective: readonly number[] | null | undefined, allowed: readonly number[] | null | undefined): number[] {
  const out = [...new Set([...(allowed ?? []), ...(effective ?? [])])].sort((a, b) => a - b);
  return out.length > 0 ? out : [44100, 48000];
}

/** Source rate → the rate PipeWire converts it to before the engine, from `device_landings`. */
export function convertedRates(landings: readonly SourceLanding[] | null | undefined): Map<number, number> {
  const out = new Map<number, number>();
  for (const l of landings ?? []) if (l.converted_to) out.set(l.source_rate, l.converted_to);
  return out;
}
