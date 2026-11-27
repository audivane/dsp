import { describe, expect, it } from "vitest";
import type { SampleFormat } from "../api/types";
import { dacHighRateBlock, dacRateBlock, dacRates, rateRatioOf, resolveRateRatio } from "./upsamplerRate";

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

});

describe("high output rates (x32, x64)", () => {
  const upTo768 = [44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000];
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
});

