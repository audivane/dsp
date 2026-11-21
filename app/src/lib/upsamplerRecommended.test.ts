import { expect, it } from "vitest";
import type { UpsamplerConfig } from "../api/types";
import { isRecommendedUpsampler, recommendedUpsampler } from "./upsamplerRecommended";
import { upsamplingSummary } from "./summary";
import { DEFAULT_CUSTOM_FIELDS, patchUpsamplerConfig, toUpsamplerConfig, type UpsamplerPatch } from "./upsamplerAdapter";

// The shape a manual Automatic + linear-phase design takes.
const LINEAR_AUTO: UpsamplerConfig = toUpsamplerConfig({
  ...DEFAULT_CUSTOM_FIELDS,
  auto_taps: "match_output",
  band: { attenuation_db: 150.0, passband_end: 0.908, stopband_start: 1.0 },
  cutoff_mode: "band",
  phase: "linear",
  preset: null,
  target: { max_rate: 768000 },
  window: "kaiser",
});

it("builds the exact Recommended payload", () => {
  const cur: UpsamplerConfig = { target: { ratio: 4 }, preset: "gaussian_long", design: null };
  expect(recommendedUpsampler(cur)).toEqual({
    target: { max_rate: 768000 },
    preset: "recommended",
    design: null,
  });
  expect(recommendedUpsampler({ ...cur, target: "off" }).target).toBe("off");
  // The input is not changed.
  expect(cur.preset).toBe("gaussian_long");
});

it("matches the Recommended preset on target", () => {
  const rec = recommendedUpsampler(LINEAR_AUTO);
  expect(isRecommendedUpsampler(rec)).toBe(true);
  // Linear phase (the same design otherwise) is not it.
  expect(isRecommendedUpsampler(LINEAR_AUTO)).toBe(false);
  // Anything that changes the filter or the rate is not.
  for (const other of [
    { preset: "gaussian_long" as const },
    // A custom copy of it is a custom design, not the named one.
    { preset: null, design: LINEAR_AUTO.design },
  ] satisfies UpsamplerPatch[]) {
    expect(isRecommendedUpsampler(patchUpsamplerConfig(rec, other))).toBe(false);
  }
  expect(isRecommendedUpsampler(null)).toBe(false);
  // The rate does not matter: off keeps it selected.
  expect(isRecommendedUpsampler(patchUpsamplerConfig(rec, { target: "off" }))).toBe(true);
  expect(isRecommendedUpsampler(patchUpsamplerConfig(rec, { target: { max_rate: 384000 } }))).toBe(true);
});

it("reads Recommended in the sidebar summary", () => {
  const rec = recommendedUpsampler(LINEAR_AUTO);
  expect(upsamplingSummary(rec)).toBe("Recommended · up to 768k");
  expect(upsamplingSummary(rec, { graphRate: 44100, outputRate: 705600, ratio: { up: 16, down: 1 } })).toBe("Recommended · ×16");
  expect(upsamplingSummary(LINEAR_AUTO)).toMatch(/^Custom Kaiser auto · linear/);
});
