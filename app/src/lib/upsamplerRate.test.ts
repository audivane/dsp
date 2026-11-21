import { describe, expect, it } from "vitest";
import type { DeviceCapabilities, EngineState, SampleFormat } from "../api/types";
import {
  chosenPreviewRate,
  dacHighRateBlock,
  dacRateBlock,
  dacRates,
  deviceResampleNote,
  highRatesFor,
  isTermLimitMessage,
  isUnmeasuredRatio,
  landsLowerNote,
  targetDacBlock,
  unreachableNote,
  unreachableSources,
  landingText,
  rateKind,
  rateRatioOf,
  resolveRateRatio,
  targetForKind,
  viewSourceRate,
} from "./upsamplerRate";
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

describe("rate targets", () => {
  it("resolves a target to a reduced ratio like the protocol", () => {
    expect(resolveRateRatio({ rate: 48000 }, 44100)).toEqual({ up: 160, down: 147 });
    expect(resolveRateRatio({ rate: 48000 }, 96000)).toEqual({ up: 1, down: 2 });
    expect(resolveRateRatio({ rate: 48000 }, 48000)).toEqual({ up: 1, down: 1 });
    expect(resolveRateRatio({ max_rate_any: 768000 }, 44100)).toEqual({ up: 16, down: 1 });
    expect(resolveRateRatio({ max_rate: 384000 }, 96000)).toEqual({ up: 4, down: 1 });
    expect(resolveRateRatio({ ratio: 3 }, 44100)).toEqual({ up: 3, down: 1 });
    expect(resolveRateRatio("off", 44100)).toEqual({ up: 1, down: 1 });
    expect(rateRatioOf(44100, 192000)).toEqual({ up: 640, down: 147 });
  });

  it("returns to the last target of a kind, else a default", () => {
    expect(rateKind({ max_rate_any: 768000 })).toBe("max");
    expect(rateKind({ rate: 48000 })).toBe("rate");
    expect(targetForKind("ratio", {}, null)).toEqual({ ratio: 4 });
    expect(targetForKind("ratio", { ratio: { ratio: 8 } }, null)).toEqual({ ratio: 8 });
    expect(targetForKind("max", {}, null)).toEqual({ max_rate: 768000 });
    expect(targetForKind("rate", {}, 48000)).toEqual({ rate: 48000 });
    expect(targetForKind("rate", {}, 176400 * 3)).toEqual({ rate: 96000 });
    expect(targetForKind("off", { ratio: { ratio: 8 } }, null)).toBe("off");
  });

  it("says when the DAC is why the rate is converted", () => {
    const base = { graphRate: 44100, outputRate: 48000, ratio: { up: 160, down: 147 }, target: { rate: 48000 } as const };
    expect(deviceResampleNote({ ...base, deviceRates: [48000] })).toContain("cannot play 44.1 kHz");
    expect(deviceResampleNote({ ...base, deviceRates: [44100, 48000] })).toBeNull();
    expect(
      deviceResampleNote({ graphRate: 44100, outputRate: 192000, ratio: { up: 640, down: 147 }, target: { max_rate_any: 768000 }, deviceRates: [44100, 48000, 96000, 192000] }),
    ).toContain("other rate family");
    expect(deviceResampleNote({ ...base, ratio: { up: 1, down: 1 }, deviceRates: [48000] })).toBeNull();
    expect(landingText(44100, 48000, { up: 160, down: 147 })).toBe("44.1 kHz → 48 kHz (160/147)");
  });
});

describe("remembered targets on an engine without fractional rates", () => {
  it("restores the integer form", () => {
    const last = { max: { max_rate_any: 384000 }, rate: { rate: 48000 } };
    expect(targetForKind("max", last, null, false)).toEqual({ max_rate: 384000 });
    expect(targetForKind("max", last, null)).toEqual({ max_rate_any: 384000 });
    expect(targetForKind("ratio", { ratio: { ratio: 8 } }, null, false)).toEqual({ ratio: 8 });
  });
});

describe("high output rates (x32, x64)", () => {
  const upTo768 = [44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000];
  it("maps caps and multiples per family", () => {
    expect(resolveRateRatio({ max_rate: 3072000 }, 44100)).toEqual({ up: 64, down: 1 });
    expect(resolveRateRatio({ max_rate: 1536000 }, 48000)).toEqual({ up: 32, down: 1 });
    expect(highRatesFor({ max_rate: 1536000 })).toEqual([1411200, 1536000]);
    expect(highRatesFor({ max_rate_any: 3072000 })).toEqual([2822400, 3072000]);
    expect(highRatesFor({ ratio: 64 })).toEqual([2822400, 3072000]);
    expect(highRatesFor({ ratio: 32 })).toEqual([1411200, 1536000]);
    expect(highRatesFor({ max_rate: 768000 })).toEqual([]);
    expect(highRatesFor({ ratio: 16 })).toEqual([]);
  });
  // A DAC probe: S16/S24/S32 up to 768 kHz, then `high` in S32 only.
  const caps = (high: number[]) => ({
    device: "hw:CARD=USB,DEV=0",
    channels: 2,
    channels_min: 2,
    channels_max: 2,
    rate_min: 44100,
    rate_max: Math.max(768000, ...high),
    formats: ["s16", "s24", "s32"] as SampleFormat[],
    rates: [
      ...upTo768.map((rate) => ({ rate, formats: (["s16", "s24", "s32"] as SampleFormat[]).map((format) => ({ format, sbits: null })) })),
      ...high.map((rate) => ({ rate, formats: [{ format: "s32" as SampleFormat, sbits: 32 }] })),
    ],
  });
  const all = [1411200, 1536000, 2822400, 3072000];
  it("greys out what the DAC does not report", () => {
    const low = dacRates(caps([]), []);
    expect(dacHighRateBlock({ max_rate: 3072000 }, low)).toBe("Your DAC does not report 2822.4 kHz or 3072 kHz");
    expect(dacHighRateBlock({ ratio: 32 }, low)).toBe("Your DAC does not report 1411.2 kHz or 1536 kHz");
    // One family is enough: it helps sources of that family.
    expect(dacHighRateBlock({ ratio: 32 }, dacRates(caps([1536000]), []))).toBeNull();
    expect(dacHighRateBlock({ max_rate: 3072000 }, dacRates(caps(all), []))).toBeNull();
    // Not probed, or within 768 kHz: never greyed for this reason.
    expect(dacHighRateBlock({ max_rate: 3072000 }, null)).toBeNull();
    expect(dacRates(null, [])).toBeNull();
    expect(dacHighRateBlock({ ratio: 16 }, low)).toBeNull();
  });
  it("follows the configured formats like the engine's open plan", () => {
    // S32 only above 768 kHz: an S16-only config never opens there.
    const s16 = dacRates(caps(all), ["s16"]);
    expect(s16?.playable.includes(3072000)).toBe(false);
    expect(s16?.playable.includes(768000)).toBe(true);
    expect(dacHighRateBlock({ max_rate: 3072000 }, s16)).toBe("Your DAC does not take 2822.4 kHz or 3072 kHz in S16");
    expect(dacRateBlock([1411200], dacRates(caps(all), ["s16", "s24"]))).toBe("Your DAC does not take 1411.2 kHz in S16 or S24");
    // S32 among them, or the driver's preference (empty): it opens.
    expect(dacHighRateBlock({ max_rate: 3072000 }, dacRates(caps(all), ["s16", "s32"]))).toBeNull();
    expect(dacHighRateBlock({ max_rate: 3072000 }, dacRates(caps(all), []))).toBeNull();
  });
  it("tells a ratio limit from a broken design", () => {
    expect(isUnmeasuredRatio("this design resolves to an unsupported ratio 3/1 at 44100 Hz")).toBe(true);
    expect(isUnmeasuredRatio("not measured above x64")).toBe(true);
    expect(isUnmeasuredRatio("upsampler.target ratio must be between 1 and 64 (got 65)")).toBe(true);
    expect(isUnmeasuredRatio("upsampler.taps must be between 1 and 4194304 (got 0)")).toBe(false);
  });
});

describe("fixed rates past the rational term limit", () => {
  const both = [44100, 48000, 88200, 96000];
  it("names the allowed sources a fixed rate cannot be reached from", () => {
    expect(unreachableSources({ rate: 3072000 }, both)).toEqual([
      { rate: 44100, ratio: { up: 10240, down: 147 } },
      { rate: 88200, ratio: { up: 5120, down: 147 } },
    ]);
    expect(unreachableSources({ rate: 1536000 }, both).map((x) => x.rate)).toEqual([44100]);
    // Same-family integer ratios, and the 48 kHz family to 2822.4 kHz (294/5), work.
    expect(unreachableSources({ rate: 2822400 }, both)).toEqual([]);
    expect(unreachableSources({ rate: 1411200 }, both)).toEqual([]);
    expect(unreachableSources({ rate: 3072000 }, [48000, 96000])).toEqual([]);
    expect(unreachableSources({ ratio: 64 }, both)).toEqual([]);
  });
  it("says so in plain words, with the same-family rate that works", () => {
    expect(unreachableNote({ rate: 3072000 }, both)).toBe(
      "3072 kHz cannot be reached from 44.1 kHz or 88.2 kHz: that takes a fractional ratio (10240/147), and the engine caps its terms at 4096. Use 2822.4 kHz, the same family, or turn 44.1 kHz and 88.2 kHz off on the Device page.",
    );
    expect(unreachableNote({ rate: 1536000 }, [44100, 48000])).toBe(
      "1536 kHz cannot be reached from 44.1 kHz: that takes a fractional ratio (5120/147), and the engine caps its terms at 4096. Use 1411.2 kHz, the same family, or turn 44.1 kHz off on the Device page.",
    );
    expect(unreachableNote({ rate: 2822400 }, both)).toBeNull();
  });
  it("recognises the engine's refusal", () => {
    expect(isTermLimitMessage("chain.upsampler.target: at 44100 Hz the ratio 10240/147 has a term above 4096")).toBe(true);
    expect(isTermLimitMessage("chain.upsampler.taps must be between 1 and 4194304 (got 0)")).toBe(false);
  });
});

describe("a selected target the DAC cannot take", () => {
  const dac = (formats: SampleFormat[]) =>
    dacRates(
      {
        rates: [
          ...[44100, 48000, 88200, 96000, 705600, 768000].map((rate) => ({ rate, formats: (["s16", "s32"] as SampleFormat[]).map((format) => ({ format, channels: [2] })) })),
          ...[2822400, 3072000].map((rate) => ({ rate, formats: [{ format: "s32" as SampleFormat, channels: [2] }] })),
        ],
        formats: ["s16", "s32"],
      } as unknown as DeviceCapabilities,
      formats,
    );
  it("names the reason for the target itself", () => {
    expect(targetDacBlock({ max_rate: 3072000 }, dac(["s16"]))).toBe("Your DAC does not take 2822.4 kHz or 3072 kHz in S16");
    expect(targetDacBlock({ ratio: 64 }, dac(["s16"]))).toBe("Your DAC does not take 2822.4 kHz or 3072 kHz in S16");
    expect(targetDacBlock({ rate: 3072000 }, dac(["s16"]))).toBe("Your DAC does not take 3072 kHz in S16");
    expect(targetDacBlock({ rate: 352800 }, dac([]))).toBe("Your DAC does not report 352.8 kHz");
    expect(targetDacBlock({ max_rate: 3072000 }, dac(["s32"]))).toBeNull();
    expect(targetDacBlock({ ratio: 16 }, dac(["s16"]))).toBeNull();
    expect(targetDacBlock("off", dac(["s16"]))).toBeNull();
    expect(targetDacBlock({ rate: 3072000 }, null)).toBeNull();
  });
  it("says the engine lands lower", () => {
    expect(landsLowerNote({ max_rate: 3072000 }, dac(["s16"]), 705600)).toBe("Your DAC does not take 2822.4 kHz or 3072 kHz in S16, so the engine lands lower: 705.6 kHz now.");
    expect(landsLowerNote({ ratio: 64 }, dac(["s16"]), null)).toBe("Your DAC does not take 2822.4 kHz or 3072 kHz in S16, so the engine lands lower.");
    expect(landsLowerNote({ ratio: 64 }, dac(["s32"]), 3072000)).toBeNull();
  });
});
