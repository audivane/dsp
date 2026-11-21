// Per-app stream volume (`set_stream_volume`): the pure parts the store and
// the Routing view share.

import type { StreamInfo, StreamVolume } from "../api/types";
import { fmtDb } from "./format";

/** An edit of one stream: only the fields present change. */
export interface StreamLevelEdit {
  volume_db?: number;
  muted?: boolean;
}

/** A note or an error beside a stream's volume control. */
export interface StreamMessage {
  text: string;
  error: boolean;
}

export const STREAM_VOLUME_NEEDS_UPDATE = "This engine version cannot set an app's volume. Update the engine to use it.";

/** The engine reports stream levels in dB (`volume_db`), so it has the call. */
export function reportsStreamLevels(streams: StreamInfo[]): boolean {
  return streams.some((s) => typeof s.volume_db === "number");
}

/**
 * Why a stream's volume cannot be set here, or null when it can.
 * `cap`: the engine has the call (true), lacks it (false), or not known yet.
 */
export function streamVolumeBlock(s: StreamInfo, cap: boolean | null): string | null {
  if (cap === false) return STREAM_VOLUME_NEEDS_UPDATE;
  if (s.sink !== "dsp" && s.sink !== "direct") {
    return "This stream is not playing through Audivane right now (it plays elsewhere, or is paused), so its volume can only be changed in the app or the desktop mixer.";
  }
  if (typeof s.volume_db !== "number") {
    return cap === true ? "This stream has not reported its volume yet." : STREAM_VOLUME_NEEDS_UPDATE;
  }
  return null;
}

/** What the engine's answer says about a level that was asked for: a note
 *  when it played lower than asked, else null. */
export function streamLimitNote(r: StreamVolume): string | null {
  const at = `${fmtDb(r.volume_db, 1, false)} dB`;
  switch (r.limited_by) {
    case "stream_max":
      return `Held at ${at}: the app stream cap (Device, Volume settings).`;
    case "app_volume":
      return `Held at ${at}: the app's own volume is lower. Raise it in the app to go higher.`;
    default:
      return null;
  }
}

/** The readout: dB only (the desktop's percent is on another scale). */
export function streamDbText(db: number | null): string {
  if (db === null) return "—";
  if (db <= -120) return "−∞ dB";
  return `${fmtDb(db, 1, false)} dB`;
}
