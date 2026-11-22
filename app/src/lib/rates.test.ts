import { describe, expect, it } from "vitest";
import { ENGINE_RATES, convertedRates, nearestRate, normalizeRatePolicy, previewRates, rateChips, toggleRate, withDefaultRate } from "./rates";

const DEFAULT = { allowed_rates: [44100, 48000, 88200, 96000, 176400, 192000], default_rate: 48000 };

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
  const base = { policy: DEFAULT, playable: ENGINE_RATES, state: "playing" as const, graphRate: 44100 };

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

  it("marks what the engine says a 48k-only device does not play; every chip stays editable", () => {
    // Same family on a 48 kHz-only DAC: the engine plays 48 kHz only.
    const chips = rateChips({ ...base, playable: [48000], graphRate: 48000 });
    const at = (r: number) => chips.find((c) => c.rate === r)!;
    expect(at(48000)).toMatchObject({ unsupported: false, canToggle: true });
    expect(at(96000)).toMatchObject({ enabled: true, unsupported: true, canToggle: true });
    expect(at(96000).title).toContain("PipeWire converts it");
    expect(at(96000).title).toContain("Click to stop using");
    expect(at(352800)).toMatchObject({ enabled: false, unsupported: true, canToggle: true });
    expect(at(352800).title).toContain("Click to allow");
  });

  it("follows the engine's list, not integer multiples", () => {
    // Best for DAC on the same DAC: decimation and 160/147 reach every rate.
    const chips = rateChips({ ...base, playable: ENGINE_RATES, graphRate: 48000 });
    expect(chips.some((c) => c.unsupported)).toBe(false);
  });

  it("the last allowed rate cannot be turned off", () => {
    const chips = rateChips({ ...base, policy: { allowed_rates: [48000], default_rate: 48000 } });
    const c = chips.find((x) => x.rate === 48000)!;
    expect(c.canToggle).toBe(false);
    expect(c.title).toContain("only allowed rate");
  });

  it("an unknown device does not mark anything", () => {
    expect(rateChips({ ...base, playable: [] }).some((c) => c.unsupported)).toBe(false);
    expect(rateChips({ ...base, playable: undefined }).some((c) => c.unsupported)).toBe(false);
  });
});

describe("previewRates", () => {
  it("lists the allowed rates and the effective ones", () => {
    expect(previewRates([48000, 352800, 44100], DEFAULT.allowed_rates)).toEqual([44100, 48000, 88200, 96000, 176400, 192000, 352800]);
    expect(previewRates([48000], [44100, 48000])).toEqual([44100, 48000]);
    expect(previewRates([], [])).toEqual([44100, 48000]);
  });
});

describe("convertedRates", () => {
  it("lists the rates PipeWire converts first", () => {
    const one = { up: 1, down: 1 };
    const landings = [
      { source_rate: 44100, ratio: one, requested: { up: 16, down: 1 }, converted_to: 48000 },
      { source_rate: 48000, ratio: one, requested: { up: 16, down: 1 } },
    ];
    expect(convertedRates(landings)).toEqual(new Map([[44100, 48000]]));
    expect(convertedRates(undefined).size).toBe(0);
  });
});
