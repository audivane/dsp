// The engine's bounds for the volume settings (control-plane.md, "Volume,
// balance, and mute"), checked before anything is sent so the message sits
// beside the field it is about.

import type { VolumeConfig, VolumeMode } from "../api/types";

export type VolumeSettings = Omit<VolumeConfig, "volume_db" | "balance_db" | "muted">;
export type VolumeField = "hardware" | "ceiling" | "rise" | "stream";

export const VOLUME_BOUNDS = {
  hardware: { min: -120, max: 0 },
  ceiling: { min: -120, max: 0 },
  rise: { min: 0, max: 1000 },
  stream: { min: -120, max: 0 },
} as const;

const inRange = (v: number | null, f: VolumeField) => v === null || (Number.isFinite(v) && v >= VOLUME_BOUNDS[f].min && v <= VOLUME_BOUNDS[f].max);

/** The message for one field's value, or null. */
export function volumeFieldProblem(field: VolumeField, v: number | null, mode?: VolumeConfig["mode"]): string | null {
  const { min, max } = VOLUME_BOUNDS[field];
  if (field === "hardware" && v === null && mode === "hybrid") return "Hybrid needs a DAC level.";
  if (field === "rise" && v === null) return `Enter ${min} to ${max} dB/s.`;
  if (!inRange(v, field)) return field === "rise" ? `Must be ${min} to ${max} dB/s.` : `Must be ${min} to ${max} dB.`;
  return null;
}

/** The first field the engine would refuse, with why, or null. */
export function volumeSettingsProblem(v: VolumeSettings): { field: VolumeField; text: string } | null {
  const checks: [VolumeField, number | null][] = [
    ["hardware", v.hardware_db],
    ["ceiling", v.max_volume_db],
    ["rise", v.guard.max_rise_db_per_s],
    ["stream", v.guard.stream_max_db],
  ];
  for (const [field, value] of checks) {
    const text = volumeFieldProblem(field, value, v.mode);
    if (text) return { field, text };
  }
  return null;
}

/** The rise rate "No rise limit" goes back to when nothing was remembered, dB/s. */
export const DEFAULT_RISE_DB_PER_S = 6;

/**
 * The guard with the rise limit turned off (`max_rise_db_per_s` 0: only the
 * caps apply) or back on at `restore` (the last non-zero rate, else 6 dB/s).
 */
export function guardRiseLimit<G extends { max_rise_db_per_s: number }>(guard: G, limited: boolean, restore: number | null): G {
  if (!limited) return { ...guard, max_rise_db_per_s: 0 };
  if (guard.max_rise_db_per_s > 0) return guard;
  const r = restore !== null && Number.isFinite(restore) && restore > 0 && restore <= VOLUME_BOUNDS.rise.max ? restore : DEFAULT_RISE_DB_PER_S;
  return { ...guard, max_rise_db_per_s: r };
}

/**
 * Next `VolumeConfig` when switching `mode` in the UI:
 * - `hybrid` with no `hardware_db` prefills the current DAC level (or -10 dB).
 * - `software` from another mode with no `hardware_db` defaults the DAC mixer
 *   to 0 dB so attenuation is not silently doubled (the engine stages the raise).
 */
export function volumeModeChange(v: VolumeConfig, mode: VolumeMode, statusHardwareDb?: number | null): VolumeConfig {
  let hardware_db = v.hardware_db;
  if (mode === "hybrid" && hardware_db === null) {
    hardware_db = Math.min(0, statusHardwareDb ?? -10);
  } else if (mode === "software" && v.mode !== "software" && hardware_db === null) {
    hardware_db = 0;
  }
  return { ...v, mode, hardware_db };
}

/**
 * True when in `software` mode with no fixed `hardware_db` and the DAC mixer is
 * still attenuated below 0 dB (doubling the digital attenuation).
 */
export function softwareMixerAttenuated(vol: Pick<VolumeConfig, "mode" | "hardware_db">, statusHardwareDb?: number | null): boolean {
  return vol.mode === "software" && vol.hardware_db === null && statusHardwareDb != null && statusHardwareDb < -0.5;
}
