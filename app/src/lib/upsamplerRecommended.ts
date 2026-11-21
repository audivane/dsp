// The "Recommended" upsampling setting: one catalogue design (`recommended`)
// and rate target (`max_rate: 768000`) offered as a single card, applied in
// one `set_upsampler`.

import type { RateTarget, UpsamplerConfig } from "../api/types";
import { RECOMMENDED_PRESET } from "./filterDesign";

/** Highest rate of the source's family the DAC takes, up to 768 kHz. */
export const RECOMMENDED_TARGET: RateTarget = { max_rate: 768000 };

export const RECOMMENDED_LABEL = "Recommended";
export const RECOMMENDED_BLURB = "Set it and forget it";
export const RECOMMENDED_TIP =
  "A clean, steep linear-phase filter, its length sized automatically for each rate to the output's resolution, upsampling to the highest rate of the same family the DAC takes (up to 768 kHz, never a fractional ratio). Applies the whole setting at once; Custom then opens it for editing.";
export const RECOMMENDED_DETAIL = "Kaiser, automatic length (matches the output), linear phase, flat to 0.908 (20 kHz at 44.1 kHz) and stopped from 1.00 × the source Nyquist; the highest rate of the same family, up to 768 kHz.";

/**
 * The config to send: `preset: "recommended"`; while upsampling is on, also
 * its target (max 768 kHz). While off it stays off: only the filter changes.
 */
export function recommendedUpsampler(cur?: UpsamplerConfig): UpsamplerConfig {
  return {
    target: cur?.target === "off" ? "off" : RECOMMENDED_TARGET,
    preset: RECOMMENDED_PRESET,
    design: null,
  };
}

/**
 * Whether `u` runs the recommended filter: the `"recommended"` design (by
 * name, or implied by neither a preset nor a design), whatever the rate.
 * A custom copy of it is a custom design ("Same as Recommended").
 */
export function isRecommendedUpsampler(u: UpsamplerConfig | null | undefined): boolean {
  if (!u) return false;
  return u.preset === RECOMMENDED_PRESET || (u.preset === null && u.design === null);
}
