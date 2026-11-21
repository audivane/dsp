import { describe, expect, it } from "vitest";
import { keyPatch, osdPayload, osdText, volumeOnDac, volumeRange } from "./volumeKeys";

const L = (volume_db: number, muted = false) => ({ volume_db, balance_db: 0, muted });
const VS = { min_db: -80, max_db: 0 };

describe("volume keys", () => {
  it("steps by the setting and stays on the 0.1 dB grid", () => {
    expect(keyPatch(L(-20), VS, "up", 1)).toEqual({ volume_db: -19 });
    expect(keyPatch(L(-20), VS, "down", 0.5)).toEqual({ volume_db: -20.5 });
    expect(keyPatch(L(-20.33), VS, "up", 1.5)).toEqual({ volume_db: -18.8 });
  });

  it("stops at the ceiling and at the bottom, and writes nothing there", () => {
    const capped = { min_db: -80, max_db: -6 };
    expect(keyPatch(L(-6.5), capped, "up", 1)).toEqual({ volume_db: -6 });
    expect(keyPatch(L(-6), capped, "up", 1)).toBeNull();
    expect(keyPatch(L(-80), VS, "down", 1)).toBeNull();
    expect(keyPatch(L(-79.6), VS, "down", 1)).toEqual({ volume_db: -80 });
  });

  it("never steps against the key from a level outside the range", () => {
    // An engine that reports a narrower range than the level: up does not pull it down.
    expect(keyPatch(L(-3), { min_db: -80, max_db: -6 }, "up", 1)).toBeNull();
    expect(keyPatch(L(-3), { min_db: -80, max_db: -6 }, "down", 1)).toEqual({ volume_db: -6 });
  });

  it("mute toggles only the mute; steps leave it alone", () => {
    expect(keyPatch(L(-20, false), VS, "mute", 1)).toEqual({ muted: true });
    expect(keyPatch(L(-20, true), VS, "mute", 1)).toEqual({ muted: false });
    expect(keyPatch(L(-20, true), VS, "up", 1)).toEqual({ volume_db: -19 });
  });

  it("range is the engine's within the protocol bounds", () => {
    expect(volumeRange({ min_db: -200, max_db: 5 })).toEqual({ lo: -120, hi: 0 });
    expect(volumeRange({ min_db: -60, max_db: -10 })).toEqual({ lo: -60, hi: -10 });
  });

  it("the volume is on the DAC only for digital volume on Direct or Bypass", () => {
    expect(volumeOnDac({ hardware: false }, "direct", true)).toBe(true);
    expect(volumeOnDac({ hardware: false }, "dsp", false)).toBe(true);
    expect(volumeOnDac({ hardware: false }, "dsp", true)).toBe(false);
    expect(volumeOnDac({ hardware: true }, "direct", true)).toBe(false);
    expect(volumeOnDac(null, "direct", true)).toBe(false);
  });

  it("the display text", () => {
    const f = (db: number) => db.toFixed(1);
    expect(osdText(osdPayload(L(-12.5), VS), f)).toBe("-12.5 dB");
    expect(osdText(osdPayload(L(-12.5, true), VS), f)).toBe("Muted · -12.5 dB");
    expect(osdText({ ...osdPayload(L(-12.5), VS), note: "On the DAC" }, f)).toBe("On the DAC");
  });
});
