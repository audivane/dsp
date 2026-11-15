import { describe, expect, it } from "vitest";
import { sinkOptions } from "./routing";

describe("routing sink options", () => {
  it("offers only Audivane's sinks in system-wide routing (the default is Audivane DSP)", () => {
    expect(sinkOptions(false, null).map(([k]) => k)).toEqual(["dsp", "direct"]);
    expect(sinkOptions(false, "Built-in Audio").map(([k]) => k)).toEqual(["dsp", "direct"]);
  });

  it("names the output an unrouted stream reaches in per-app routing", () => {
    expect(sinkOptions(true, "Built-in Audio Analog Stereo")).toEqual([
      ["dsp", "Audivane DSP"],
      ["direct", "Audivane Direct (bit-perfect)"],
      ["default", "Not through Audivane (Built-in Audio Analog Stereo)"],
    ]);
    expect(sinkOptions(true, null)[2]).toEqual(["default", "Not through Audivane (no other output)"]);
  });
});
