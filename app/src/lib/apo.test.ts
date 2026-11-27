import { describe, expect, it } from "vitest";
import { parseApo, toApo } from "./apo";

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

});

describe("ParametricEQ.txt export", () => {

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

});
