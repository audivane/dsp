import { expect, it } from "vitest";
import type { UpsamplerConfig } from "../api/types";
import { isResampling, ratioCapNote, rateRatioText, rateRatioValue, upsamplingLatencyMs, upsamplingSummary } from "./summary";
import { DEFAULT_CUSTOM_FIELDS, patchUpsamplerConfig, toUpsamplerConfig } from "./upsamplerAdapter";

it("formats running ratios", () => {
  expect(rateRatioText({ up: 4, down: 1 })).toBe("×4");
  expect(rateRatioText({ up: 160, down: 147 })).toBe("160/147");
  expect(rateRatioValue({ up: 1, down: 2 })).toBe(0.5);
  expect(isResampling({ up: 1, down: 1 })).toBe(false);
  expect(isResampling({ up: 1, down: 2 })).toBe(true);
});

it("does not report a fractional ratio as upsampling off", () => {
  // Target {rate: 48000} from 44.1 kHz, running 160/147: nothing is capped.
  expect(ratioCapNote(44100, 48000 / 44100, { up: 160, down: 147 })).toBeNull();
  // A device that fell back to x1.
  expect(ratioCapNote(44100, 48000 / 44100, { up: 1, down: 1 })).toContain("nothing is upsampled");
  // max_rate_any fell back to a fractional ratio below the wanted x16.
  const note = ratioCapNote(44100, 16, { up: 640, down: 147 });
  expect(note).toContain("640/147");
  expect(note).toContain("192");
});

const MANUAL: UpsamplerConfig = toUpsamplerConfig({
  ...DEFAULT_CUSTOM_FIELDS,
  target: { ratio: 4 },
  preset: null,
});

it("summarises a preset and a custom design for the sidebar", () => {
  expect(upsamplingSummary(patchUpsamplerConfig(MANUAL, { target: { ratio: 8 }, preset: "flattop_xlong" }))).toBe("Flat-top xlong · linear · ×8");
  expect(upsamplingSummary(patchUpsamplerConfig(MANUAL, { phase: "minimum", target: { rate: 48000 } }), { graphRate: 44100, outputRate: 48000, ratio: { up: 160, down: 147 } })).toBe(
    "Custom Kaiser 64k · minimum · 44.1→48k",
  );
  expect(upsamplingSummary(patchUpsamplerConfig(MANUAL, { auto_taps: "match_output" }))).toBe("Custom Kaiser auto · linear · ×4");
  expect(upsamplingSummary(patchUpsamplerConfig(MANUAL, { target: { max_rate_any: 768000 } }))).toBe("Custom Kaiser 64k · linear · best up to 768k");
  expect(upsamplingSummary({ ...MANUAL, design: { kind: "lanczos", lobes: 23, decimation: { length: "keep" }, phase: { type: "linear" }, preserve_samples: false, rates: null, numerics: { path: "fir", grid_cutoff: "scaled" } } })).toBe("Custom Lanczos 23 · linear · ×4");
  expect(upsamplingSummary({ ...MANUAL, design: { kind: "cascade", stage: "fir_minimum" } })).toBe("Custom Cascade FIR minimum · ×4");
  expect(upsamplingSummary(patchUpsamplerConfig(MANUAL, { target: "off", preset: "gaussian_long" }))).toBe("Off");
  // The engine fell back to x2: the summary says what runs.
  expect(upsamplingSummary(patchUpsamplerConfig(MANUAL, { target: { ratio: 8 } }), { graphRate: 96000, outputRate: 192000, ratio: { up: 2, down: 1 } })).toBe("Custom Kaiser 64k · linear · ×2");
});

it("takes the latency the engine measured, else the plan's, and has none for presets or automatic lengths", () => {
  const measured = { source_rate: 44100, output_rate: 176400, latency_samples: 17640 };
  expect(upsamplingLatencyMs(patchUpsamplerConfig(MANUAL, { preset: "gaussian_long" }), 44100, measured)).toBeCloseTo(100, 6);
  expect(upsamplingLatencyMs(patchUpsamplerConfig(MANUAL, { preset: "gaussian_long" }), 44100)).toBeNull();
  expect(upsamplingLatencyMs(patchUpsamplerConfig(MANUAL, { auto_taps: { db: 140 } }), 44100)).toBeNull();
  // A measurement at another source rate is not used.
  expect(upsamplingLatencyMs(patchUpsamplerConfig(MANUAL, { preset: "gaussian_long" }), 48000, measured)).toBeNull();
  // No measurement: half the planned kernel of a linear design.
  const plan = { grid_up: 4, grid_down: 1, grid_taps: 65536, grid_taps_exact: true, workspace_bytes: 0, resident_bytes: 0 };
  expect(upsamplingLatencyMs(MANUAL, 44100)).toBeNull();
  expect(upsamplingLatencyMs(MANUAL, 44100, null, plan)).toBeCloseTo((65535 / 2 / 176400) * 1000, 6);
  expect(upsamplingLatencyMs(MANUAL, 44100, null, { ...plan, grid_taps_exact: false })).toBeNull();
  expect(upsamplingLatencyMs(patchUpsamplerConfig(MANUAL, { phase: "minimum" }), 44100, null, plan)).toBeNull();
  expect(upsamplingLatencyMs(patchUpsamplerConfig(MANUAL, { target: "off" }), 44100)).toBe(0);
});
