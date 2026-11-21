// Measured level matching ("Match to this track", `ab_measure`): the words
// and progress the A/B view shows for a running, locked or failed
// measurement. The engine owns the rule (docs/control-plane.md): at least
// 20 s of A's gated audio, then an offset that moves less than 0.1 dB over
// the last 10 s, or `max_seconds` at the latest.

import type { AbMeasuring, AbStatus, LevelMatch } from "../api/types";
import { fmtDb } from "./format";

/** Seconds of contributing audio before a lock can happen. */
export const MEASURE_MIN_SECONDS = 20;
/** The window the offset must hold still over (less than 0.1 dB) to lock. */
export const MEASURE_STABLE_SECONDS = 10;
export const MEASURE_STABLE_DB = 0.1;
/** `ab_measure`'s `max_seconds` range and default. */
export const MEASURE_MAX_MIN = 10;
export const MEASURE_MAX_MAX = 300;
export const MEASURE_MAX_DEFAULT = 60;

export type MeasurePhase =
  | { phase: "idle" }
  | { phase: "running"; step: "waiting" | "minimum" | "settling" | "locking"; seconds: number; maxSeconds: number; offsetDb: number | null; canAccept: boolean; fraction: number; text: string }
  | { phase: "locked"; offsetDb: number; seconds: number; text: string }
  | { phase: "failed"; text: string };

const secs = (s: number) => `${Math.floor(s)} s`;

/** Where a running measurement stands, in plain words, with a 0–1 fraction for its progress bar. */
export function runningText(m: AbMeasuring): Extract<MeasurePhase, { phase: "running" }> {
  const seconds = Math.max(0, m.seconds);
  const max = m.max_seconds > 0 ? m.max_seconds : MEASURE_MAX_DEFAULT;
  const base = { phase: "running" as const, seconds, maxSeconds: max, offsetDb: m.offset_db, canAccept: m.offset_db !== null };
  if (m.stable) return { ...base, step: "locking", fraction: 1, text: "Settled: locking the offset." };
  if (seconds <= 0 || m.offset_db === null) {
    return { ...base, step: "waiting", fraction: 0, text: "Waiting for music: silence and quiet passages do not count." };
  }
  // The bar spans the minimum plus one settling window (the soonest lock).
  const min = Math.min(MEASURE_MIN_SECONDS, max);
  const fraction = Math.min(0.99, seconds / Math.min(max, min + MEASURE_STABLE_SECONDS));
  if (seconds < min) {
    return { ...base, step: "minimum", fraction, text: `Listening: ${secs(seconds)} of the ${secs(min)} minimum.` };
  }
  // Past the minimum: it locks once the offset holds still for 10 s, or at the cap.
  return {
    ...base,
    step: "settling",
    fraction,
    text: `Settling: ${secs(seconds)} so far. Locks once the offset holds within ${MEASURE_STABLE_DB} dB for ${MEASURE_STABLE_SECONDS} s, or at ${secs(max)}.`,
  };
}

/** `measure_error` in plain words: "No match: side B stayed silent over 60 s of A." */
export function measureErrorText(err: string): string {
  let t = err.trim().replace(/\bB stayed silent\b/, "side B stayed silent");
  if (!t) return "The measurement stopped.";
  t = t[0].toUpperCase() + t.slice(1);
  return /[.!?]$/.test(t) ? t : t + ".";
}

/** "Measured: −0.84 dB over 31.5 s" for a locked level. */
export function measuredText(level: Extract<LevelMatch, { mode: "measured" }>): string {
  return `Measured: B ${fmtDb(level.offset_db, 2)} dB over ${level.seconds.toFixed(1)} s of this track`;
}

/** The measurement's state from A/B status and the level in force. */
export function measurePhase(ab: AbStatus | null | undefined, level: LevelMatch | null | undefined): MeasurePhase {
  if (!ab) return { phase: "idle" };
  if (ab.measuring) return runningText(ab.measuring);
  if (ab.measure_error) return { phase: "failed", text: measureErrorText(ab.measure_error) };
  if (level?.mode === "measured") return { phase: "locked", offsetDb: level.offset_db, seconds: level.seconds, text: measuredText(level) };
  return { phase: "idle" };
}
