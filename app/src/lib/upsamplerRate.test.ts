import { describe, expect, it } from "vitest";
import type { EngineState } from "../api/types";
import { chosenPreviewRate, viewSourceRate } from "./upsamplerRate";
import { previewRates } from "./rates";

const DEFAULT = [44100, 48000, 88200, 96000, 176400, 192000];

/** Feed a status sequence through the view's rate logic like the component does. */
function run(steps: { state: EngineState; graph: number | null; eff: number | null }[], rates: number[] = DEFAULT): number[] {
  let held: number | null = null;
  return steps.map(({ state, graph, eff }) => {
    const rate = viewSourceRate({ state, graphRate: graph, effectiveDefaultRate: eff, held, rates });
    if (state !== "switching") held = rate;
    return rate;
  });
}

describe("upsampling view source rate", () => {
  it("holds the rate through a reopen (switching -> playing)", () => {
    // 48k-only device playing; a filter-length change reopens the DAC. The
    // status may report a transient graph rate while switching.
    const rates = run([
      { state: "playing", graph: 48000, eff: 48000 },
      { state: "switching", graph: 44100, eff: 48000 },
      { state: "switching", graph: null, eff: null },
      { state: "playing", graph: 48000, eff: 48000 },
    ]);
    expect(rates).toEqual([48000, 48000, 48000, 48000]);
  });

  it("uses the device default when idle, not a hard-coded 44.1k", () => {
    expect(run([{ state: "ready", graph: 48000, eff: 48000 }])).toEqual([48000]);
    expect(run([{ state: "ready", graph: 48000, eff: null }])).toEqual([44100]);
    // Ready -> switching -> ready keeps the same rate.
    expect(
      run([
        { state: "ready", graph: 48000, eff: 48000 },
        { state: "switching", graph: 44100, eff: null },
        { state: "ready", graph: 48000, eff: 48000 },
      ]),
    ).toEqual([48000, 48000, 48000]);
  });

  it("follows a real rate change once playing again", () => {
    expect(
      run([
        { state: "playing", graph: 44100, eff: 48000 },
        { state: "switching", graph: 96000, eff: 48000 },
        { state: "playing", graph: 96000, eff: 48000 },
      ]),
    ).toEqual([44100, 44100, 96000]);
  });

  it("previews only offered rates", () => {
    // 48k-only device: no 44.1k even with nothing reported.
    expect(run([{ state: "ready", graph: 48000, eff: null }], [48000])).toEqual([48000]);
    // A high rate is followed once it is allowed.
    expect(run([{ state: "playing", graph: 352800, eff: 48000 }], [...DEFAULT, 352800])).toEqual([352800]);
    expect(run([{ state: "playing", graph: 352800, eff: 48000 }], DEFAULT)).toEqual([48000]);
  });
});

describe("preview rate list", () => {
  it("uses the effective rates, else the configured ones", () => {
    expect(previewRates([48000], DEFAULT)).toEqual([48000]);
    expect(previewRates([], [192000, 44100, 352800])).toEqual([44100, 192000, 352800]);
    expect(previewRates(null, null)).toEqual([44100, 48000]);
  });

  it("drops a chosen rate that is no longer offered", () => {
    expect(chosenPreviewRate(96000, DEFAULT)).toBe(96000);
    expect(chosenPreviewRate(44100, [48000, 96000])).toBeNull();
    expect(chosenPreviewRate(null, DEFAULT)).toBeNull();
  });
});
