import { describe, expect, it } from "vitest";
import type { DeviceCapabilities } from "../api/types";
import { ENGINE_RATES, devicePlays, nearestRate, normalizeRatePolicy, previewRates, rateChips, toggleRate, withDefaultRate } from "./rates";

const DEFAULT = { allowed_rates: [44100, 48000, 88200, 96000, 176400, 192000], default_rate: 48000 };

function caps(rates: number[]): DeviceCapabilities {
  return {
    device: "hw:X",
    channels: 2,
    channels_min: 2,
    channels_max: 2,
    rates: rates.map((rate) => ({ rate, formats: [{ format: "s32", sbits: 24 }] })),
    rate_min: rates[0],
    rate_max: rates[rates.length - 1],
    formats: ["s32"],
  } as DeviceCapabilities;
}

describe("nearestRate (io-pw nearest_rate)", () => {
  it("prefers the same family, then the closest, then 48k", () => {
    expect(nearestRate(48000, [44100, 96000, 192000])).toBe(96000);
    expect(nearestRate(44100, [48000, 88200])).toBe(88200);
    expect(nearestRate(48000, [44100, 88200])).toBe(44100);
    expect(nearestRate(705600, [352800, 768000])).toBe(352800);
    expect(nearestRate(46050, [44100, 48000])).toBe(48000);
    expect(nearestRate(48000, [])).toBeNull();
  });
});

describe("toggleRate", () => {
  it("allows a rate and keeps the list sorted", () => {
    expect(toggleRate(DEFAULT, 352800)!.allowed_rates).toEqual([44100, 48000, 88200, 96000, 176400, 192000, 352800]);
  });

  it("moves a blocked default to the nearest allowed rate, same family first", () => {
    const p = toggleRate(DEFAULT, 48000)!;
    expect(p.allowed_rates).not.toContain(48000);
    expect(p.default_rate).toBe(96000);
    const only44 = toggleRate({ allowed_rates: [44100, 48000], default_rate: 48000 }, 48000)!;
    expect(only44).toEqual({ allowed_rates: [44100], default_rate: 44100 });
  });

  it("never blocks the last rate", () => {
    expect(toggleRate({ allowed_rates: [48000], default_rate: 48000 }, 48000)).toBeNull();
  });

  it("keeps other fields", () => {
    const p = toggleRate({ ...DEFAULT, conflict: "highest" as const, follow_source: false }, 192000)!;
    expect(p.conflict).toBe("highest");
    expect(p.follow_source).toBe(false);
  });
});

describe("withDefaultRate / normalizeRatePolicy", () => {
  it("sets the default and allows it", () => {
    expect(withDefaultRate(DEFAULT, 96000).default_rate).toBe(96000);
    expect(withDefaultRate(DEFAULT, 384000)).toEqual({ allowed_rates: [...DEFAULT.allowed_rates, 384000], default_rate: 384000 });
  });

  it("dedups, sorts and repairs a default that is not allowed", () => {
    expect(normalizeRatePolicy({ allowed_rates: [96000, 44100, 96000], default_rate: 48000 })).toEqual({ allowed_rates: [44100, 96000], default_rate: 96000 });
  });
});

describe("rateChips", () => {
  const base = { policy: DEFAULT, effective: DEFAULT.allowed_rates, caps: caps(ENGINE_RATES as number[]), maxRatio: 4, state: "playing" as const, graphRate: 44100 };

  it("shows all engine rates with the high ones off by default", () => {
    const chips = rateChips(base);
    expect(chips.map((c) => c.rate)).toEqual(ENGINE_RATES);
    expect(chips.filter((c) => !c.enabled).map((c) => c.rate)).toEqual([352800, 384000, 705600, 768000]);
    expect(chips.find((c) => c.isDefault)?.rate).toBe(48000);
    expect(chips.find((c) => c.playing)?.rate).toBe(44100);
    expect(chips.every((c) => c.canToggle && !c.unsupported)).toBe(true);
    expect(chips.find((c) => c.rate === 352800)!.title).toContain("DXD");
  });

  it("no playing highlight while idle", () => {
    expect(rateChips({ ...base, state: "ready" }).some((c) => c.playing)).toBe(false);
  });

  it("greys rates a 48k-only device cannot play; enabled ones can still be turned off", () => {
    const chips = rateChips({ ...base, effective: [48000], caps: caps([48000]), maxRatio: 1, graphRate: 48000 });
    const at = (r: number) => chips.find((c) => c.rate === r)!;
    expect(at(48000)).toMatchObject({ unsupported: false, canToggle: true });
    expect(at(44100)).toMatchObject({ enabled: true, unsupported: true, canToggle: true });
    expect(at(44100).title).toContain("Not supported by this device");
    expect(at(352800)).toMatchObject({ enabled: false, unsupported: true, canToggle: false });
  });

  it("the last allowed rate cannot be turned off", () => {
    const chips = rateChips({ ...base, policy: { allowed_rates: [48000], default_rate: 48000 } });
    const c = chips.find((x) => x.rate === 48000)!;
    expect(c.canToggle).toBe(false);
    expect(c.title).toContain("only allowed rate");
  });

  it("an unknown device does not grey anything", () => {
    expect(rateChips({ ...base, effective: [], caps: null }).some((c) => c.unsupported)).toBe(false);
  });
});

describe("devicePlays", () => {
  it("counts upsampled rates when upsampling can reach them", () => {
    const c = caps([96000, 192000]);
    expect(devicePlays(c, 48000, 4)).toBe(true);
    expect(devicePlays(c, 48000, 1)).toBe(false);
    expect(devicePlays(c, 44100, 4)).toBe(false);
  });
});

describe("previewRates", () => {
  it("follows the effective list", () => {
    expect(previewRates([48000, 352800, 44100], DEFAULT.allowed_rates)).toEqual([44100, 48000, 352800]);
  });
});
