import { describe, expect, it } from "vitest";
import type { VolumeConfig } from "../api/types";
import { softwareMixerAttenuated, volumeModeChange } from "./volumeSettings";

const baseVol = (over: Partial<VolumeConfig> = {}): VolumeConfig => ({
  volume_db: -20,
  balance_db: 0,
  muted: false,
  mode: "auto",
  hardware_db: null,
  max_volume_db: null,
  sink_volume: "ignore",
  guard: { mode: "off", max_rise_db_per_s: 6, stream_max_db: null },
  ...over,
});

describe("volumeModeChange", () => {
  it("defaults hardware_db to 0 when switching into software with none set", () => {
    const next = volumeModeChange(baseVol({ mode: "auto", hardware_db: null }), "software", -20);
    expect(next.mode).toBe("software");
    expect(next.hardware_db).toBe(0);
  });

  it("preserves an existing hardware_db when switching into software", () => {
    const next = volumeModeChange(baseVol({ mode: "hybrid", hardware_db: -12 }), "software", -12);
    expect(next.mode).toBe("software");
    expect(next.hardware_db).toBe(-12);
  });

  it("keeps hardware_db null when already in software", () => {
    const next = volumeModeChange(baseVol({ mode: "software", hardware_db: null }), "software", -20);
    expect(next.hardware_db).toBeNull();
  });

  it("prefills hybrid from status hardware_db or -10", () => {
    expect(volumeModeChange(baseVol(), "hybrid", -18).hardware_db).toBe(-18);
    expect(volumeModeChange(baseVol(), "hybrid", null).hardware_db).toBe(-10);
  });
});

describe("softwareMixerAttenuated", () => {
  it("flags software mode with no fixed hardware_db when mixer is below -0.5 dB", () => {
    expect(softwareMixerAttenuated({ mode: "software", hardware_db: null }, -20)).toBe(true);
    expect(softwareMixerAttenuated({ mode: "software", hardware_db: null }, -0.5)).toBe(false);
    expect(softwareMixerAttenuated({ mode: "software", hardware_db: null }, 0)).toBe(false);
    expect(softwareMixerAttenuated({ mode: "software", hardware_db: null }, null)).toBe(false);
    expect(softwareMixerAttenuated({ mode: "software", hardware_db: 0 }, -20)).toBe(false);
    expect(softwareMixerAttenuated({ mode: "auto", hardware_db: null }, -20)).toBe(false);
  });
});
