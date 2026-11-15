// Upsampler design limits, mirrored from dsp-core so the UI can disable an
// option the daemon would reject (INVALID_PARAMS) instead of failing on
// click. Keep in sync with `crates/dsp-core/src/resample.rs`
// (`nyquist_stop_*`), `crates/dsp-core/src/minphase.rs`
// (`MIN/MAX_MINIMUM_PHASE_BETA`) and `validate_upsampler` in the daemon; the
// shared `NYQUIST_STOP_MIN_TAPS_CASES` below are asserted on both sides.

import type { RateTarget } from "../api/generated/RateTarget";

/** Source rates the engine runs at (daemon `ALLOWED_RATES`). */
export const ALLOWED_SOURCE_RATES = [44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000] as const;

/** Kaiser beta range for minimum phase and for "Stop at Nyquist". */
export const MIN_PHASE_BETA = [2, 20] as const;
export const NYQUIST_STOP_BETA = [5, 24] as const;
/** Smallest "Stop at Nyquist" cutoff (fraction of the source Nyquist). */
export const NYQUIST_STOP_MIN_CUTOFF = 0.5;
const SHORT_GAIN = 0.7;

/** (ratio, beta, fewest feasible taps): also asserted in the Rust tests. */
export const NYQUIST_STOP_MIN_TAPS_CASES: [number, number, number][] = [
  [2, 8, 32],
  [16, 14, 433],
  [32, 20, 1254],
  [32, 24, 1653],
  [16, 5, 157],
];

export function kaiserAttenuationDb(beta: number): number {
  return beta / 0.1102 + 8.7;
}

function safetyK(beta: number): number {
  return Math.max(1.19, 1.21 + 0.03 * (beta - 20));
}

function rawCutoff(taps: number, ratio: number, beta: number): number {
  const x = (ratio * Math.max(0, kaiserAttenuationDb(beta) - 7.95)) / (14.36 * (Math.max(taps, 2) - 1));
  const y = safetyK(beta) * x;
  return 1 - y * (1 + SHORT_GAIN * y);
}

export function resolveRatio(target: RateTarget, sourceRate: number): number {
  if (target === "off") return 1;
  if ("ratio" in target) return Math.max(1, target.ratio);
  let r = 1;
  while (sourceRate * r * 2 <= target.max_rate) r *= 2;
  return r;
}

/** Largest ratio a target reaches over the allowed source rates. */
export function worstCaseRatio(target: RateTarget): number {
  return Math.max(...ALLOWED_SOURCE_RATES.map((rate) => resolveRatio(target, rate)));
}

const inRange = (beta: number, [lo, hi]: readonly [number, number]) => beta >= lo && beta <= hi;

/** Fewest taps for which "Stop at Nyquist" is feasible, or null (beta out of range). */
export function nyquistStopMinTaps(ratio: number, beta: number): number | null {
  if (!inRange(beta, NYQUIST_STOP_BETA)) return null;
  if (ratio <= 1) return 1;
  let taps = 2;
  // Bisect on the monotone cutoff(taps).
  let hi = 2;
  while (rawCutoff(hi, ratio, beta) < NYQUIST_STOP_MIN_CUTOFF) hi *= 2;
  let lo = Math.max(2, Math.floor(hi / 2));
  if (rawCutoff(lo, ratio, beta) >= NYQUIST_STOP_MIN_CUTOFF) return lo;
  while (hi - lo > 1) {
    const mid = Math.floor((lo + hi) / 2);
    if (rawCutoff(mid, ratio, beta) >= NYQUIST_STOP_MIN_CUTOFF) hi = mid;
    else lo = mid;
  }
  taps = hi;
  return taps;
}

/** The "Stop at Nyquist" cutoff dsp-core uses (clamped like `nyquist_stop_cutoff`). */
export function nyquistStopCutoff(taps: number, ratio: number, beta: number): number {
  if (ratio <= 1) return 1;
  if (taps <= 1) return NYQUIST_STOP_MIN_CUTOFF;
  const b = Math.min(NYQUIST_STOP_BETA[1], Math.max(NYQUIST_STOP_BETA[0], beta));
  return Math.min(1, Math.max(NYQUIST_STOP_MIN_CUTOFF, rawCutoff(taps, ratio, b)));
}

/** Why "Stop at Nyquist" is not available for these settings, or null. */
export function nyquistStopBlockReason(u: { target: RateTarget; taps: number; kaiser_beta: number }): string | null {
  // Mirrors the daemon: with upsampling off (ratio ≤ 1) there is no filter, so any β is accepted.
  const ratio = worstCaseRatio(u.target);
  if (ratio <= 1) return null;
  const [lo, hi] = NYQUIST_STOP_BETA;
  if (!inRange(u.kaiser_beta, NYQUIST_STOP_BETA)) {
    return `Stop at Nyquist needs a Kaiser β between ${lo} and ${hi} (now ${u.kaiser_beta}).`;
  }
  const min = nyquistStopMinTaps(ratio, u.kaiser_beta)!;
  if (u.taps < min) {
    return `Stop at Nyquist is only guaranteed from ${min.toLocaleString("en-US")} taps at ×${ratio} (now ${u.taps.toLocaleString("en-US")}): below that the calibrated cutoff formula cannot promise the stopband at the source Nyquist.`;
  }
  return null;
}

/** Why minimum phase is not available for this beta, or null. */
export function minimumPhaseBlockReason(beta: number): string | null {
  const [lo, hi] = MIN_PHASE_BETA;
  return inRange(beta, MIN_PHASE_BETA) ? null : `Minimum phase needs a Kaiser β between ${lo} and ${hi} (now ${beta}).`;
}

export interface KaiserBetaPreset {
  readonly beta: number;
  readonly label: string;
}

export const DEFAULT_KAISER_BETA = 14;

export const KAISER_BETA_PRESETS: readonly KaiserBetaPreset[] = [
  { beta: 10, label: "Steeper" },
  { beta: 14, label: "Standard" },
  { beta: 17, label: "Deeper" },
  { beta: 20, label: "Deepest" },
  // Linear phase only: minimum phase is designed for β 2..20 (blocked through
  // `kaiserBetaBlockReason`); 24 is also the top of the Stop at Nyquist range.
  { beta: 24, label: "Extreme" },
] as const;

/** Matching preset β, or null when a custom β (e.g. from CLI) is active. */
export function selectedKaiserPreset(beta: number): number | null {
  if (!Number.isFinite(beta)) return null;
  const hit = KAISER_BETA_PRESETS.find((p) => Math.abs(p.beta - beta) < 1e-9);
  return hit ? hit.beta : null;
}

/** Plain-language tooltip for a Kaiser β preset, computed from `kaiserAttenuationDb`. */
export function kaiserPresetTooltip(beta: number, label: string): string {
  const db = Math.round(kaiserAttenuationDb(beta));
  const audibleRefDb = Math.round(kaiserAttenuationDb(DEFAULT_KAISER_BETA));
  const trade =
    beta < DEFAULT_KAISER_BETA
      ? "narrowest transition from passband to stopband for the same tap count, trading a shallower stopband"
      : beta === DEFAULT_KAISER_BETA
        ? "default balance of transition width and stopband depth"
        : "deeper stopband against a wider transition from passband to stopband";
  const def = beta === DEFAULT_KAISER_BETA ? ", default" : "";
  return `${label} (β = ${beta}, ≈ ${db} dB stopband${def}): ${trade}. Past about ${audibleRefDb} dB the images already sit below the analog noise of real DACs (typically 110–130 dB), so deeper settings are measurable rather than audible (ideal 24-bit quantization is about 146 dB).`;
}

/** Why a candidate Kaiser β would make the current phase or cutoff mode invalid, or null. */
export function kaiserBetaBlockReason(
  u: { target: RateTarget; taps: number; phase: "linear" | "minimum"; cutoff_mode: "fraction" | "nyquist_stop" },
  beta: number,
): string | null {
  const reasons: string[] = [];
  if (u.phase === "minimum") {
    const r = minimumPhaseBlockReason(beta);
    if (r) reasons.push(r);
  }
  if (u.cutoff_mode === "nyquist_stop") {
    const r = nyquistStopBlockReason({ target: u.target, taps: u.taps, kaiser_beta: beta });
    if (r) reasons.push(r);
  }
  return reasons.length > 0 ? reasons.join(" ") : null;
}

/** Kaiser empirical transition width Δf in Hz for `taps` at `outputRate`. */
export function kaiserTransitionWidthHz(beta: number, taps: number, outputRate: number): number | null {
  if (!Number.isFinite(beta) || !Number.isFinite(taps) || !Number.isFinite(outputRate)) return null;
  if (taps <= 1 || outputRate <= 0) return null;
  const atten = kaiserAttenuationDb(beta);
  return (Math.max(0, atten - 7.95) / (14.36 * (taps - 1))) * outputRate;
}
