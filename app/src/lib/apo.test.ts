import { describe, expect, it } from "vitest";
import { ApoError, parseApo, presetNameFromFile, toApo } from "./apo";

const AUTOEQ = `# AutoEQ-style profile for a headphone
Preamp: -6.2 dB
Filter 1: ON PK Fc 105 Hz Gain 3.4 dB Q 0.70
Filter 2: ON PK Fc 3200 Hz Gain -2.1 dB Q 1.41
Filter 3: OFF PK Fc 1000 Hz Gain 5.0 dB Q 0.50
Filter 4: ON LSC Fc 105 Hz Gain 5.5 dB Q 0.71
Filter 5: ON HSC Fc 10000 Hz Gain -4.0 dB Q 0.71
Filter 6: ON PK Fc 200 Hz Gain -1.2 dB Q 0.9
Filter 7: ON PK Fc 4000 Hz Gain 2.2 dB Q 2.0
Device: Speakers
Channel: all
`;

describe("ParametricEQ.txt import", () => {
  it("parses an AutoEQ file like dsp-core", () => {
    const { profile, warnings } = parseApo(AUTOEQ, "HD650");
    expect(profile.name).toBe("HD650");
    expect(profile.preamp_db).toBeCloseTo(-6.2, 12);
    expect(profile.bands).toHaveLength(7);
    expect(profile.bands[0]).toEqual({ kind: "peaking", freq_hz: 105, gain_db: 3.4, q: 0.7, enabled: true });
    expect(profile.bands[2].enabled).toBe(false);
    expect(profile.bands[3].kind).toBe("low_shelf");
    expect(profile.bands[4].kind).toBe("high_shelf");
    expect(profile.right).toBeUndefined();
    expect(warnings).toHaveLength(1); // Device:
  });

  it("converts BW and S like dsp-core and defaults Q", () => {
    const { profile } = parseApo("Filter: ON PK Fc 1000 Hz Gain 3 dB BW Oct 1\nFilter: ON LS Fc 100 Hz Gain 6 dB S 1\nFilter: ON LP Fc 18000 Hz");
    const w = 2;
    expect(profile.bands[0].q).toBeCloseTo(Math.sqrt(w) / (w - 1), 12);
    expect(profile.bands[1].q).toBeCloseTo(Math.SQRT1_2, 12);
    expect(profile.bands[2]).toMatchObject({ kind: "low_pass", gain_db: 0, q: Math.SQRT1_2 });
  });

  it("puts Channel L and R filters in their own sections", () => {
    const { profile, warnings } = parseApo("Preamp: -3 dB\nChannel: L\nFilter: ON PK Fc 100 Hz Gain 2 dB Q 1\nChannel: R\nFilter: ON PK Fc 200 Hz Gain -2 dB Q 1\nChannel: all\nFilter: ON HS Fc 8000 Hz Gain 1 dB Q 0.7");
    expect(profile.version).toBe(2);
    expect(profile.linked_enabled).toBe(true);
    expect(profile.preamp_db).toBe(-3);
    expect(profile.bands.map((b) => b.freq_hz)).toEqual([8000]);
    expect(profile.left?.bands.map((b) => b.freq_hz)).toEqual([100]);
    expect(profile.right?.bands.map((b) => b.freq_hz)).toEqual([200]);
    expect(warnings).toHaveLength(0);
  });

  it("rejects malformed filters with the line number", () => {
    expect(() => parseApo("Preamp: -1 dB\nFilter 1: ON XX Fc 100 Hz")).toThrow(ApoError);
    expect(() => parseApo("Filter 1: ON PK Gain 3 dB")).toThrow(/Line 1/);
    expect(() => parseApo("Preamp -3 dB")).toThrow(/Line 1/);
  });

  it("skips None filters and unknown directives", () => {
    const { profile, warnings } = parseApo("Include: other.txt\nFilter 1: ON None\nFilter 2: ON NO Fc 50 Hz Q 30");
    expect(profile.bands).toEqual([{ kind: "notch", freq_hz: 50, gain_db: 0, q: 30, enabled: true }]);
    expect(warnings).toHaveLength(2);
  });
});

describe("ParametricEQ.txt export", () => {
  it("round-trips the linked section", () => {
    const linked = parseApo(AUTOEQ).profile;
    expect(parseApo(toApo(linked)).profile).toEqual(linked);
  });

  it("writes Equalizer APO syntax", () => {
    expect(
      toApo({
        name: "x",
        version: 2,
        preamp_db: -6,
        linked_enabled: true,
        bands: [{ kind: "low_shelf", freq_hz: 105, gain_db: 5.5, q: 0.7, enabled: true }],
      }),
    ).toBe("Preamp: -6 dB\nFilter 1: ON LSC Fc 105 Hz Gain 5.5 dB Q 0.7\n");
  });

  it("names presets from file names", () => {
    expect(presetNameFromFile("Sennheiser HD 650 ParametricEQ.txt")).toBe("Sennheiser HD 650");
    expect(presetNameFromFile("ParametricEQ.txt")).toBe("Imported");
  });
});
