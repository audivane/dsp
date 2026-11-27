import { describe, expect, it } from "vitest";
import type { DeviceCapabilities } from "../api/types";
import { directRateNote, directResamples, effectiveDitherBits } from "./deviceCaps";

/** A USB DAC that plays 48 kHz only (the mock's `USB48`). */
const USB48: DeviceCapabilities = {
  device: "hw:CARD=USB48,DEV=0",
  channels: 2,
  channels_min: 2,
  channels_max: 2,
  rates: [{ rate: 48000, formats: [{ format: "s16", sbits: 16 }, { format: "s24", sbits: 24 }, { format: "s32", sbits: 24 }] }],
  rate_min: 48000,
  rate_max: 48000,
  formats: ["s16", "s24", "s32"],
};

describe("device capabilities", () => {

  it("says where Audivane Direct is bit-perfect", () => {
    // 96k-only DAC, DSP x2: the graph may run at 48k, Direct only at 96k.
    expect(directRateNote([48000, 96000], [96000])).toBe(
      "Audivane Direct is bit-perfect at 96 kHz only on this device; a Direct stream at another rate is resampled by PipeWire.",
    );
    expect(directRateNote([48000], [])).toMatch(/cannot play on this device/);
    // The engine bypass resampler converts 44.1 kHz on a 48 kHz-only DAC.
    expect(directRateNote([44100, 48000], [44100, 48000], USB48)).toBe(
      "Audivane Direct is bit-perfect at 48 kHz on this device; at 44.1 kHz the engine converts to a rate the DAC plays (not bit-perfect).",
    );
    expect(directRateNote([44100], [44100], USB48)).toMatch(/^Audivane Direct is not bit-perfect on this device: at 44.1 kHz/);
    expect(directRateNote([44100, 48000], [44100, 48000])).toBeNull();
    expect(directRateNote([], [])).toBeNull();
    expect(directResamples(48000, [96000])).toBe(true);
    expect(directResamples(96000, [96000])).toBe(false);
    expect(directResamples(48000, [])).toBe(false);
  });

  it("caps dither at the format in use", () => {
    expect(effectiveDitherBits(24, "s16")).toBe(16);
    expect(effectiveDitherBits(24, "s24_3")).toBe(24);
    expect(effectiveDitherBits(20, "s32")).toBe(20);
    expect(effectiveDitherBits(24, null)).toBe(24);
  });

});

