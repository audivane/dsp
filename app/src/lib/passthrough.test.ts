import { describe, expect, it } from "vitest";
import type { Passthrough, StreamInfo } from "../api/types";
import { breakText, caveatText, verdict } from "./passthrough";

const p = (broken_because: Passthrough["broken_because"], caveats: Passthrough["caveats"] = []): Passthrough => ({
  intact: broken_because.length === 0,
  broken_because,
  caveats,
});
const firefox: StreamInfo = { node: 94, app_name: "Firefox", binary: "firefox", media_name: null, rate: 48000, channels: 2, format: "F32LE", sink: "dsp", volume: 0.8, muted: false };

describe("passthrough badge", () => {
  it("is green when intact", () => {
    const v = verdict(p([]));
    expect(v).toMatchObject({ kind: "ok", label: "Bit-perfect" });
  });

  it("keeps caveats as reasons but stays green", () => {
    const v = verdict(p([], [{ reason: "unknown_source_precision", format: "S32LE" }]));
    expect(v.kind).toBe("ok");
    expect(v.reasons).toEqual(["Source precision unknown (the player sends a S32LE container)"]);
  });

  it("is blue when processing is chosen", () => {
    const v = verdict(p([{ reason: "dsp_active" }, { reason: "upsampling", ratio: 4 }, { reason: "mixing", streams: 2 }]));
    expect(v).toMatchObject({ kind: "proc", label: "DSP active" });
    expect(v.reasons).toEqual(["DSP processing is on", "Upsampling ×4", "2 streams are mixed together"]);
  });

  it("is amber for unexpected breaks", () => {
    const v = verdict(p([{ reason: "volume", node: 94, volume: 0.512 }, { reason: "resampled", stream_rate: 44100, graph_rate: 48000 }]), [firefox]);
    expect(v).toMatchObject({ kind: "warn", label: "Not bit-perfect" });
    // Linear 0.512 is 80% on a desktop volume slider (cubic scale).
    expect(v.reasons).toEqual(["Firefox volume is at 80%, not 100%", "PipeWire resamples 44.1 kHz to 48 kHz"]);
  });

  it("is never green without an open output", () => {
    const v = verdict(p([{ reason: "no_output" }]));
    expect(v).toMatchObject({ kind: "warn", label: "Not bit-perfect" });
  });

  it("is neutral when nothing plays", () => {
    expect(verdict(p([{ reason: "not_playing" }]))).toMatchObject({ kind: "idle", label: "Nothing playing" });
    expect(verdict(null).kind).toBe("idle");
  });

  it("words every break without codes", () => {
    const all: Passthrough["broken_because"] = [
      { reason: "not_playing" },
      { reason: "dsp_active" },
      { reason: "upsampling", ratio: 2 },
      { reason: "volume", node: 1, volume: 0 },
      { reason: "mixing", streams: 3 },
      { reason: "resampled", stream_rate: 96000, graph_rate: 48000 },
      { reason: "channel_mix", stream_channels: 6, sink_channels: 2 },
      { reason: "source_over24_bit" },
      { reason: "truncated", source_bits: 24, output_bits: 16 },
      { reason: "no_output" },
    ];
    const texts = all.map((b) => breakText(b));
    expect(texts).toEqual([
      "Nothing is playing",
      "DSP processing is on",
      "Upsampling ×2",
      "the Audivane output is muted",
      "3 streams are mixed together",
      "PipeWire resamples 96 kHz to 48 kHz",
      "6-channel audio is remixed to 2 channels",
      "The source has more than 24 bits",
      "24-bit audio is cut to 16 bits for the DAC",
      "The output is not playing: nothing reaches the DAC",
    ]);
    for (const t of texts) expect(t).not.toMatch(/_/);
    expect(caveatText({ reason: "unknown_source_precision", format: "F32LE" })).toContain("F32LE");
  });
});
