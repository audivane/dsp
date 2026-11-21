// The engine's design and CPU limits (`status.limits`), which depend on
// the machine (the design workspace is a quarter of its RAM, 1.5 to 4 GiB)
// and on the engine version (the tap limits). The app reads them from the
// daemon; a daemon from before `status.limits` has the limits below.

import type { EngineLimits } from "../api/types";

/** The limits of a daemon that does not publish them (before `status.limits`). */
export const FALLBACK_LIMITS: EngineLimits = {
  max_kernel_taps: 4_194_304,
  intermediate_max_taps: 1_048_576,
  design_workspace_bytes: 1.5 * 2 ** 30,
  cpu_share: 0.5,
};

/** The published limits, or [`FALLBACK_LIMITS`] when the daemon sends none (or is not connected). */
export function engineLimits(published: EngineLimits | null | undefined): EngineLimits {
  return published ?? FALLBACK_LIMITS;
}

/** The power-of-two lengths the length control offers: 4k up to the engine's kernel limit. */
export function tapChoices(maxKernelTaps: number): number[] {
  const out: number[] = [];
  for (let n = 4096; n <= maxKernelTaps; n *= 2) out.push(n);
  return out;
}
