// Plain-word explanations for the daemon's passthrough verdict. Nothing here
// shows internal codes; every badge pairs its colour with text.

import type { Passthrough, PassthroughBreak, PassthroughCaveat, StreamInfo } from "../api/types";
import { fmtRate } from "./format";

export type BadgeKind = "ok" | "proc" | "warn" | "idle";

export interface Verdict {
  kind: BadgeKind;
  label: string;
  /** One sentence for the tooltip. */
  summary: string;
  /** Plain-word reasons (breaks first, then caveats). */
  reasons: string[];
}

function streamName(node: number, streams: readonly StreamInfo[]): string {
  const s = streams.find((x) => x.node === node);
  if (!s) return "the Audivane output";
  return s.app_name || s.binary || s.media_name || "a stream";
}

/**
 * A linear PipeWire volume as desktop volume sliders show it (pavucontrol,
 * KDE and GNOME use a cubic scale): linear 0.27 reads 65%.
 */
export function sliderPercent(volume: number): number {
  return Math.round(Math.cbrt(Math.max(0, volume)) * 100);
}

/** One break in plain words. */
export function breakText(b: PassthroughBreak, streams: readonly StreamInfo[] = []): string {
  switch (b.reason) {
    case "not_playing":
      return "Nothing is playing";
    case "no_output":
      return "The output is not playing: nothing reaches the DAC";
    case "dsp_active":
      return "DSP processing is on";
    case "upsampling":
      return `Upsampling ×${b.ratio}`;
    case "rate_conversion":
      return `Resampling ${fmtRate(b.graph_rate)} to ${fmtRate(b.output_rate)}`;
    case "volume": {
      const who = streamName(b.node, streams);
      if (b.volume === 0) return `${who} is muted`;
      return `${who} volume is at ${sliderPercent(b.volume)}%, not 100%`;
    }
    case "mixing":
      return `${b.streams} streams are mixed together`;
    case "resampled":
      return `PipeWire resamples ${fmtRate(b.stream_rate)} to ${fmtRate(b.graph_rate)}`;
    case "channel_mix":
      return `${b.stream_channels}-channel audio is remixed to ${b.sink_channels} channels`;
    case "source_over24_bit":
      return "The source has more than 24 bits";
    case "truncated":
      return `${b.source_bits}-bit audio is cut to ${b.output_bits} bits for the DAC`;
    default:
      return "The output differs from the source";
  }
}

export function caveatText(c: PassthroughCaveat): string {
  switch (c.reason) {
    case "unknown_source_precision":
      return `Source precision unknown (the player sends a ${c.format} container)`;
    default:
      return "Some details could not be checked";
  }
}

const CHOSEN = new Set<PassthroughBreak["reason"]>(["dsp_active", "upsampling"]);

/** Turn the daemon's verdict into a badge. */
export function verdict(p: Passthrough | null | undefined, streams: readonly StreamInfo[] = []): Verdict {
  if (!p) return { kind: "idle", label: "Unknown", summary: "The engine has not reported its output yet.", reasons: [] };
  const breaks = p.broken_because;
  const reasons = [...breaks.filter((b) => b.reason !== "not_playing").map((b) => breakText(b, streams))];
  const caveats = (p.caveats ?? []).map(caveatText);
  if (p.intact || breaks.length === 0) {
    return {
      kind: "ok",
      label: "Bit-perfect",
      summary: "The DAC receives exactly the samples the player sent.",
      reasons: caveats,
    };
  }
  const notPlaying = breaks.some((b) => b.reason === "not_playing");
  const chosen = breaks.some((b) => CHOSEN.has(b.reason));
  const unexpected = breaks.filter((b) => !CHOSEN.has(b.reason) && b.reason !== "not_playing");
  if (chosen) {
    return {
      kind: "proc",
      label: "DSP active",
      summary: "The sound is processed as you set it up. Turn on Bypass for bit-perfect output.",
      reasons: [...reasons, ...caveats],
    };
  }
  if (unexpected.length === 0 && notPlaying) {
    return { kind: "idle", label: "Nothing playing", summary: "No stream is feeding the engine.", reasons: caveats };
  }
  return {
    kind: "warn",
    label: "Not bit-perfect",
    summary: "Something changes the samples on the way to the DAC.",
    reasons: [...reasons, ...caveats],
  };
}
