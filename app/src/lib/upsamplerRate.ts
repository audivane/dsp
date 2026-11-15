// Which source rate the Upsampling view designs, plots and measures for.

import type { EngineState } from "../api/types";

/** Source rates the view offers (the rate picker). */
export const SOURCE_RATES = [44100, 48000, 88200, 96000] as const;

export function isSourceRate(r: number | null | undefined): r is (typeof SOURCE_RATES)[number] {
  return typeof r === "number" && (SOURCE_RATES as readonly number[]).includes(r);
}

export interface ViewRateInputs {
  state: EngineState | null | undefined;
  graphRate: number | null | undefined;
  /** Device-restricted default rate from status (`effective_default_rate`). */
  effectiveDefaultRate: number | null | undefined;
  /** What this function returned the last time the engine was not switching. */
  held: number | null;
}

/**
 * The rate while playing; while the engine is switching (a reopen, e.g. after
 * a filter-length change) the last rate shown, so the readout and plot do not
 * flash to another rate; otherwise the device's default rate (44.1 kHz only
 * when the engine reports nothing: a 48k-only device never plays 44.1k).
 */
export function viewSourceRate({ state, graphRate, effectiveDefaultRate, held }: ViewRateInputs): number {
  if (state === "playing" && isSourceRate(graphRate)) return graphRate;
  if (state === "switching" && held !== null) return held;
  return isSourceRate(effectiveDefaultRate) ? effectiveDefaultRate : 44100;
}
