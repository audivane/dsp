// Bass and treble knobs (`EqProfile.tone`): two fixed RBJ shelves on every
// channel, after the bands. Constants match crates/protocol (TONE_*).

import type { Band, EqProfile, ToneConfig } from "../api/types";

export const TONE_BASS_HZ = 140;
export const TONE_BASS_Q = 0.48;
export const TONE_TREBLE_HZ = 4900;
export const TONE_TREBLE_Q = 0.49;
export const TONE_MAX_DB = 6;
export const TONE_STEP_DB = 0.5;

/** The shelves the tone adds, as bands: none for a knob at 0 dB. */
export function toneBands(tone: ToneConfig | null | undefined): Band[] {
  const out: Band[] = [];
  if (tone?.bass_db) out.push({ kind: "low_shelf", freq_hz: TONE_BASS_HZ, gain_db: tone.bass_db, q: TONE_BASS_Q, enabled: true });
  if (tone?.treble_db) out.push({ kind: "high_shelf", freq_hz: TONE_TREBLE_HZ, gain_db: tone.treble_db, q: TONE_TREBLE_Q, enabled: true });
  return out;
}

/** Whether either knob is off 0 dB. */
export function toneOn(eq: Pick<EqProfile, "tone"> | null | undefined): boolean {
  return !!eq?.tone && (eq.tone.bass_db !== 0 || eq.tone.treble_db !== 0);
}

/** A knob value snapped to the 0.5 dB step and clamped to ±6 dB. */
export function snapTone(db: number): number {
  if (!Number.isFinite(db)) return 0;
  const v = Math.round(db / TONE_STEP_DB) * TONE_STEP_DB;
  return Math.max(-TONE_MAX_DB, Math.min(TONE_MAX_DB, v)) || 0;
}

/** `eq` with one knob set; a 0/0 tone is dropped. */
export function withTone(eq: EqProfile, knob: "bass_db" | "treble_db", db: number): EqProfile {
  const tone = { bass_db: 0, treble_db: 0, ...eq.tone, [knob]: snapTone(db) };
  const next: EqProfile = { ...eq, tone };
  if (tone.bass_db === 0 && tone.treble_db === 0) delete next.tone;
  return next;
}

/** Whether `band` is the tone shelf of its kind (as the APO export writes it). */
export function isToneShelf(b: Band): "bass_db" | "treble_db" | null {
  const stepOk = b.gain_db !== 0 && Math.abs(b.gain_db) <= TONE_MAX_DB && Number.isInteger(b.gain_db * 2);
  if (!b.enabled || !stepOk) return null;
  if (b.kind === "low_shelf" && b.freq_hz === TONE_BASS_HZ && b.q === TONE_BASS_Q) return "bass_db";
  if (b.kind === "high_shelf" && b.freq_hz === TONE_TREBLE_HZ && b.q === TONE_TREBLE_Q) return "treble_db";
  return null;
}
