// Varispeed helpers: tape-style speed change (tempo and pitch together), or
// with Keep pitch, the tempo only (a time stretch).

import type { VarispeedConfig, VarispeedStatus } from "../api/types";

export const DEFAULT_VARISPEED: VarispeedConfig = { enabled: false, speed: 1, ramp_ms: 200, keep_pitch: false };

/** Beyond this distance from 1× the time stretch audibly softens transients. */
export const SOFTEN_BEYOND = 0.25;

/** Whether the engine knows Keep pitch (an older one sends no `keep_pitch`). */
export function keepPitchSupported(v: VarispeedConfig | null | undefined): boolean {
  return typeof v?.keep_pitch === "boolean";
}

/** The "transients soften" note applies: Keep pitch on, more than ±25 % away. */
export function transientsSoften(speed: number, keepPitch: boolean): boolean {
  return keepPitch && Math.abs(speed - 1) > SOFTEN_BEYOND + 1e-9;
}

/** The slider ranges: ±8 % and ±16 % as on a turntable, and 0.5×–1.5×. */
export type VarispeedRange = 8 | 16 | 50;
export const VARISPEED_RANGES: readonly VarispeedRange[] = [8, 16, 50];

export function rangeBounds(r: VarispeedRange): { min: number; max: number } {
  return { min: 1 - r / 100, max: 1 + r / 100 };
}

/** Clamp a speed into a range (non-finite values go back to 1×). */
export function clampSpeed(speed: number, r: VarispeedRange): number {
  if (!Number.isFinite(speed)) return 1;
  const { min, max } = rangeBounds(r);
  return Math.min(max, Math.max(min, speed));
}

/** The narrowest range that holds the speed (rounding noise allowed). */
export function rangeFor(speed: number): VarispeedRange {
  const d = Math.abs(speed - 1);
  return VARISPEED_RANGES.find((r) => d <= r / 100 + 1e-9) ?? 50;
}

/** `12·log2(speed)`: the pitch change in semitones. */
export function speedSemitones(speed: number): number {
  return 12 * Math.log2(speed);
}

function signed(v: number, digits: number): string {
  const s = v.toFixed(digits);
  // No "-0.0": a value that rounds to zero shows as ±0.
  if (Number(s) === 0) return `±${(0).toFixed(digits)}`;
  return v > 0 ? `+${s}` : s.replace("-", "−");
}

/** 1.02 → "+2.0 %", 0.95 → "−5.0 %", 1 → "±0.0 %". */
export function fmtSpeedPercent(speed: number): string {
  return `${signed((speed - 1) * 100, 1)} %`;
}

/** 1.02 → "+0.34 st". */
export function fmtSemitones(speed: number): string {
  return `${signed(speedSemitones(speed), 2)} st`;
}

/** The readout: "+2.0 % · +0.34 st", or with Keep pitch "+2.0 % · pitch held". */
export function speedReadout(speed: number, keepPitch: boolean): string {
  return `${fmtSpeedPercent(speed)} · ${keepPitch ? "pitch held" : fmtSemitones(speed)}`;
}

/** Why the speed is capped or held, in plain words, or null when it is not. */
export function varispeedNote(s: VarispeedStatus | null | undefined): string | null {
  if (!s || !s.enabled) return null;
  if (s.error && s.keep_pitch)
    return "Keep pitch needs more CPU than this sample rate allows (it fits up to 96 kHz), so the music plays at normal speed. Turn Keep pitch off to change the speed here.";
  if (s.error) return `Varispeed is off at this rate: ${s.error}`;
  if (s.overload_hold === "off") return "The engine could not keep up, so varispeed is off until you change the speed.";
  if (s.overload_hold === "unity") return "The engine could not keep up, so the speed is held at 1× until you change it.";
  if (s.held === "underfeed") return "The source could not keep up, so the speed is held at 1× until you change it.";
  const capped = s.requested > s.speed_limit + 1e-9;
  if (!capped) return null;
  const limit = `${fmtSpeedPercent(s.speed_limit)} (${s.speed_limit.toFixed(3)}×)`;
  if (s.overload_hold === "budget") return `The engine could not keep up, so the speed is capped at ${limit}.`;
  switch (s.cap_reason) {
    case "period_quantum":
    case "quantum_too_small":
      return `The buffer size (quantum) is too small for this speed: capped at ${limit}. A larger quantum allows more.`;
    case "ring_size":
      return `The engine's buffer is too small for this speed: capped at ${limit}.`;
    default:
      return `The speed is capped at ${limit}.`;
  }
}
