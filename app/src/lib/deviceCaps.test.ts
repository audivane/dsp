import { describe, expect, it } from "vitest";
import { MockDaemon } from "../api/mock";
import type { DeviceCapabilities } from "../api/types";
import { directRateNote, directResamples, ditherAdvice, effectiveDitherBits, formatUnavailableReason, rateSupportNote, ratioLadder, sbitsAt, suggestedDitherBits, supportsFormat, usedBits } from "./deviceCaps";

/** A USB speaker that plays S16_LE, 2 channels, 48000 Hz only. */
const SPK48: DeviceCapabilities = {
  device: "hw:CARD=SPK,DEV=0",
  channels: 2,
  channels_min: 2,
  channels_max: 2,
  rates: [{ rate: 48000, formats: [{ format: "s16", sbits: 16 }] }],
  rate_min: 48000,
  rate_max: 48000,
  formats: ["s16"],
};

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

/** USB DAC with 24-bit only up to 96k. */
const TIED: DeviceCapabilities = {
  ...SPK48,
  device: "hw:X",
  rates: [48000, 96000, 192000].map((rate) => ({
    rate,
    formats: [{ format: "s16" as const, sbits: 16 }, ...(rate <= 96000 ? [{ format: "s32" as const, sbits: 24 }] : [])],
  })),
};

describe("device capabilities", () => {
  it("falls back to the unfiltered formats for an unusable device", () => {
    // Mono-only 48k S16 device: the stereo probe found no usable rate.
    const mono: DeviceCapabilities = { ...SPK48, channels_min: 1, channels_max: 1, rates: [] };
    expect(supportsFormat(mono, "s16")).toBe(true);
    expect(supportsFormat(mono, "s32")).toBe(false);
  });

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

  it("greys out formats a 48 kHz S16-only speaker never takes", () => {
    expect(supportsFormat(SPK48, "s16")).toBe(true);
    for (const f of ["s32", "s24", "s24_3"] as const) expect(supportsFormat(SPK48, f)).toBe(false);
    expect(formatUnavailableReason(SPK48, "s32", "s16", 48000)).toBe("S32 not supported by this device");
    expect(sbitsAt(TIED, 96000, "s32")).toBe(24);
  });

  it("explains a format that is not available at the current rate", () => {
    expect(formatUnavailableReason(TIED, "s32", "s16", 192000)).toBe("S32 not available at 192 kHz");
    expect(formatUnavailableReason(TIED, "s32", "s32", 96000)).toBeNull();
    expect(formatUnavailableReason(null, "s32", "s16", 192000)).toBeNull();
  });

  it("caps dither at the format in use", () => {
    expect(effectiveDitherBits(24, "s16")).toBe(16);
    expect(effectiveDitherBits(24, "s24_3")).toBe(24);
    expect(effectiveDitherBits(20, "s32")).toBe(20);
    expect(effectiveDitherBits(24, null)).toBe(24);
  });

  it("describes rates the device cannot play", () => {
    expect(rateSupportNote([44100, 48000], [48000])?.text).toBe("This device supports 48 kHz only; 44.1 kHz streams are resampled by PipeWire.");
    expect(rateSupportNote([44100, 48000, 88200, 96000], [48000])?.text).toBe(
      "This device supports 48 kHz only; 44.1 kHz, 88.2 kHz and other streams are resampled by PipeWire.",
    );
    expect(rateSupportNote([44100, 48000, 96000], [48000, 96000])?.text).toBe("This device cannot play 44.1 kHz; those streams are resampled by PipeWire.");
    expect(rateSupportNote([44100, 48000], [44100, 48000])).toBeNull();
    expect(rateSupportNote([44100, 48000], [])).toBeNull();
  });

  it("walks the ratio ladder like the engine", () => {
    expect(ratioLadder(8)).toEqual([8, 4, 2, 1]);
    expect(ratioLadder(6)).toEqual([6, 5, 4, 2, 1]);
  });
});

describe("mock daemon on a 48 kHz S16-only speaker", () => {
  it("reports 48k only, ratio 1, S16 and 16-bit dither with the default (S32, x4, 24-bit) config", () => {
    const d = new MockDaemon({ device: "hw:CARD=SPK,DEV=0" });
    const st = d.status();
    d.stop();
    expect(d.config.formats[0]).toBe("s32");
    expect(d.config.chain.output.bits).toBe(24);
    // The published rates add the ones Direct converts with the engine
    // bypass resampler; the DSP sink itself still plays 48k only.
    expect(st.effective_rates).toEqual([44100, 48000, 88200, 96000, 176400, 192000]);
    expect(st.effective_default_rate).toBe(48000);
    // The 44.1k stream is resampled into the 48k graph; x4 falls back to x1.
    expect(st.graph_rate).toBe(48000);
    expect(st.ratio).toBe(1);
    expect(st.output_rate).toBe(48000);
    expect(st.format).toBe("s16");
    expect(st.dither_bits).toBe(16);
    expect(st.device_caps?.rates.map((r) => r.rate)).toEqual([48000]);
    expect(st.passthrough.broken_because).toContainEqual({ reason: "resampled", stream_rate: 44100, graph_rate: 48000 });
  });

  it("publishes 48k only when PipeWire converts for Direct", () => {
    const d = new MockDaemon({ device: "hw:CARD=USB48,DEV=0" });
    d.handle("set_rate", { ...d.config.rate, bypass_resampler: "pipewire" });
    const st = d.status();
    d.stop();
    expect(st.effective_rates).toEqual([48000]);
    expect(st.direct_rates).toEqual([48000]);
  });

  it("keeps the default DAC unchanged", () => {
    const d = new MockDaemon();
    const st = d.status();
    d.stop();
    expect(st.format).toBe("s32");
    expect(st.ratio).toBe(4);
    expect(st.dither_bits).toBe(24);
    expect(st.effective_rates).toEqual([44100, 48000, 88200, 96000, 176400, 192000]);
  });
});

describe("bits in use and the suggested dither", () => {
  it("reads the DAC's significant bits at the rate in use", () => {
    expect(usedBits(TIED, 96000, "s32")).toBe(24);
    expect(usedBits(TIED, 192000, "s32")).toBeNull();
    expect(usedBits(null, 96000, "s32")).toBeNull();
  });

  it("suggests what the DAC converts, at most 24 bits", () => {
    expect(suggestedDitherBits("s32", 24)).toBe(24);
    expect(suggestedDitherBits("s32", 32)).toBe(24);
    expect(suggestedDitherBits("s32", null)).toBe(24);
    expect(suggestedDitherBits("s32", 20)).toBe(20);
    expect(suggestedDitherBits("s32", 18)).toBe(16);
    expect(suggestedDitherBits("s16", 16)).toBe(16);
    expect(suggestedDitherBits(null, 24)).toBeNull();
  });
});

describe("dither advice", () => {
  it("says matches only when the target equals the DAC's bits", () => {
    expect(ditherAdvice(24, "s32", 24)).toMatchObject({ text: "Matches this DAC", offer: null });
  });

  it("recommends 24 on a full 32-bit DAC without calling it a match", () => {
    expect(ditherAdvice(24, "s32", 32)).toMatchObject({ text: "Recommended; 32 bits also works", offer: null });
    expect(ditherAdvice(32, "s32", 32)).toMatchObject({ text: "Fine: this DAC uses all 32 bits. 24 is the safer default", offer: 24 });
  });

  it("suggests the DAC's bits otherwise", () => {
    expect(ditherAdvice(32, "s32", 24)).toMatchObject({ text: "Suggested for this DAC: 24 bits", offer: 24 });
    expect(ditherAdvice(16, "s32", 32)).toMatchObject({ text: "Suggested for this DAC: 24 bits", offer: 24 });
    expect(ditherAdvice(24, "s16", 16)).toMatchObject({ text: "Suggested for this DAC: 16 bits", offer: 16 });
    expect(ditherAdvice(24, "s32", null)).toMatchObject({ text: "Recommended for this DAC", offer: null });
    expect(ditherAdvice(24, null, null)).toBeNull();
  });
});
