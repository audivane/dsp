// The engine's graph-rate policy as the Device page edits it: which rates are
// allowed and which one is the default. Mirrors the daemon: io-pw
// `nearest_rate` and `control::config::normalize_rates`.

import type { DeviceCapabilities, EngineState, RatePolicy } from "../api/types";
import { fmtRate } from "./format";
import { ratioLadder } from "./deviceCaps";
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

/**
 * Whether the device can run a graph at `rate`: it takes `rate × k` for a
 * ratio on the ladder from `maxRatio` down to 1 (io-pw `graph_rate_playable`,
 * format aside). Unknown device: assume yes.
 */
export function devicePlays(caps: DeviceCapabilities | null | undefined, rate: number, maxRatio: number): boolean {
  if (!caps || caps.rates.length === 0) return true;
  return ratioLadder(maxRatio).some((k) => caps.rates.some((c) => c.rate === rate * k && c.formats.length > 0));
}

export interface RateChip {
  rate: number;
  /** In `allowed_rates`. */
  enabled: boolean;
  /** `default_rate`. */
  isDefault: boolean;
  /** The graph runs at it right now (something is playing). */
  playing: boolean;
  /** The device cannot play it (greyed). */
  unsupported: boolean;
  /** Clicking toggles it. */
  canToggle: boolean;
  title: string;
}

export interface RateChipInputs {
  policy: Policy;
  /** `status.effective_rates`: what the engine offers on this device (empty when unknown). */
  effective: readonly number[] | null | undefined;
  caps: DeviceCapabilities | null | undefined;
  /** Largest upsampling ratio in use (1 when upsampling or DSP is off). */
  maxRatio: number;
  state: EngineState | null | undefined;
  graphRate: number | null | undefined;
}

export function rateChips({ policy, effective, caps, maxRatio, state, graphRate }: RateChipInputs): RateChip[] {
  const known = !!effective && effective.length > 0;
  return ENGINE_RATES.map((rate) => {
    const enabled = policy.allowed_rates.includes(rate);
    const isDefault = enabled && policy.default_rate === rate;
    const playing = state === "playing" && graphRate === rate;
    // Enabled rates: what the engine reports. Others: the probed capabilities.
    const unsupported = enabled && known ? !effective!.includes(rate) : !devicePlays(caps, rate, maxRatio);
    const last = enabled && policy.allowed_rates.length === 1;
    const canToggle = !last && (enabled || !unsupported);
    const name = fmtRate(rate);
    const parts: string[] = [];
    if (unsupported) parts.push("Not supported by this device.");
    if (last) parts.push(`${name} is the only allowed rate; allow another one first.`);
    else if (canToggle) parts.push(enabled ? `Allowed. Click to stop using ${name}${unsupported ? "" : "; streams at it are then resampled by PipeWire"}.` : `Not allowed. Click to allow ${name}.`);
    if (isDefault) parts.push("Default rate: used while idle and when rate following is off.");
    if (playing) parts.push("Playing at this rate now.");
    if (rate >= HIGH_RATE && !unsupported)
      parts.push("Allowing it lets e.g. DXD (352.8 kHz) streams reach the DAC without resampling; upsampling then uses a smaller ratio at this rate.");
    if (enabled && !isDefault && !unsupported) parts.push("Right-click (or Shift+F10) to use it as the default.");
    return { rate, enabled, isDefault, playing, unsupported, canToggle, title: parts.join(" ") };
  });
}

/**
 * The source rates the Upsampling view previews: the engine's effective
 * graph rates, else the configured ones (ascending).
 */
export function previewRates(effective: readonly number[] | null | undefined, allowed: readonly number[] | null | undefined): number[] {
  const list = effective && effective.length > 0 ? effective : (allowed ?? []);
  const out = [...new Set(list)].sort((a, b) => a - b);
  return out.length > 0 ? out : [44100, 48000];
}
