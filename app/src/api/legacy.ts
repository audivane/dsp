// Older engines' reply shapes, read into the current ones where the data
// enters the app (the store), so views never see a missing field.
//
// Before fractional rates (0.9.4 and earlier) `Status` and
// `UpsamplerResponse` had only the integer `ratio`; `rate_ratio` is then
// `ratio / 1`.

import type { RateRatio, Status, UpsamplerResponse } from "./types";

function legacyRatio(ratio: number | undefined): RateRatio {
  return { up: Math.max(1, Math.round(ratio ?? 1)), down: 1 };
}

/** Whether this status comes from an engine that runs fractional rates. */
export function hasRateRatio(s: Partial<Status>): boolean {
  return !!s.rate_ratio && typeof s.rate_ratio.up === "number" && typeof s.rate_ratio.down === "number";
}

export function normalizeStatus(s: Status): Status {
  return hasRateRatio(s) ? s : { ...s, rate_ratio: legacyRatio(s.ratio) };
}

export function normalizeUpsamplerResponse(r: UpsamplerResponse): UpsamplerResponse {
  const ok = !!r.rate_ratio && typeof r.rate_ratio.up === "number" && typeof r.rate_ratio.down === "number";
  return ok ? r : { ...r, rate_ratio: legacyRatio(r.ratio) };
}
