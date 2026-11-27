// The device channels the engine's left and right play on, for a device
// with more than two (`output_channels` on the main device and on each
// follower; see docs/control-plane.md "Output channels"). 0-based on the
// wire, shown from 1.

import type { OutputChannels } from "../api/types";

export const DEFAULT_PAIR: OutputChannels = [0, 1];
/** Channels a pair may name: 0..63 (the daemon's `MAX_DEVICE_CHANNELS`). */
export const MAX_DEVICE_CHANNELS = 64;

export function isDefaultPair(p: OutputChannels | null | undefined): boolean {
  return !p || (p[0] === 0 && p[1] === 1);
}

/**
 * Offer the selector only where it means something: the device opened at
 * more than two channels (it has no stereo mode), or a pair other than the
 * first two is already set (so it can be put back). A stereo device shows
 * nothing new.
 */
export function showPairSelector(openedChannels: number | null | undefined, pair: OutputChannels | null | undefined): boolean {
  return (openedChannels ?? 0) > 2 || !isDefaultPair(pair);
}

/**
 * The pairs to pick from on a device with `channels` channels: neighbours
 * 1+2, 3+4, … (an odd last channel is left out), plus the current pair
 * when it is another one (set elsewhere, e.g. with the CLI).
 */
export function pairOptions(channels: number, current?: OutputChannels | null): OutputChannels[] {
  const n = Math.min(Math.max(channels, 2), MAX_DEVICE_CHANNELS);
  const out: OutputChannels[] = [];
  for (let l = 0; l + 1 < n; l += 2) out.push([l, l + 1]);
  if (current && !out.some((p) => p[0] === current[0] && p[1] === current[1])) out.push(current);
  return out;
}

export function pairKey(p: OutputChannels | null | undefined): string {
  const [l, r] = p ?? DEFAULT_PAIR;
  return `${l},${r}`;
}

export function parsePairKey(key: string): OutputChannels {
  const [l, r] = key.split(",").map((s) => Number.parseInt(s, 10));
  return [l, r];
}

/** "Channels 3 + 4" (from 1, as devices label them). */
export function pairLabel(p: OutputChannels): string {
  return `Channels ${p[0] + 1} + ${p[1] + 1}`;
}
