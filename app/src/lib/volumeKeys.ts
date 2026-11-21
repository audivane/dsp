// Volume keys, the tray wheel and the on-screen display: what one step
// does to the level, and what the display shows. The write itself is the
// store's guarded volume write (`AppStore.volumeKey`), the same as the
// sliders'.

import type { KeyAction, OsdPayload } from "../api/shell";
import { VOLUME_MAX_DB, VOLUME_MIN_DB, type VolumeStatus } from "../api/types";

export interface Level {
  volume_db: number;
  balance_db: number;
  muted: boolean;
}

/** The range the volume can take: what the engine reports (its minimum and
 *  the ceiling), within the protocol's bounds. */
export function volumeRange(vs: Pick<VolumeStatus, "min_db" | "max_db">): { lo: number; hi: number } {
  const lo = Math.max(vs.min_db ?? VOLUME_MIN_DB, VOLUME_MIN_DB);
  const hi = Math.min(vs.max_db ?? VOLUME_MAX_DB, VOLUME_MAX_DB);
  return { lo, hi: Math.max(lo, hi) };
}

/** Digital volume is never applied on the Direct sink or with Bypass on: a
 *  DAC without a hardware mixer then keeps its own level (bit-perfect). */
export function volumeOnDac(vs: Pick<VolumeStatus, "hardware"> | null | undefined, activeSink: string | null | undefined, dspEnabled: boolean): boolean {
  return !!vs && !vs.hardware && (activeSink === "direct" || !dspEnabled);
}

/**
 * The change one key press makes: mute toggles the mute (and only it); up
 * and down move the volume by `stepDb`, kept in range and on the 0.1 dB
 * grid, and leave the mute alone. Null when nothing would change (at the
 * ceiling, at the bottom).
 */
export function keyPatch(level: Level, vs: Pick<VolumeStatus, "min_db" | "max_db">, action: KeyAction, stepDb: number): Partial<Level> | null {
  if (action === "mute") return { muted: !level.muted };
  const { lo, hi } = volumeRange(vs);
  const raw = level.volume_db + (action === "up" ? stepDb : -stepDb);
  const next = Math.round(Math.min(hi, Math.max(lo, raw)) * 10) / 10;
  // A level outside the range (an older engine's report) only moves inward.
  if (next === level.volume_db || (action === "up" && next < level.volume_db) || (action === "down" && next > level.volume_db)) return null;
  return { volume_db: next };
}

/** What the display shows for a level. */
export function osdPayload(level: Pick<Level, "volume_db" | "muted">, vs: Pick<VolumeStatus, "min_db" | "max_db">): OsdPayload {
  const { lo, hi } = volumeRange(vs);
  return { volume_db: level.volume_db, muted: level.muted, min_db: lo, max_db: hi, note: null };
}

/** The display's text for a level: "−23.5 dB", "Muted", or the note. */
export function osdText(p: OsdPayload, fmt: (db: number) => string): string {
  if (p.note) return p.note;
  if (p.volume_db === null) return "—";
  return p.muted ? `Muted · ${fmt(p.volume_db)} dB` : `${fmt(p.volume_db)} dB`;
}
