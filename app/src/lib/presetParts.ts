// Parts of chain A that each page's preset picker saves and loads on its own
// (`PresetPart` in docs/control-plane.md "Presets"). Mirrors the protocol's
// `PresetPart::copy` / `same`, for the mock and for composing side B.

import type { ChainConfig, Preset, PresetInfo, PresetPart } from "../api/types";
import { deepEqual } from "./chain";

/** Every part, in the sidebar's order (the output-side pages share one). */
export const PRESET_PARTS: PresetPart[] = ["stereo", "eq", "convolution", "upsampling", "harmonics", "output"];

export const PART_NAME: Record<PresetPart, string> = {
  eq: "Parametric EQ",
  convolution: "Convolution",
  stereo: "Stereo image",
  harmonics: "Harmonics",
  upsampling: "Upsampling",
  output: "Output",
};

/** What each part holds, for tooltips (the output part spans four pages). */
export const PART_HOLDS: Record<PresetPart, string> = {
  eq: "bands, channel sections, preamp, tone and tilt",
  convolution: "the impulse responses",
  stereo: "crossfeed and width",
  harmonics: "the harmonics layers",
  upsampling: "the rate and filter",
  output: "DC block, pitch, limiter and headroom, output bits and dither",
};

/** The chain keys each part owns. */
const PART_KEYS: Record<PresetPart, string[]> = {
  eq: ["eq"],
  convolution: ["convolution"],
  stereo: ["crossfeed"],
  harmonics: ["harmonics"],
  upsampling: ["upsampler"],
  output: ["dc_block", "pitch", "limiter", "output"],
};

type Mutable = Record<string, unknown>;

/** Copy `part` of `from` into `to` (in place), leaving the rest of `to` alone. */
export function copyPart(part: PresetPart, from: ChainConfig, to: ChainConfig): void {
  const f = from as unknown as Mutable;
  const t = to as unknown as Mutable;
  for (const k of PART_KEYS[part]) {
    if (f[k] === undefined) delete t[k];
    else t[k] = structuredClone(f[k]);
  }
}

/** Whether `a` and `b` agree on `part` (an absent optional stage equals null). */
export function samePart(part: PresetPart, a: ChainConfig, b: ChainConfig): boolean {
  const x = a as unknown as Mutable;
  const y = b as unknown as Mutable;
  return PART_KEYS[part].every((k) => deepEqual(x[k] ?? null, y[k] ?? null));
}

/** The parts a preset (or its list entry) holds; older engines send none: every part. */
export function presetCovers(p: Pick<Preset, "part" | "parts"> | Pick<PresetInfo, "part" | "parts">): PresetPart[] {
  if (p.part) return [p.part];
  const parts = p.parts;
  return parts && parts.length ? PRESET_PARTS.filter((x) => parts.includes(x)) : [...PRESET_PARTS];
}

/** `chain` with `preset`'s parts applied, the rest as it was. */
export function applyPreset(chain: ChainConfig, preset: Preset): ChainConfig {
  const out = structuredClone(chain);
  for (const part of presetCovers(preset)) copyPart(part, preset.chain, out);
  return out;
}

/** Whether `chain` matches `preset` on every part it holds. */
export function presetMatches(preset: Preset, chain: ChainConfig): boolean {
  return presetCovers(preset).every((part) => samePart(part, preset.chain, chain));
}
