// Which source rate the Upsampling view designs, plots and measures for.

import type { EngineState } from "../api/types";

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
