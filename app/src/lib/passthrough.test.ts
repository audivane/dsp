import { describe, expect, it } from "vitest";
import type { Passthrough, StreamInfo } from "../api/types";
import { verdict } from "./passthrough";

const p = (broken_because: Passthrough["broken_because"], caveats: Passthrough["caveats"] = []): Passthrough => ({
  intact: broken_because.length === 0,
  broken_because,
  caveats,
});
const firefox: StreamInfo = { node: 94, app_name: "Firefox", binary: "firefox", media_name: null, rate: 48000, channels: 2, format: "F32LE", sink: "dsp", volume: 0.8, muted: false };

describe("passthrough badge", () => {

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

});
