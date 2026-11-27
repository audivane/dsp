// A/B side B as a JSON merge patch on chain A: the daemon's
// `control/ab_side.rs`, mirrored for the mock daemon and the UI.

import type { AbB, AbConfig, ChainConfig } from "../api/types";

type Json = null | boolean | number | string | Json[] | { [k: string]: Json };

function isObject(v: unknown): v is Record<string, Json> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/**
 * RFC 7386 merge, plus the daemon's rule: a patch that only removes fields
 * from a stage chain A lacks leaves that stage absent.
 */
export function mergePatch(target: unknown, patch: unknown): unknown {
  if (!isObject(patch)) return structuredClone(patch);
  const absent = target === null || target === undefined;
  const out: Record<string, unknown> = isObject(target) ? { ...target } : {};
  for (const [k, v] of Object.entries(patch)) {
    if (v === null) delete out[k];
    else out[k] = mergePatch(out[k] ?? null, v);
  }
  if (absent && Object.keys(out).length === 0 && Object.keys(patch).length > 0) return null;
  return out;
}

/** Why a patch is refused (the daemon's `check_patch`), or null. */
export function patchError(patch: unknown): string | null {
  if (!isObject(patch)) return "ab.b.patch must be a JSON object";
  if ("limiter" in patch) return "ab.b.patch.limiter may not differ from chain A (both sides share the limiter after the mix)";
  if ("output" in patch) {
    if (!isObject(patch.output)) return "ab.b.patch.output must be an object (side B may set its own bits and dither)";
    const bad = Object.keys(patch.output).find((k) => k !== "bits" && k !== "dither");
    if (bad) {
      const why =
        bad === "headroom_db"
          ? "both sides take the same output headroom before the mix"
          : "side B may set only its own bits and dither; the device, its format and the rate are shared";
      return `ab.b.patch.output.${bad} may not differ from chain A (${why})`;
    }
  }
  if ("upsampler" in patch) {
    if (!isObject(patch.upsampler)) return "ab.b.patch.upsampler must be an object (B runs at chain A's ratio; a filter may differ)";
    if ("target" in patch.upsampler) {
      return "ab.b.patch.upsampler.target may not differ from chain A (a ratio compare needs the DAC reopened, not supported in A/B yet)";
    }
  }
  return null;
}

/** Chain B for `b` against chain A (no type checks beyond the patch rules). */
export function materializeB(a: ChainConfig, b: AbB): ChainConfig {
  if ("chain" in b) {
    const chain = structuredClone(b.chain);
    return {
      ...chain,
      upsampler: { ...chain.upsampler, target: structuredClone(a.upsampler.target) },
      limiter: structuredClone(a.limiter),
      output: structuredClone(a.output),
    };
  }
  const err = patchError(b.patch);
  if (err) throw new Error(err);
  return mergePatch(structuredClone(a), b.patch) as ChainConfig;
}

/**
 * Chain B against chain `a` as it is now (a client editing A sees B follow
 * before the daemon answers), else the daemon's last `chain_b`.
 */
export function chainB(ab: AbConfig, a: ChainConfig): ChainConfig {
  try {
    return materializeB(a, ab.b);
  } catch {
    return ab.chain_b ?? a;
  }
}

/** The "Flat" side B: chain A without EQ and convolution. */
export const FLAT_PATCH = { eq: null, convolution: [] };

/** What side B is called: its label, else a preset's EQ name, else a generic name. */
export function abLabel(ab: AbConfig): string {
  if (ab.label) return ab.label;
  if ("chain" in ab.b) return ab.b.chain.eq?.name || "Preset";
  return JSON.stringify(ab.b.patch) === JSON.stringify(FLAT_PATCH) ? "Flat" : "Changed";
}
