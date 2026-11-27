import { describe, expect, it } from "vitest";
import type { DeviceInfo, FollowerConfig, FollowerStatus } from "../api/types";
import { addBlockedReason, followerCandidates, followerChip, followerDetail, followerEndToEndMs, followerLatencyTitle, followerOn, followerRows, followsVolume, fmtPpm, fmtTrim, ownChainLabel, speedCapNote } from "./followers";

const dev = (card: string, active = false): DeviceInfo => ({ id: `hw:CARD=${card},DEV=0`, card, name: card, hdmi: false, loopback: false, active, channels: [] });
const fol = (card: string, patch: Partial<FollowerConfig> = {}): FollowerConfig => ({ device: `hw:CARD=${card},DEV=0`, enabled: true, trim_ms: 0, volume_db: 0, follow_volume: false, ...patch });
const status = (patch: Partial<FollowerStatus> = {}): FollowerStatus => ({
  device: "hw:CARD=USB,DEV=0",
  enabled: true,
  trim_ms: 0,
  volume_db: 0,
  follow_volume: false,
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
  device_channels: 2,
  device_channels_max: 2,
  error: null,
  own_chain: [],
  latency: {
    rate_hz: 96000,
    chain_frames: 0,
    chain_us: 0,
    compensation_frames: 0,
    compensation_us: 0,
    alignment_frames: 0,
    alignment_us: 0,
    total_frames: 0,
    total_us: 0,
  },
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
    expect(followerDetail(status())).toBe("clock +12.3 ppm · 96 kHz · 2 xruns");
    // The main device's end-to-end basis, not its own path.
    expect(followerDetail(status(), 188)).toBe("clock +12.3 ppm · 188.0 ms latency · 96 kHz · 2 xruns");
    expect(followerDetail(status({ state: "locking" }), 188)).toBe("clock +12.3 ppm · 96 kHz · 2 xruns");
    expect(followerDetail(status({ state: "unaligned", offset_ms: 42, xruns: 1, skips: 1 }))).toContain("42.0 ms late");
    expect(followerDetail(status({ state: "opening" }))).toBeNull();
    expect(followerDetail(status({ state: "lost", error: "the device is not connected" }))).toBe("the device is not connected");
  });

  it("puts a follower's latency on the main device's basis, its path in the tooltip", () => {
    expect(followerEndToEndMs(status(), 188)).toBe(188);
    expect(followerEndToEndMs(status({ trim_ms: 2.5 }), 188)).toBe(190.5);
    expect(followerEndToEndMs(status({ state: "unaligned", offset_ms: 42 }), 188)).toBe(230);
    expect(followerEndToEndMs(status({ state: "locking" }), 188)).toBeNull();
    expect(followerEndToEndMs(status(), null)).toBeNull();
    expect(followerEndToEndMs(status(), 0)).toBeNull();
    expect(followerLatencyTitle(status(), 188)).toContain("64.0 ms");
    expect(followerLatencyTitle(status({ state: "lost" }), 188)).toBeUndefined();
  });

  it("says when a follower plays its own chain", () => {
    expect(ownChainLabel(fol("USB"), status())).toBeNull();
    const own = fol("USB", { chain_patch: { convolution: [] } });
    expect(ownChainLabel(own, status({ own_chain: ["convolution"] }))).toBe("Own chain: convolution differs");
    expect(ownChainLabel(own, status({ own_chain: ["eq", "limiter"] }))).toBe("Own chain: EQ, limiter differ");
    expect(ownChainLabel(own, status({ own_chain: [] }))).toBe("Own chain: same as the main chain");
    expect(ownChainLabel(own, null)).toBe("Own chain: convolution differs");
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

  it("says whether a follower follows the main volume, null for an engine without it", () => {
    expect(followsVolume(fol("USB"))).toBe(false);
    expect(followsVolume(fol("USB", { follow_volume: true }))).toBe(true);
    const old = { device: "hw:CARD=USB,DEV=0", enabled: true, trim_ms: 0, volume_db: 0 } as unknown as FollowerConfig;
    expect(followsVolume(old)).toBeNull();
  });

  it("shows the level a following device plays at, a draft first, its own volume otherwise", () => {
    const st = [status({ volume_db: -6, follow_volume: true, level_db: -12 })];
    const [following] = followerRows([fol("USB", { volume_db: -6, follow_volume: true })], st);
    expect(following.level).toBe(-12);
    const [own] = followerRows([fol("USB", { volume_db: -6 })], [status({ volume_db: -6, level_db: -6 })]);
    expect(own.level).toBe(-6);
    const [drafted] = followerRows([fol("USB", { volume_db: -6, follow_volume: true })], st, { "hw:CARD=USB,DEV=0": { volume_db: -3 } });
    expect(drafted.level).toBe(-3);
    const [old] = followerRows([fol("USB", { volume_db: -6, follow_volume: true })], [status({ volume_db: -6 })]);
    expect(old.level).toBe(-6);
  });
});
