import { expect, it } from "vitest";
import type { UpsamplerConfig } from "../api/types";
import { recommendedUpsampler } from "./upsamplerRecommended";

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

