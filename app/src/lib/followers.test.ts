import { describe, expect, it } from "vitest";
import type { DeviceInfo, FollowerConfig, FollowerStatus } from "../api/types";
import { addBlockedReason, followerCandidates, followerChip, followerDetail, followerOn, followerRows, fmtPpm, fmtTrim, speedCapNote } from "./followers";

const dev = (card: string, active = false): DeviceInfo => ({ id: `hw:CARD=${card},DEV=0`, card, name: card, hdmi: false, loopback: false, active });
const fol = (card: string, patch: Partial<FollowerConfig> = {}): FollowerConfig => ({ device: `hw:CARD=${card},DEV=0`, enabled: true, trim_ms: 0, volume_db: 0, ...patch });
const status = (patch: Partial<FollowerStatus> = {}): FollowerStatus => ({
  device: "hw:CARD=USB,DEV=0",
  enabled: true,
  trim_ms: 0,
  volume_db: 0,
  state: "locked",
  offset_ms: 0,
  ppm: 12.34,
  phase_error_frames: 0.1,
  bandwidth_hz: 0.05,
  skips: 0,
  xruns: 2,
  latency_ms: 64,
  device_rate: 96000,
  period: 1024,
  error: null,
  ...patch,
});

describe("follower list", () => {
  const devices = [dev("DAC", true), dev("USB"), dev("PCH"), dev("SPK"), dev("HDMI")];

  it("offers every device but the main one and those already following", () => {
    const ids = followerCandidates(devices, "hw:CARD=DAC,DEV=0", [fol("USB")]).map((d) => d.card);
    expect(ids).toEqual(["PCH", "SPK", "HDMI"]);
    // The main device is excluded by id too (another spelling marks it active).
    const noActive = devices.map((d) => ({ ...d, active: false }));
    expect(followerCandidates(noActive, "hw:CARD=DAC,DEV=0", []).map((d) => d.card)).not.toContain("DAC");
  });

  it("offers nothing once three follow, and says why", () => {
    const full = [fol("USB"), fol("PCH"), fol("SPK")];
    const c = followerCandidates(devices, "hw:CARD=DAC,DEV=0", full);
    expect(c).toEqual([]);
    expect(addBlockedReason(c, full)).toMatch(/At most 3/);
    expect(addBlockedReason([], [])).toMatch(/No other device/);
    expect(addBlockedReason([dev("USB")], [])).toBeNull();
  });

  it("finds the follower a device move would collide with", () => {
    expect(followerOn("hw:CARD=USB,DEV=0", [fol("USB")])?.device).toBe("hw:CARD=USB,DEV=0");
    expect(followerOn("hw:CARD=PCH,DEV=0", [fol("USB")])).toBeNull();
  });

  it("merges drafts over the config and matches status by device", () => {
    const rows = followerRows([fol("USB"), fol("PCH")], [status({ device: "hw:CARD=PCH,DEV=0", state: "locking" })], { "hw:CARD=USB,DEV=0": { trim_ms: 2.5 } });
    expect(rows[0].config.trim_ms).toBe(2.5);
    expect(rows[0].status).toBeNull();
    expect(rows[1].status?.state).toBe("locking");
  });
});

describe("follower status", () => {
  it("names each state in plain words", () => {
    expect(followerChip(status()).text).toBe("In sync");
    expect(followerChip(status()).tone).toBe("ok");
    expect(followerChip(status({ state: "locking" })).text).toBe("Locking");
    expect(followerChip(status({ state: "muted", enabled: false })).title).toMatch(/turned off/);
    expect(followerChip(status({ state: "muted" })).title).toMatch(/Direct/);
    expect(followerChip(status({ state: "unaligned", offset_ms: 42.4 })).text).toBe("42 ms late");
    expect(followerChip(status({ state: "lost", error: "gone" })).tone).toBe("bad");
    expect(followerChip(status({ state: "off", error: "busy" })).title).toMatch(/busy/);
    expect(followerChip(null).text).toBe("Starting");
  });

  it("details the clock, offset and counters once it plays", () => {
    expect(followerDetail(status())).toBe("clock +12.3 ppm · 64.0 ms path · 96 kHz · 2 xruns");
    expect(followerDetail(status({ state: "unaligned", offset_ms: 42, xruns: 1, skips: 1 }))).toContain("42.0 ms late");
    expect(followerDetail(status({ state: "opening" }))).toBeNull();
    expect(followerDetail(status({ state: "lost", error: "the device is not connected" }))).toBe("the device is not connected");
  });

  it("formats ppm and trim with signs", () => {
    expect(fmtPpm(-4)).toBe("−4.0");
    expect(fmtPpm(0)).toBe("±0.0");
    expect(fmtTrim(1.5)).toBe("+1.5 ms");
    expect(fmtTrim(-2)).toBe("−2.0 ms");
    expect(fmtTrim(0)).toBe("0.0 ms");
  });

  it("notes the speed cap only with followers", () => {
    expect(speedCapNote(0, true)).toBeNull();
    expect(speedCapNote(1, false)).toMatch(/1\.996×/);
    expect(speedCapNote(2, true)).toMatch(/on now/);
  });
});
