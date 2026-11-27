import { afterEach, describe, expect, it } from "vitest";
import { MockDaemon, MockTransport, type MockLatency } from "../api/mock";
import { DaemonError, ErrorCode } from "../api/transport";
import type { DaemonEvent, UpsamplerConfig } from "../api/types";
import { profilePeakGainDb } from "../lib/eqmath";
import { upsamplingSummary } from "../lib/summary";
import { isRecommendedUpsampler } from "../lib/upsamplerRecommended";
import { DEFAULT_CUSTOM_FIELDS, patchUpsamplerConfig, toUpsamplerConfig } from "../lib/upsamplerAdapter";
import { defaultDesign, refusalField, sameDesign } from "../lib/filterDesign";
import { AppStore, clearStash } from "./store";
import { meterStore } from "./meters";
import { spectrumStore } from "./spectrum";

const MANUAL: UpsamplerConfig = toUpsamplerConfig({ ...DEFAULT_CUSTOM_FIELDS, target: "off", preset: null });

const until = async (pred: () => boolean, ms = 2000) => {
  const t = Date.now();
  while (!pred()) {
    if (Date.now() - t > ms) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 5));
  }
};
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Records every call the store makes. */
class SpyTransport extends MockTransport {
  calls: { method: string; params: unknown }[] = [];
  refuse = new Set<string>();
  /** Calls of these methods wait for the promise before they are delivered. */
  hold = new Map<string, Promise<void>>();
  /** Calls of these methods run now; their reply waits for the promise. */
  holdReply = new Map<string, Promise<void>>();
  /** The next call of each of these methods times out. */
  timeoutOnce = new Set<string>();
  override async call<T>(method: string, params?: unknown): Promise<T> {
    this.calls.push({ method, params });
    if (this.refuse.has(method)) throw new DaemonError(ErrorCode.INVALID_REQUEST, `unknown method "${method}"`);
    if (this.timeoutOnce.delete(method)) throw new DaemonError(ErrorCode.TIMEOUT, "The engine did not answer in time.");
    const gate = this.hold.get(method);
    if (gate) await gate;
    const replyGate = this.holdReply.get(method);
    const result = await super.call<T>(method, params);
    if (replyGate) await replyGate;
    return result;
  }
  count(method: string) {
    return this.calls.filter((c) => c.method === method).length;
  }
}

let stores: AppStore[] = [];
async function setup(opts: { refuse?: string[]; latency?: MockLatency; legacyRatio?: boolean } = {}) {
  const daemon = new MockDaemon({ legacyRatio: opts.legacyRatio });
  const t = new SpyTransport(daemon, opts.latency);
  for (const m of opts.refuse ?? []) t.refuse.add(m);
  const s = new AppStore(t);
  stores.push(s);
  s.connect();
  await until(() => s.getState().conn === "up" && !!s.getState().config && !!s.getState().status);
  await until(() => s.getState().caps.presets !== null && s.getState().caps.devices !== null);
  return { s, t, daemon };
}

afterEach(() => {
  for (const s of stores) s.disconnect();
  stores = [];
  // Remembered settings (last preset, rate targets, limiter mode) live in
  // memory when localStorage is unavailable: start each test without them.
  clearStash();
});

describe("store", () => {
  it("loads config, status, streams, presets and devices on connect", async () => {
    const { s } = await setup();
    const st = s.getState();
    expect(st.config?.chain.eq?.bands).toHaveLength(7);
    expect(st.status?.state).toBe("playing");
    expect(st.streams.length).toBeGreaterThan(0);
    expect(st.presets.map((p) => p.name)).toContain("HD650 AutoEQ");
    expect(st.devices.length).toBeGreaterThan(1);
    expect(st.caps).toMatchObject({ presets: true, devices: true, volume: true, convChain: true, perChannel: true });
  });

  it("feeds meters into the meter store, not React state", async () => {
    const { s } = await setup();
    await until(() => meterStore.latest !== null);
    expect(meterStore.latest?.meters.peak_db).toHaveLength(2);
    expect(Object.keys(s.getState())).not.toContain("meters");
  });

  it("commits a band edit and renders the daemon's echo", async () => {
    const { s, daemon } = await setup();
    s.updateBand(0, { gain_db: 2 });
    expect(s.eq()?.bands[0].gain_db).toBe(2); // draft while in flight
    await until(() => s.getState().eqDraft === null);
    expect(daemon.config.chain.eq?.bands[0].gain_db).toBe(2);
    expect(s.getState().config?.chain.eq?.bands[0].gain_db).toBe(2);
    // auto preamp is on: the daemon got the matching preamp
    expect(daemon.config.chain.eq?.preamp_db).toBeLessThan(0);
  });

  it("sets one convolution guard and keeps the other", async () => {
    const { s, t, daemon } = await setup();
    s.setConvolutionSafety({ subsonic_hz: 30 });
    await until(() => t.count("set_convolution_safety") === 1 && s.getState().config?.convolution_safety?.subsonic_hz === 30);
    expect(daemon.config.convolution_safety).toEqual({ max_boost_db: 12, subsonic_hz: 30 });
  });

  it("loads a room calibration, keeps it out of the IR list, and unloads it", async () => {
    const { s, t, daemon } = await setup();
    await s.loadCalibration();
    await until(() => s.getState().status?.calibration?.label === "desk");
    expect(t.count("bundle_load")).toBe(1);
    expect(daemon.config.chain.convolution.some((c) => c.calibration)).toBe(true);
    expect(s.convolution().some((c) => c.calibration)).toBe(false);
    // Adding an IR sends the user's list; the calibration stays.
    await s.addImpulseResponse();
    await until(() => t.count("set_convolution") === 1);
    const sent = t.calls.find((c) => c.method === "set_convolution")?.params as { calibration?: boolean }[];
    expect(sent.some((c) => c.calibration)).toBe(false);
    expect(daemon.config.chain.convolution.filter((c) => c.calibration)).toHaveLength(1);
    await s.unloadCalibration();
    await until(() => !s.getState().status?.calibration);
    expect(daemon.config.chain.convolution.some((c) => c.calibration)).toBe(false);
  });

  it("sets the room calibration's main level and resets it to the bundle's", async () => {
    const { s, t, daemon } = await setup();
    await s.loadCalibration();
    await until(() => !!s.getState().config?.calibration);
    await s.setCalibrationGain(null, -3);
    expect(t.calls.find((c) => c.method === "bundle_gain")?.params).toEqual({ gain_db: -3 });
    expect(daemon.config.chain.convolution.find((c) => c.calibration)?.gain_db).toBe(-3);
    expect(s.getState().config?.calibration?.master?.gain_db).toBe(-3);
    expect(s.getState().config?.calibration?.master_manifest_gain_db).toBe(0);
    await s.setCalibrationGain(null, null);
    expect(s.getState().config?.calibration?.master?.gain_db).toBe(0);
    expect(s.getState().config?.calibration?.master_manifest_gain_db).toBeUndefined();
  });

  it("throttles drags to ~25 Hz and always sends the final value", async () => {
    const { s, t, daemon } = await setup();
    const before = t.count("set_eq");
    s.beginEqDrag();
    for (let i = 0; i < 30; i++) {
      s.updateBand(1, { freq_hz: 60 + i }, "drag");
      await sleep(4);
    }
    s.updateBand(1, { freq_hz: 400 }, "drag");
    s.endEqDrag();
    await until(() => s.getState().eqDraft === null);
    const sent = t.count("set_eq") - before;
    expect(sent).toBeLessThan(12); // ~31 edits over ~150 ms
    expect(sent).toBeGreaterThanOrEqual(2);
    expect(daemon.config.chain.eq?.bands[1].freq_hz).toBe(400);
    // One undo step for the whole drag.
    s.undo();
    await until(() => daemon.config.chain.eq?.bands[1].freq_hz === 57);
  });

  it("undo and redo walk the EQ history", async () => {
    const { s, daemon } = await setup();
    s.addBand({ freq_hz: 2000, gain_db: 3 });
    await until(() => daemon.config.chain.eq?.bands.length === 8);
    s.deleteBand(0);
    await until(() => daemon.config.chain.eq?.bands.length === 7);
    expect(s.getState().canUndo).toBe(true);
    s.undo();
    await until(() => daemon.config.chain.eq?.bands.length === 8);
    s.undo();
    await until(() => daemon.config.chain.eq?.bands.length === 7);
    expect(daemon.config.chain.eq?.bands[0].kind).toBe("low_shelf");
    s.redo();
    await until(() => daemon.config.chain.eq?.bands.length === 8);
    expect(s.getState().canRedo).toBe(true);
  });

  it("manages layered L+R, L, and R sections", async () => {
    const { s, daemon } = await setup();
    s.addSection("right");
    await until(() => !!daemon.config.chain.eq?.right);
    expect(s.getState().eqMode).toBe("right");
    s.addBand({ freq_hz: 138, gain_db: -4, q: 3 });
    await until(() => daemon.config.chain.eq?.right?.bands.length === 1);
    // Linked section stays intact on top of the right section.
    expect(daemon.config.chain.eq?.bands).toHaveLength(7);
    s.toggleSectionEnabled("right");
    await until(() => daemon.config.chain.eq?.right?.enabled === false);
    s.clearSection("right");
    await until(() => daemon.config.chain.eq?.right?.bands.length === 0);
    s.removeSection("right");
    await until(() => !daemon.config.chain.eq?.right);
    expect(s.getState().eqMode).toBe("linked");
  });

  it("mock: layered EQ passthrough, preset band count, A/B level match, and legacy normalization", async () => {
    const daemon = new MockDaemon({ split: true });
    const t = new SpyTransport(daemon);
    const s = new AppStore(t);
    stores.push(s);
    s.connect();
    await until(() => s.getState().conn === "up" && !!s.getState().config && !!s.getState().status);
    await until(() => s.getState().presets.length > 0);

    // Split mock starts with 7 linked + 1 right band = 8 total.
    expect(daemon.config.chain.eq?.bands).toHaveLength(7);
    expect(daemon.config.chain.eq?.right?.bands).toHaveLength(1);
    expect(s.getState().presets.find((p) => p.name === "HD650 AutoEQ")?.bands).toBe(8);

    // Analytic A/B offset includes both L and R layers.
    expect(s.getState().status?.ab?.offset_db).not.toBe(0);

    // Disabling all active sections + zero preamp marks EQ inactive.
    s.setManualPreamp(0);
    s.toggleSectionEnabled("linked");
    s.toggleSectionEnabled("right");
    await until(() => daemon.config.chain.eq?.linked_enabled === false && daemon.config.chain.eq?.right?.enabled === false);
    // Still broken only if conv/upsampler are on; verify eq_active reason disappears when conv+ups are off.
    s.setOutput({ headroom_db: 0 });
    s.setConvEnabled(false);
    s.setUpsamplingEnabled(false);
    await until(() => s.getState().status?.passthrough.broken_because.every((b) => b.reason !== "dsp_active") ?? false);

    // Legacy v1 split profile sent to set_eq is migrated to v2 on write.
    await t.call("set_eq", {
      name: "legacy",
      preamp_db: -2,
      bands: [{ kind: "peaking", freq_hz: 100, gain_db: 2, q: 1, enabled: true }],
      right: { preamp_db: -1, bands: [{ kind: "peaking", freq_hz: 200, gain_db: -1, q: 1, enabled: true }] },
    });
    expect(daemon.config.chain.eq?.version).toBe(2);
    expect(daemon.config.chain.eq?.bands).toEqual([]);
    expect(daemon.config.chain.eq?.left).toMatchObject({ enabled: true, preamp_db: -2 });
    expect(daemon.config.chain.eq?.right).toMatchObject({ enabled: true, preamp_db: -1 });

    // set_dc_block normalizes omitted fields like serde ({} -> enabled: true, cutoff_hz: 5).
    await t.call("set_eq", null);
    await until(() => s.getState().status?.passthrough.broken_because.every((b) => b.reason !== "dsp_active") ?? false);

    await t.call("set_dc_block", {});
    expect(daemon.config.chain.dc_block).toEqual({ enabled: true, cutoff_hz: 5 });
    await until(() => s.getState().status?.passthrough.broken_because.some((b) => b.reason === "dsp_active") ?? false);

    await t.call("set_dc_block", { cutoff_hz: 8 });
    expect(daemon.config.chain.dc_block).toEqual({ enabled: true, cutoff_hz: 8 });
    await until(() => s.getState().status?.passthrough.broken_because.some((b) => b.reason === "dsp_active") ?? false);

    await t.call("set_dc_block", { enabled: false });
    expect(daemon.config.chain.dc_block).toEqual({ enabled: false, cutoff_hz: 5 });
    await until(() => s.getState().status?.passthrough.broken_because.every((b) => b.reason !== "dsp_active") ?? false);

    await expect(t.call("set_dc_block", { cutoff_hz: 25 })).rejects.toMatchObject({ code: ErrorCode.INVALID_PARAMS });
    await t.call("set_dc_block", null);
    expect(daemon.config.chain.dc_block).toBeNull();
  });

  it("enforces dc_block type checks, defaults, and cutoff validation across set_dc_block, set_ab, and set_config", async () => {
    const { daemon, t } = await setup();

    // Omitted params clears the stage (`SetDcBlock(None)` in Rust).
    await t.call("set_dc_block", { cutoff_hz: 9 });
    expect(daemon.config.chain.dc_block).toEqual({ enabled: true, cutoff_hz: 9 });
    await t.call("set_dc_block");
    expect(daemon.config.chain.dc_block).toBeNull();

    // Serde tuple-sequence shapes for DcBlockConfig: `[]`, `[enabled]`, `[enabled, cutoff_hz]`.
    await t.call("set_dc_block", []);
    expect(daemon.config.chain.dc_block).toEqual({ enabled: true, cutoff_hz: 5 });
    await t.call("set_dc_block", [false]);
    expect(daemon.config.chain.dc_block).toEqual({ enabled: false, cutoff_hz: 5 });
    await t.call("set_dc_block", [true, 12]);
    expect(daemon.config.chain.dc_block).toEqual({ enabled: true, cutoff_hz: 12 });

    for (const bad of [
      17,
      "bad",
      true,
      [null],
      ["false"],
      [true, "5"],
      [true, 25],
      [true, 5, 1],
      { enabled: null },
      { enabled: "false" },
      { cutoff_hz: null },
      { cutoff_hz: "5" },
      { cutoff_hz: Number.NaN },
      { cutoff_hz: 0.5 },
      { cutoff_hz: 25 },
    ]) {
      await expect(t.call("set_dc_block", bad)).rejects.toMatchObject({ code: ErrorCode.INVALID_PARAMS });
    }

    // A minimal chain B gets Serde defaults; its target, limiter and output follow A.
    await t.call("set_ab", { b: { chain: { dc_block: {} } } });
    expect(daemon.config.ab?.chain_b?.dc_block).toEqual({ enabled: true, cutoff_hz: 5 });
    expect(daemon.config.ab?.chain_b?.upsampler.target).toEqual(daemon.config.chain.upsampler.target);
    expect(daemon.config.ab?.chain_b?.output).toEqual(daemon.config.chain.output);

    // A patch B is chain A plus the change, normalized like A.
    const baseChain = structuredClone(daemon.config.chain);
    await t.call("set_ab", { b: { patch: { dc_block: { cutoff_hz: 7 } } }, label: "DC 7", level: { mode: "off" } });
    expect(daemon.config.ab?.label).toBe("DC 7");
    expect(daemon.config.ab?.chain_b?.dc_block).toEqual({ enabled: true, cutoff_hz: 7 });
    expect(daemon.config.ab?.chain_b?.upsampler).toEqual(baseChain.upsampler);

    // Shared fields and bad values are refused; B is left as it was.
    for (const patch of [{ output: { headroom_db: 1 } }, { limiter: {} }, { upsampler: { target: { ratio: 2 } } }, [1]]) {
      await expect(t.call("set_ab", { b: { patch } })).rejects.toMatchObject({ code: ErrorCode.INVALID_PARAMS });
    }
    for (const bad of [17, [true, 25], { enabled: "false" }, { cutoff_hz: 25 }]) {
      await expect(t.call("set_ab", { b: { patch: { dc_block: bad } } })).rejects.toMatchObject({ code: ErrorCode.INVALID_PARAMS });
    }
    expect(daemon.config.ab?.chain_b?.dc_block).toEqual({ enabled: true, cutoff_hz: 7 });

    // set_config normalizes chain A, and B follows it outside the patch.
    const nextCfg = structuredClone(daemon.config);
    (nextCfg.chain as { dc_block: unknown }).dc_block = {};
    nextCfg.chain.pitch = { ...nextCfg.chain.pitch, cents: 12 };
    await t.call("set_config", nextCfg);
    expect(daemon.config.chain.dc_block).toEqual({ enabled: true, cutoff_hz: 5 });
    expect(daemon.config.ab?.chain_b?.dc_block).toEqual({ enabled: true, cutoff_hz: 7 });
    expect(daemon.config.ab?.chain_b?.pitch.cents).toBe(12);

    for (const bad of [17, [true, 25], { enabled: "false" }, { cutoff_hz: 25 }]) {
      const badA = structuredClone(daemon.config);
      (badA.chain as { dc_block: unknown }).dc_block = bad;
      await expect(t.call("set_config", badA)).rejects.toMatchObject({ code: ErrorCode.INVALID_PARAMS });

      const badB = structuredClone(daemon.config);
      badB.ab!.b = { patch: { dc_block: bad } as never };
      await expect(t.call("set_config", badB)).rejects.toMatchObject({ code: ErrorCode.INVALID_PARAMS });
    }
  });

  it("marks the preset modified, saves and reverts", async () => {
    const { s } = await setup();
    expect(s.presetModified()).toBe(false);
    s.updateBand(2, { q: 3 });
    await until(() => s.presetModified());
    await s.revertPreset();
    await until(() => !s.presetModified());
    s.updateBand(2, { q: 3 });
    await until(() => s.presetModified());
    await s.savePreset();
    await until(() => !s.presetModified());
    expect(await s.savePresetAs("Mine")).toBe(true);
    await until(() => s.getState().presets.some((p) => p.name === "Mine"));
    expect(s.activePreset()).toBe("Mine");
    expect(await s.renamePreset("Mine", "Ours")).toBe(true);
    await until(() => s.activePreset() === "Ours");
    await s.deletePreset("Ours");
    await until(() => !s.getState().presets.some((p) => p.name === "Ours"));
  });

  it("imports a ParametricEQ.txt as an EQ preset and loads it, touching nothing else", async () => {
    const { s, t } = await setup();
    const before = structuredClone(s.getState().config!.chain);
    await s.importApoPreset("Preamp: -4 dB\nFilter 1: ON PK Fc 1000 Hz Gain 3 dB Q 1\n", "My HP ParametricEQ.txt");
    expect(t.calls.some((c) => c.method === "import_apo" && (c.params as { name: string }).name === "My HP")).toBe(true);
    await until(() => s.activePreset("eq") === "My HP");
    const after = s.getState().config!.chain;
    expect(after.eq?.bands).toHaveLength(1);
    // Only the EQ changed: convolution, upsampling and output stay.
    expect({ ...after, eq: null }).toEqual({ ...before, eq: null });
    expect(s.getState().toast?.text).toBe("Imported “My HP”");
    expect(s.getState().presets.find((p) => p.name === "My HP")).toMatchObject({ part: "eq", bands: 1, convolution: 0 });
    // Not a global preset.
    expect(s.presetsOf().some((p) => p.name === "My HP")).toBe(false);
  });

  it("switches EQ presets while the upsampler and harmonics stay put", async () => {
    const { s } = await setup();
    void s.setStageEnabled("harmonics", true);
    await until(() => !!s.getState().config?.chain.harmonics?.enabled);
    const before = structuredClone(s.getState().config!.chain);
    expect(s.presetsOf("eq").map((p) => p.name)).toEqual(["HD650 AutoEQ", "Room + bass shelf"]);
    await s.loadPreset("Room + bass shelf", "eq");
    await until(() => s.activePreset("eq") === "Room + bass shelf");
    const after = s.getState().config!.chain;
    expect(after.eq?.name).toBe("Room + bass shelf");
    expect(after.upsampler).toEqual(before.upsampler);
    expect(after.harmonics ?? null).toEqual(before.harmonics ?? null);
    expect({ ...after, eq: null }).toEqual({ ...before, eq: null });
    // The global preset's marker now shows the EQ differs; the EQ's does not.
    await until(() => s.presetModified());
    expect(s.presetModified("eq")).toBe(false);
    // An upsampling change leaves the EQ preset unmodified.
    await s.loadPreset("Desk ×4", "upsampling");
    await until(() => s.activePreset("upsampling") === "Desk ×4");
    expect(s.presetModified("eq")).toBe(false);
    expect(s.getState().config!.chain.eq?.name).toBe("Room + bass shelf");
  });

  it("starts a blank page preset: that part at its defaults, unnamed, the saved ones kept", async () => {
    const { s } = await setup();
    await s.loadPreset("Room + bass shelf", "eq");
    await until(() => s.activePreset("eq") === "Room + bass shelf");
    s.updateBand(0, { q: 3 });
    await until(() => s.presetModified("eq"));
    const before = structuredClone(s.getState().config!.chain);
    await s.blankPreset("eq");
    await until(() => s.activePreset("eq") === null);
    const after = s.getState().config!.chain;
    // An empty EQ that is on (no EQ would read as switched off).
    expect(after.eq).toMatchObject({ bands: [], preamp_db: 0, linked_enabled: true });
    expect(after.eq?.left ?? null).toBeNull();
    expect(after.eq?.tone ?? null).toBeNull();
    expect(after.eq?.tilt ?? null).toBeNull();
    expect({ ...after, eq: null }).toEqual({ ...before, eq: null });
    expect(s.presetModified("eq")).toBe(false);
    // Like a load, it replaces the EQ's undo history.
    expect(s.getState().canUndo).toBe(false);
    expect(s.presetsOf("eq").map((p) => p.name)).toContain("Room + bass shelf");
    // "Save as" names it.
    expect(await s.savePresetAs("Just tone", "eq")).toBe(true);
    await until(() => s.activePreset("eq") === "Just tone");
  });

  it("saves a global preset of some parts and loads only those", async () => {
    const { s, t } = await setup();
    expect(await s.savePresetAs("Just EQ", undefined, ["eq"])).toBe(true);
    const save = t.calls.find((c) => c.method === "save_preset" && (c.params as { name: string }).name === "Just EQ");
    expect(save?.params).toMatchObject({ parts: ["eq"] });
    await until(() => s.activePreset() === "Just EQ");
    const eq = structuredClone(s.getState().config!.chain.eq);
    // Change the EQ and the upsampler; loading brings back only the EQ.
    await s.loadPreset("Room + bass shelf", "eq");
    await s.loadPreset("Desk ×4", "upsampling");
    await until(() => s.presetModified());
    const ups = structuredClone(s.getState().config!.chain.upsampler);
    await s.loadPreset("Just EQ");
    await until(() => !s.presetModified());
    expect(s.getState().config!.chain.eq).toEqual(eq);
    expect(s.getState().config!.chain.upsampler).toEqual(ups);
    // Loading it cleared the EQ page's name; the upsampling page keeps its own.
    expect(s.activePreset("eq")).toBeNull();
    expect(s.activePreset("upsampling")).toBe("Desk ×4");
    // Every part ticked sends no list (today's whole-chain preset).
    expect(await s.savePresetAs("All", undefined, ["stereo", "eq", "convolution", "upsampling", "harmonics", "output"])).toBe(true);
    const all = t.calls.find((c) => c.method === "save_preset" && (c.params as { name: string }).name === "All");
    expect(all?.params).not.toHaveProperty("parts");
  });

  it("does not store a file without filters as a preset", async () => {
    const { s, t } = await setup();
    await s.importApoPreset("this is not an EQ\n", "notes.txt");
    expect(t.count("import_apo")).toBe(0);
    expect(s.getState().toast?.text).toMatch(/No filters found/);
  });

  it("marks the configured device by the daemon's active flag, whatever its spelling", async () => {
    const { s, daemon, t } = await setup();
    daemon.config.device = "hw:DAC,0,0";
    await s.refreshConfig();
    await until(() => s.getState().devices.length > 0);
    expect(s.currentDevice()?.id).toBe("hw:CARD=DAC,DEV=0");
    const n = t.count("list_devices");
    await s.refreshConfig();
    await s.refreshConfig();
    expect(t.count("list_devices")).toBe(n);
  });

  it("save refuses an existing name without overwrite (BAD_STATE), like the daemon", async () => {
    const { t } = await setup();
    await expect(t.call("save_preset", { name: "Flat", overwrite: false })).rejects.toMatchObject({ code: ErrorCode.BAD_STATE });
    const info = await t.call<{ name: string; bands: number }>("save_preset", { name: "Flat", overwrite: true });
    expect(info).toMatchObject({ name: "Flat", bands: 7 });
  });

  it("maps Bypass to set_dsp_enabled", async () => {
    const { s, daemon } = await setup();
    s.setBypass(true);
    await until(() => daemon.config.dsp_enabled === false);
    await until(() => s.bypassed());
    await until(() => s.getState().status?.passthrough.broken_because.every((b) => b.reason !== "dsp_active") ?? false);
  });

  it("sends volume, balance and mute, throttled while dragging", async () => {
    const { s, t, daemon } = await setup();
    const before = t.count("set_volume_guarded");
    for (let i = 0; i < 20; i++) {
      s.setVolume({ volume_db: -20 + i * 0.5 }, "drag");
      await sleep(2);
    }
    s.setVolume({ volume_db: -5 });
    await until(() => s.getState().volumeDraft === null);
    expect(t.count("set_volume_guarded") - before).toBeLessThan(8);
    expect(daemon.volume.volume_db).toBe(-5);
    s.setVolume({ muted: true, balance_db: -2 });
    await until(() => daemon.volume.muted && daemon.volume.balance_db === -2);
  });

  it("the last throttled drag or wheel step reaches the daemon's stored config, even without a commit", async () => {
    const { s, daemon } = await setup();
    for (let i = 0; i < 15; i++) {
      s.setVolume({ volume_db: -20 + i * 0.5 }, "drag");
      await sleep(1);
    }
    await until(() => daemon.volume.volume_db === -13);
    expect(daemon.config.volume?.volume_db).toBe(-13);
    // The wheel's closing commit repeats the value: still one level, stored.
    s.setVolume({ volume_db: -13 });
    await s.whenIdle();
    expect(daemon.config.volume?.volume_db).toBe(-13);
    expect(s.getState().volumeDraft).toBeNull();
    expect(s.getState().status?.volume?.volume_db).toBe(-13);
  });

  it("a drag interrupted by a reconnect never survives it", async () => {
    const { s, t, daemon } = await setup({ latency: { request: 4, reply: 30 } });
    // An uncommitted drag up to 0 dB (pointer still held), then the
    // daemon restarts at -30 dB.
    s.setVolume({ volume_db: -10 }, "drag");
    s.setVolume({ volume_db: 0 }, "drag");
    daemon.volume = { ...daemon.volume, volume_db: -30 };
    if (daemon.config.volume) daemon.config.volume = { ...daemon.config.volume, volume_db: -30 };
    daemon.restartEngine();
    await until(() => s.getState().conn === "down");
    await until(() => s.getState().conn === "up" && s.getState().status?.volume?.volume_db === -30);
    const sentAfter = t.count("set_volume_guarded");
    await sleep(120);
    // Nothing queued or in flight from the old connection lands now.
    expect(t.count("set_volume_guarded")).toBe(sentAfter);
    expect(daemon.volume.volume_db).toBe(-30);
    expect(s.getState().volumeDraft).toBeNull();
    expect(s.volume()?.volume_db).toBe(-30);
    // Volume Down is a step down from what the daemon reports.
    const v = s.volume()!;
    s.setVolume({ volume_db: v.volume_db - 0.5 });
    await s.whenIdle();
    expect(daemon.volume.volume_db).toBe(-30.5);
    expect(daemon.config.volume?.volume_db).toBe(-30.5);
    expect(s.getState().status?.volume?.volume_db).toBe(-30.5);
  });

  it("volume, balance and mute wait for the new daemon's own status after a reconnect", async () => {
    const { s, t, daemon } = await setup({ latency: { request: 4, reply: 0, event: 400 } });
    expect(s.volume()?.volume_db).toBe(-12);
    let release!: () => void;
    t.hold.set("status", new Promise<void>((r) => (release = r)));
    daemon.volume = { ...daemon.volume, volume_db: -30 };
    if (daemon.config.volume) daemon.config.volume = { ...daemon.config.volume, volume_db: -30 };
    daemon.restartEngine();
    await until(() => s.getState().conn === "down");
    await until(() => s.getState().conn === "up");
    // Up, but no status from the new daemon yet: no level, and nothing
    // can be sent from the old one's.
    expect(s.volume()).toBeNull();
    const sent = t.count("set_volume_guarded");
    s.setVolume({ volume_db: -12.5 });
    s.setVolume({ balance_db: -2 });
    s.setVolume({ muted: true });
    await sleep(30);
    expect(t.count("set_volume_guarded")).toBe(sent);
    expect(daemon.volume).toMatchObject({ volume_db: -30, balance_db: 0, muted: false });
    t.hold.delete("status");
    release();
    await until(() => s.volume()?.volume_db === -30);
    s.setVolume({ volume_db: s.volume()!.volume_db - 0.5 });
    await s.whenIdle();
    expect(daemon.volume.volume_db).toBe(-30.5);
  });

  it("a set_volume dispatched before a restart and delivered after it is refused", async () => {
    const { s, t, daemon } = await setup();
    let release!: () => void;
    t.hold.set("set_volume_guarded", new Promise<void>((r) => (release = r)));
    s.setVolume({ volume_db: 0, balance_db: -2, muted: false });
    await until(() => t.count("set_volume_guarded") > 0);
    daemon.volume = { ...daemon.volume, volume_db: -30, muted: true };
    if (daemon.config.volume) daemon.config.volume = { ...daemon.config.volume, volume_db: -30, muted: true };
    daemon.restartEngine();
    await until(() => s.getState().conn === "down");
    await until(() => s.getState().conn === "up" && s.volume()?.volume_db === -30);
    // The old request now reaches the replacement daemon.
    t.hold.delete("set_volume_guarded");
    release();
    await sleep(50);
    expect(daemon.volume).toMatchObject({ volume_db: -30, balance_db: 0, muted: true });
    expect(daemon.config.volume).toMatchObject({ volume_db: -30, balance_db: 0, muted: true });
    expect(s.volume()).toMatchObject({ volume_db: -30, muted: true });
  });

  it("a change refused as stale drops the gesture and reads the new daemon's level", async () => {
    const { s, t, daemon } = await setup();
    // The request socket reached a replacement daemon before the event
    // connection noticed anything.
    daemon.instance = "replacement";
    daemon.volume = { ...daemon.volume, volume_db: -30 };
    s.setVolume({ volume_db: -11 }, "drag");
    await until(() => s.getState().status?.volume?.volume_db === -30 && s.getState().volumeDraft === null);
    expect(daemon.volume.volume_db).toBe(-30);
    expect(s.volume()?.volume_db).toBe(-30);
    // The dropped drag's release is not resent from the old level...
    const before = t.count("set_volume_guarded");
    s.setVolume({ volume_db: -11 });
    await s.whenIdle();
    expect(t.count("set_volume_guarded")).toBe(before);
    expect(daemon.volume.volume_db).toBe(-30);
    // ...and the next change starts from the new daemon's.
    s.setVolume({ volume_db: s.volume()!.volume_db - 0.5 });
    await s.whenIdle();
    expect(t.count("set_volume_guarded")).toBe(before + 1);
    expect(daemon.volume.volume_db).toBe(-30.5);
  });

  it("every app volume write is a compare-and-set on the status it came from", async () => {
    const { s, t } = await setup();
    const v0 = s.getState().status!.volume!;
    s.setVolume({ volume_db: -13 }, "drag");
    s.setVolume({ volume_db: -14 });
    await s.whenIdle();
    const sends = t.calls.filter((c) => c.method === "set_volume_guarded").map((c) => c.params as Record<string, unknown>);
    expect(sends.length).toBeGreaterThan(0);
    for (const p of sends) {
      expect(p.if_instance).toBe(v0.instance);
      expect(typeof p.if_revision).toBe("number");
    }
    // The first step from the status's revision, the next from its echo.
    expect(sends[0].if_revision).toBe(v0.revision);
    if (sends.length > 1) expect(sends[1].if_revision).toBeGreaterThan(v0.revision!);
  });

  it("a silent re-subscribe to a replaced daemon during a balance drag: nothing of the drag lands", async () => {
    const { s, t, daemon } = await setup();
    s.setVolume({ balance_db: -1 }, "drag");
    await s.whenIdle();
    // The native client re-subscribed (spectrum off) to a replacement:
    // no connection events, just the new daemon's snapshot.
    daemon.instance = "f".repeat(32);
    daemon.revision = 1;
    daemon.volume = { ...daemon.volume, volume_db: -30, balance_db: 0, muted: true };
    if (daemon.config.volume) daemon.config.volume = { ...daemon.config.volume, volume_db: -30, balance_db: 0, muted: true };
    daemon.snapshot(["status"]);
    // The drag goes on, then is released.
    for (let i = 2; i <= 6; i++) {
      s.setVolume({ balance_db: -i }, "drag");
      await sleep(15);
    }
    s.setVolume({ balance_db: -6 });
    await s.whenIdle();
    await until(() => s.volume() !== null);
    expect(daemon.volume).toMatchObject({ volume_db: -30, balance_db: 0, muted: true });
    expect(s.volume()).toMatchObject({ volume_db: -30, balance_db: 0, muted: true });
    // A new gesture starts from the replacement's level.
    const before = t.count("set_volume_guarded");
    s.setVolume({ balance_db: -0.5 });
    await s.whenIdle();
    expect(t.count("set_volume_guarded")).toBe(before + 1);
    expect(daemon.volume).toMatchObject({ volume_db: -30, balance_db: -0.5, muted: true });
  });

  it("a delayed status snapshot never replaces a newer push from the same daemon", async () => {
    const { s, t, daemon } = await setup();
    let release!: () => void;
    t.holdReply.set("status", new Promise<void>((r) => (release = r)));
    daemon.restartEngine();
    await until(() => s.getState().conn === "down");
    await until(() => s.getState().conn === "up" && t.count("status") > 0);
    await sleep(20);
    // The snapshot (taken at -12 dB) is on its way; the level changes and
    // the push arrives first.
    daemon.setLevel({ volume_db: -25 });
    await until(() => s.volume()?.volume_db === -25);
    const pushed = s.getState().status!.volume!.revision!;
    t.holdReply.delete("status");
    release();
    await sleep(50);
    expect(s.getState().status!.volume!.revision).toBe(pushed);
    expect(s.volume()?.volume_db).toBe(-25);
    s.setVolume({ volume_db: s.volume()!.volume_db - 0.5 });
    await s.whenIdle();
    expect(daemon.volume.volume_db).toBe(-25.5);
  });

  it("a daemon without instance and revision gets no volume writes", async () => {
    const daemon = new MockDaemon();
    daemon.legacyVolume = true;
    const t = new SpyTransport(daemon);
    const s = new AppStore(t);
    stores.push(s);
    s.connect();
    await until(() => s.getState().conn === "up" && !!s.getState().status?.volume);
    expect(s.volume()).toBeNull();
    s.setVolume({ volume_db: -20 });
    s.setVolume({ muted: true });
    await sleep(100);
    expect(t.count("set_volume_guarded")).toBe(0);
    // And it does not keep re-reading the status for a revision.
    const reads = t.count("status");
    await sleep(300);
    expect(t.count("status")).toBeLessThanOrEqual(reads + 1);
  });

  it("a stale refusal after the replacement's snapshot: the refresh retries until it gets the status", async () => {
    const { s, t, daemon } = await setup();
    let release!: () => void;
    t.hold.set("set_volume_guarded", new Promise<void>((r) => (release = r)));
    s.setVolume({ volume_db: -3 }, "drag");
    await until(() => t.count("set_volume_guarded") > 0);
    // Silently replaced; its snapshot arrives first.
    daemon.instance = "e".repeat(32);
    daemon.revision = 1;
    daemon.volume = { ...daemon.volume, volume_db: -30 };
    daemon.snapshot(["status"]);
    await until(() => s.getState().status?.volume?.instance === "e".repeat(32));
    // Then the old write is refused, and the first refresh times out.
    t.timeoutOnce.add("status");
    const reads = t.count("status");
    t.hold.delete("set_volume_guarded");
    release();
    s.setVolume({ volume_db: -3 });
    // Refused: the controls wait while the first read times out, then the
    // retry brings the replacement's level back.
    await until(() => s.volume() === null);
    await until(() => t.count("status") >= reads + 2 && s.volume() !== null, 3000);
    expect(daemon.volume.volume_db).toBe(-30);
    expect(s.volume()?.volume_db).toBe(-30);
    s.setVolume({ volume_db: s.volume()!.volume_db - 0.5 });
    await s.whenIdle();
    expect(daemon.volume.volume_db).toBe(-30.5);
  });

  it("a refused gesture's queued job never borrows a later gesture's revision", async () => {
    const { s, t, daemon } = await setup();
    // X: a balance step on its way to the daemon.
    let releaseX!: () => void;
    t.hold.set("set_volume_guarded", new Promise<void>((r) => (releaseX = r)));
    s.setVolume({ balance_db: -1 });
    await until(() => t.count("set_volume_guarded") > 0);
    // A DSP toggle, then A (a balance edit made from X's draft, so from
    // -12 dB unmuted) queued behind it, then another change after A.
    let releaseD!: () => void;
    t.hold.set("set_dsp_enabled", new Promise<void>((r) => (releaseD = r)));
    s.setBypass(true);
    s.setVolume({ balance_db: -2 });
    s.setBypass(false);
    // The desktop moves the level; X is refused.
    daemon.setLevel({ volume_db: -40, muted: true });
    t.hold.delete("set_volume_guarded");
    releaseX();
    await until(() => s.volume()?.volume_db === -40 && s.volume()?.muted === true);
    // After the recovery, B: a new balance drag from the current revision,
    // still in the throttle when A's turn comes.
    s.setVolume({ balance_db: -0.5 }, "drag");
    s.setVolume({ balance_db: -0.6 }, "drag");
    t.hold.delete("set_dsp_enabled");
    releaseD();
    s.setVolume({ balance_db: -0.7 });
    await s.whenIdle();
    // A never ran on B's revision: nothing went back to -12 dB unmuted.
    expect(daemon.volume).toMatchObject({ volume_db: -40, muted: true, balance_db: -0.7 });
    const sent = t.calls.filter((c) => c.method === "set_volume_guarded").map((c) => c.params as { balance_db: number });
    expect(sent.map((p) => p.balance_db)).not.toContain(-2);
  });

  it("a mute click does not let a refused drag's pointer resume", async () => {
    const { s, daemon } = await setup();
    const drag = s.beginVolumeGesture();
    s.setVolume({ volume_db: -13 }, "drag", drag);
    await s.whenIdle();
    expect(daemon.volume.volume_db).toBe(-13);
    daemon.setLevel({ volume_db: -40 });
    await until(() => s.getState().status?.volume?.volume_db === -40);
    // The drag's next step is refused: the level changed under it.
    s.setVolume({ volume_db: -14 }, "drag", drag);
    await s.whenIdle();
    await until(() => s.volume()?.volume_db === -40);
    // An unrelated mute click, a gesture of its own.
    expect(s.setVolume({ muted: true }, "commit", s.beginVolumeGesture())).toBe(true);
    await s.whenIdle();
    expect(daemon.volume).toMatchObject({ volume_db: -40, muted: true });
    // The old pointer moves on and is released: ignored.
    expect(s.setVolume({ volume_db: -15 }, "drag", drag)).toBe(false);
    expect(s.setVolume({ volume_db: -15 }, "commit", drag)).toBe(false);
    await s.whenIdle();
    expect(daemon.volume).toMatchObject({ volume_db: -40, muted: true });
    // A later gesture works.
    s.setVolume({ volume_db: -40.5 }, "commit", s.beginVolumeGesture());
    await s.whenIdle();
    expect(daemon.volume.volume_db).toBe(-40.5);
  });

  it("a press on the knob captures the gesture's base before the first move", async () => {
    const { s, daemon } = await setup();
    // Pointer-down where the knob is: the value does not change yet.
    const drag = s.beginVolumeGesture();
    expect(drag).not.toBeNull();
    daemon.setLevel({ volume_db: -40 });
    await until(() => s.volume()?.volume_db === -40);
    // The first move and the release, computed from -12 dB.
    s.setVolume({ volume_db: -12.5 }, "drag", drag);
    s.setVolume({ volume_db: -12.5 }, "commit", drag);
    await s.whenIdle();
    expect(daemon.volume.volume_db).toBe(-40);
    await until(() => s.volume()?.volume_db === -40);
  });

  it("a key step is bound to the render it was computed from", async () => {
    const { s, daemon } = await setup();
    const view = s.volumeView()!;
    expect(view.level.volume_db).toBe(-12);
    daemon.setLevel({ volume_db: -40 });
    await until(() => s.volume()?.volume_db === -40);
    // Arrow Up on the stale render: -12 + 0.5.
    s.setVolume({ volume_db: view.level.volume_db + 0.5 }, "commit", s.beginVolumeGesture(view));
    await s.whenIdle();
    expect(daemon.volume.volume_db).toBe(-40);
  });

  it("a release where the press began writes nothing", async () => {
    const { s, t } = await setup();
    const before = t.count("set_volume_guarded");
    const drag = s.beginVolumeGesture();
    s.setVolume({ volume_db: -12 }, "commit", drag);
    await s.whenIdle();
    expect(t.count("set_volume_guarded")).toBe(before);
  });

  it("a wheel burst never undoes a mute clicked during it", async () => {
    const { s, t, daemon } = await setup();
    // One wheel step down, written at once.
    const wheel = s.beginVolumeGesture();
    s.setVolume({ volume_db: -12.5 }, "drag", wheel);
    await s.whenIdle();
    expect(daemon.volume).toMatchObject({ volume_db: -12.5, muted: false });
    // Mute, clicked within the burst's pause: acknowledged.
    s.setVolume({ muted: true }, "commit", s.beginVolumeGesture());
    await s.whenIdle();
    expect(daemon.volume).toMatchObject({ volume_db: -12.5, muted: true });
    // The burst closes: its step is already written, so nothing goes out.
    const before = t.count("set_volume_guarded");
    s.setVolume({ volume_db: -12.5 }, "commit", wheel);
    await s.whenIdle();
    expect(t.count("set_volume_guarded")).toBe(before);
    expect(daemon.volume).toMatchObject({ volume_db: -12.5, muted: true });
    expect(s.volume()).toMatchObject({ volume_db: -12.5, muted: true });
    // A further step of a burst writes the volume only.
    const next = s.beginVolumeGesture();
    s.setVolume({ volume_db: -13 }, "drag", next);
    s.setVolume({ volume_db: -13 }, "commit", next);
    await s.whenIdle();
    expect(daemon.volume).toMatchObject({ volume_db: -13, muted: true });
    const last = t.calls.filter((c) => c.method === "set_volume_guarded").at(-1)!.params as Record<string, unknown>;
    expect(last).not.toHaveProperty("muted");
    expect(last).not.toHaveProperty("balance_db");
  });

  it("a refused pointer drag stays dead while a wheel burst on the same slider writes", async () => {
    const { s, daemon } = await setup();
    const pointer = s.beginVolumeGesture();
    daemon.setLevel({ volume_db: -40 });
    await until(() => s.getState().status?.volume?.volume_db === -40);
    // The pointer moves: refused.
    s.setVolume({ volume_db: -11 }, "drag", pointer);
    await s.whenIdle();
    await until(() => s.volume()?.volume_db === -40);
    // With the button held, a wheel step: its own gesture, from -40.
    const wheel = s.beginVolumeGesture();
    expect(s.setVolume({ volume_db: -40.5 }, "drag", wheel)).toBe(true);
    s.setVolume({ volume_db: -40.5 }, "commit", wheel);
    await s.whenIdle();
    expect(daemon.volume.volume_db).toBe(-40.5);
    // The old pointer moves up and is released: ignored.
    expect(s.setVolume({ volume_db: -8.4 }, "drag", pointer)).toBe(false);
    expect(s.setVolume({ volume_db: -8.4 }, "commit", pointer)).toBe(false);
    await s.whenIdle();
    expect(daemon.volume.volume_db).toBe(-40.5);
  });

  it("a released drag's final step goes out at the release, ahead of a later DSP toggle", async () => {
    const { s, t, daemon } = await setup();
    const drag = s.beginVolumeGesture();
    s.setVolume({ volume_db: -13 }, "drag", drag);
    // The next step lands within the throttle's interval: held back.
    s.setVolume({ volume_db: -14 }, "drag", drag);
    // Released there, then a slow DSP toggle right after.
    s.setVolume({ volume_db: -14 }, "commit", drag);
    let release!: () => void;
    t.hold.set("set_dsp_enabled", new Promise<void>((r) => (release = r)));
    s.setBypass(true);
    await until(() => t.count("set_dsp_enabled") > 0);
    await sleep(120);
    // The final step is not stuck behind the toggle.
    expect(daemon.volume.volume_db).toBe(-14);
    t.hold.delete("set_dsp_enabled");
    release();
    await s.whenIdle();
    const steps = t.calls.filter((c) => c.method === "set_volume_guarded").map((c) => (c.params as { volume_db?: number }).volume_db);
    expect(steps.filter((v) => v === -14)).toHaveLength(1);
    expect(daemon.volume.volume_db).toBe(-14);
  });

  it("a daemon that does not know guarded writes gets no unconditional one", async () => {
    const { s, t, daemon } = await setup();
    s.setVolume({ volume_db: -13 }, "drag");
    await s.whenIdle();
    // Silently replaced by an older engine (the native client reconnected
    // before the subscription reports it).
    daemon.legacyVolume = true;
    s.setVolume({ volume_db: -20 }, "drag");
    s.setVolume({ volume_db: -20 });
    await s.whenIdle();
    expect(daemon.volume.volume_db).toBe(-13);
    expect(t.count("set_volume")).toBe(0);
    // The re-read finds a status without revisions: controls stay off.
    await until(() => s.getState().status?.volume?.instance === undefined);
    expect(s.volume()).toBeNull();
    expect(s.setVolume({ volume_db: -30 })).toBe(false);
  });

  it("degrades when the engine lacks the phase-4a calls", async () => {
    const { s } = await setup({ refuse: ["list_presets", "list_devices"] });
    expect(s.getState().caps.presets).toBe(false);
    expect(s.getState().caps.devices).toBe(false);
    // Import still works on the EQ alone.
    await s.importApoPreset("Filter 1: ON PK Fc 500 Hz Gain -2 dB Q 2\n", "x.txt");
    await until(() => s.getState().config?.chain.eq?.bands.length === 1);
  });

  it("follows the device list the daemon pushes, and sets what an unplug does", async () => {
    const { s, daemon } = await setup();
    const plugged = { id: "hw:CARD=USB,DEV=0", card: "USB", name: "USB DAC", hdmi: false, loopback: false, active: false, channels: [] };
    const push = (daemon as unknown as { emit(e: DaemonEvent): void }).emit.bind(daemon);
    push({ event: "devices", data: [...s.getState().devices, plugged] });
    await until(() => s.getState().devices.some((d) => d.id === plugged.id));
    expect(s.getState().config?.device_lost).toBe("pause");
    s.setDeviceLost("keep_playing");
    await until(() => daemon.config.device_lost === "keep_playing");
    await until(() => s.getState().config?.device_lost === "keep_playing");
  });

  it("turns stages off and back on without losing their settings", async () => {
    const { s, daemon } = await setup();
    s.setUpsamplingEnabled(false);
    await until(() => daemon.config.chain.upsampler.target === "off");
    s.setUpsamplingEnabled(true);
    await until(() => daemon.config.chain.upsampler.target !== "off");
    expect(daemon.config.chain.upsampler.target).toEqual({ ratio: 4 });
    s.setIr(1, { enabled: false });
    await until(() => Array.isArray(daemon.config.chain.convolution) && daemon.config.chain.convolution[1].enabled === false);
    s.setConvEnabled(false);
    await until(() => Array.isArray(daemon.config.chain.convolution) && daemon.config.chain.convolution.every((c) => !c.enabled));
    s.setConvEnabled(true);
    await until(() => Array.isArray(daemon.config.chain.convolution) && daemon.config.chain.convolution[0].enabled);
  });

  it("switches between Preset and Custom and keeps each mode's last settings", async () => {
    const { s, t, daemon } = await setup();
    await s.setUpsampler({ design: { ...defaultDesign(), length: { rule: "fixed", taps: 32768 }, phase: { type: "minimum", method: "reflect" } } });
    await s.setUpsamplerMode("preset", "flattop_long");
    expect(daemon.config.chain.upsampler.preset).toBe("flattop_long");
    expect(daemon.config.chain.upsampler.design).toBeNull();
    await s.setUpsamplerPreset("gaussian_xlong");
    expect(s.getState().lastUpsPreset).toBe("gaussian_xlong");
    await s.setUpsamplerMode("custom");
    expect(daemon.config.chain.upsampler.preset).toBeNull();
    expect(daemon.config.chain.upsampler.design).toMatchObject({ length: { rule: "fixed", taps: 32768 }, phase: { type: "minimum" } });
    // Back to Preset: the last preset, not the view's fallback.
    await s.setUpsamplerMode("preset");
    expect(daemon.config.chain.upsampler.preset).toBe("gaussian_xlong");
    // Already in Preset mode: nothing is sent.
    const before = t.count("set_upsampler");
    await s.setUpsamplerMode("preset", "cosine");
    expect(t.count("set_upsampler")).toBe(before);
    expect(daemon.config.chain.upsampler.preset).toBe("gaussian_xlong");
    expect(t.count("set_config")).toBe(0);
  });

  it("applies Recommended in one set_upsampler, and Custom then keeps that design", async () => {
    const { s, t, daemon } = await setup();
    await s.setUpsamplerMode("preset", "flattop_long");
    const before = t.count("set_upsampler");
    expect(await s.setUpsamplerRecommended()).toBe(true);
    expect(t.count("set_upsampler")).toBe(before + 1);
    const sent = [...t.calls].reverse().find((c) => c.method === "set_upsampler")!.params as UpsamplerConfig;
    expect(sent).toEqual({
      target: { max_rate: 768000 },
      preset: "recommended",
      design: null,
    });
    expect(isRecommendedUpsampler(daemon.config.chain.upsampler)).toBe(true);
    expect(upsamplingSummary(s.getState().config!.chain.upsampler)).toMatch(/^Recommended/);
    // Custom plays the default custom design at once.
    const sends = t.count("set_upsampler");
    expect(await s.setUpsamplerMode("custom")).toBe(true);
    expect(t.count("set_upsampler")).toBe(sends + 1);
    expect(daemon.config.chain.upsampler.preset).toBeNull();
    expect(s.getState().upsDraft).toBeNull();
    expect(daemon.config.chain.upsampler.design).toMatchObject({
      window: { type: "kaiser", beta: 16 },
      length: { rule: "fixed", taps: 4096 },
      cutoff: { mode: "nyquist_stop" },
      phase: { type: "linear" },
    });
    expect(t.count("set_config")).toBe(0);
  });

  it("shows the daemon's refusal of an upsampler edit next to the controls, and clears it on the next good edit", async () => {
    const { s, daemon } = await setup();
    // 65,536 output-rate taps do not fit 44.1 → 48 kHz (160/147).
    expect(await s.setUpsampler({ target: { rate: 48000 } })).toBe(false);
    expect(s.getState().upsamplerError).toMatch(/design\.length: 65,536 taps is .* at 44\.1k ×160\/147.*or change the rate target\.$/);
    expect(refusalField(s.getState().upsamplerError!)?.path).toBe("design.length");
    expect(s.getState().toast).toBeNull();
    expect(daemon.config.chain.upsampler.target).toEqual({ ratio: 4 });
    // An integer-only preset at a fractional ratio: the engine names it.
    await s.setUpsampler({ design: { ...defaultDesign(), length: { rule: "per_input", taps: 4096 } } });
    expect(s.getState().upsamplerError).toBeNull();
    expect(await s.setUpsampler({ target: { rate: 48000 }, preset: "cascade_fir" })).toBe(false);
    expect(s.getState().upsamplerError).toContain("does not run at 160/147");
    expect(await s.setUpsampler({ target: { rate: 48000 } })).toBe(true);
    expect(s.getState().upsamplerError).toBeNull();
    await until(() => s.getState().status?.rate_ratio.down === 147);
    const st = s.getState().status!;
    expect(st).toMatchObject({ graph_rate: 44100, output_rate: 48000, ratio: 0, rate_ratio: { up: 160, down: 147 } });
    expect(st.passthrough.broken_because).toContainEqual({ reason: "rate_conversion", graph_rate: 44100, output_rate: 48000 });
  });

  it("reads an engine from before fractional rates (no rate_ratio) as integer ratios", async () => {
    const { s } = await setup({ legacyRatio: true });
    expect(s.getState().status?.rate_ratio).toEqual({ up: 4, down: 1 });
    expect(s.getState().caps.rationalRates).toBe(false);
    const r = await s.upsamplerResponse(null, 44100);
    expect(r.rate_ratio).toEqual({ up: 4, down: 1 });
    const off = await s.upsamplerResponse({ ...s.getState().config!.chain.upsampler, target: "off" }, 44100);
    expect(off.rate_ratio).toEqual({ up: 1, down: 1 });
  });

  it("knows a current engine runs fractional rates", async () => {
    const { s } = await setup();
    expect(s.getState().caps.rationalRates).toBe(true);
  });

  it("keeps a custom design the rate setting refuses as a draft, the preset playing, and sends the fixed design", async () => {
    const { s, t, daemon } = await setup();
    await s.setUpsamplerMode("preset", "gaussian_long");
    expect(await s.setUpsampler({ target: { rate: 48000 } })).toBe(true);
    // The saved custom design (65,536 output taps) does not fit 160/147.
    const before = t.count("set_upsampler");
    expect(await s.setUpsamplerMode("custom")).toBe(false);
    expect(t.count("set_upsampler")).toBe(before);
    expect(daemon.config.chain.upsampler.preset).toBe("gaussian_long");
    expect(s.getState().upsDraft).toMatchObject({ preset: null, design: { length: { rule: "fixed", taps: 65536 } } });
    // The fixed design is sent once, on the current rate setting.
    expect(await s.editCustomDesign({ ...defaultDesign(), length: { rule: "per_input", taps: 4096 } })).toBe(true);
    expect(t.count("set_upsampler")).toBe(before + 1);
    expect(daemon.config.chain.upsampler).toMatchObject({ preset: null, design: { length: { rule: "per_input", taps: 4096 } }, target: { rate: 48000 } });
    expect(s.getState().upsDraft).toBeNull();
    expect(s.getState().upsamplerError).toBeNull();
  });

  it("refuses a rate policy that the custom design does not fit (set_rate validates like set_config)", async () => {
    const { daemon } = await setup();
    daemon.handle("set_ab", null);
    const rate = { ...daemon.config.rate, allowed_rates: [48000, 96000], default_rate: 48000 };
    daemon.handle("set_rate", rate);
    // Without A/B, 8,000 taps per input sample fit 96 -> 48 kHz (1/2), not
    // 44.1 -> 48 kHz (160/147: 1,280,000 grid taps, over 1,048,576).
    daemon.handle("set_upsampler", patchUpsamplerConfig(daemon.config.chain.upsampler, { preset: null, target: { rate: 48000 }, taps: 8000, taps_unit: "input", cutoff_mode: "fraction", auto_taps: null, phase: "linear" }));
    expect(() => daemon.handle("set_rate", { ...rate, allowed_rates: [44100, 48000, 96000] })).toThrow(/at 44100 Hz/);
    expect(daemon.config.rate.allowed_rates).toEqual([48000, 96000]);
  });

  it("refuses a rate policy that a band design does not fit on the fractional grid", async () => {
    const { daemon } = await setup();
    const rate = { ...daemon.config.rate, allowed_rates: [48000], default_rate: 48000 };
    daemon.handle("set_rate", rate);
    // From 48 kHz, 384 kHz is x8; from 44.1 kHz it is 1280/147, where a
    // 0.99..1.00 band at 200 dB needs far more than 1,048,576 grid taps.
    daemon.handle(
      "set_upsampler",
      patchUpsamplerConfig(daemon.config.chain.upsampler, {
        preset: null,
        target: { rate: 384000 },
        cutoff_mode: "band",
        auto_taps: null,
        phase: "linear",
        window: "kaiser",
        band: { passband_end: 0.99, stopband_start: 1.0, attenuation_db: 200 },
      }),
    );
    expect(() => daemon.handle("set_rate", { ...rate, allowed_rates: [44100, 48000] })).toThrow(/at 44100 Hz/);
    expect(daemon.config.rate.allowed_rates).toEqual([48000]);
  });

  it("edits a preset as custom: its exact values with based_on play at once", async () => {
    const { s, t, daemon } = await setup();
    await s.setUpsamplerMode("preset", "flattop_long");
    const before = t.count("set_upsampler");
    expect(await s.editAsCustom("flattop_long")).toBe(true);
    expect(t.count("set_upsampler")).toBe(before + 1);
    expect(s.getState().upsDraft).toBeNull();
    const draft = daemon.config.chain.upsampler.design!;
    expect(daemon.config.chain.upsampler.preset).toBeNull();
    expect(draft).toMatchObject({ based_on: "flattop_long", kind: "windowed_sinc", window: { type: "flattop", set: "hft248d" }, length: { rule: "per_input", taps: 511 }, numerics: { path: "fir" } });
    expect(sameDesign(draft, s.getState().designBases.flattop_long)).toBe(true);
    // A valid edit plays at once.
    expect(await s.editCustomDesign({ ...(draft as Extract<typeof draft, { kind: "windowed_sinc" }>), length: { rule: "per_input", taps: 255 } })).toBe(true);
    expect(t.count("set_upsampler")).toBe(before + 2);
    expect(daemon.config.chain.upsampler).toMatchObject({ preset: null, design: { based_on: "flattop_long", length: { taps: 255 } } });
    // Revert to preset: the name again.
    expect(await s.useBasePreset("flattop_long")).toBe(true);
    expect(daemon.config.chain.upsampler).toMatchObject({ preset: "flattop_long", design: null });
  });

  it("sends a custom edit that passes the check at once, and keeps one that does not as a draft", async () => {
    const { s, t, daemon } = await setup();
    const design = defaultDesign();
    expect(await s.editCustomDesign({ ...design, length: { rule: "fixed", taps: 32768 } })).toBe(true);
    expect(daemon.config.chain.upsampler.design).toMatchObject({ length: { taps: 32768 } });
    expect(s.getState().upsDraft).toBeNull();
    // beta 41 is out of bounds: checked, not sent, kept with its field error.
    const before = t.count("set_upsampler");
    const bad = { ...design, window: { type: "kaiser" as const, beta: 41, sampling: "symmetric" as const } };
    expect(await s.editCustomDesign(bad)).toBe(false);
    expect(t.count("set_upsampler")).toBe(before);
    expect(s.getState().upsDraft?.design).toEqual(bad);
    const check = await s.checkDesign(bad);
    expect(check.errors).toEqual([{ path: "design.window.beta", message: "must be between 0 and 40 (got 41)" }]);
    // A config event for something else keeps the draft.
    s.setUpsamplingEnabled(true);
    await until(() => t.count("set_upsampler") === before + 1);
    expect(s.getState().upsDraft?.design).toEqual(bad);
    s.discardUpsDraft();
    expect(s.getState().upsDraft).toBeNull();
    expect(daemon.config.chain.upsampler.design).toMatchObject({ length: { taps: 32768 } });
  });

  it("shows a long build as building and folds the edits made meanwhile into one", async () => {
    const { s, t, daemon } = await setup({ latency: { build: 150 } });
    const before = t.count("set_upsampler");
    const first = s.setUpsampler({ target: { ratio: 2 } });
    await until(() => s.getState().upsBuilding !== null);
    expect(s.getState().upsBuilding?.waiting).toBe(false);
    // Three edits while the first builds: one more build, the last edit's.
    const later = [s.setUpsampler({ target: { ratio: 4 } }), s.setUpsampler({ target: { ratio: 8 } }), s.setUpsampler({ target: { ratio: 16 } })];
    expect(await first).toBe(true);
    expect(await Promise.all(later)).toEqual([true, true, true]);
    expect(t.count("set_upsampler") - before).toBe(2);
    expect(daemon.config.chain.upsampler.target).toEqual({ ratio: 16 });
    expect(s.getState().upsBuilding).toBeNull();
    expect(s.getState().upsBuilds).toBeGreaterThanOrEqual(2);
  });

  it("waits out a build the app stopped waiting for without sending it again", async () => {
    // The engine takes 200 ms; the app gives up after 80 and asks how it went.
    const { s, t, daemon } = await setup({ latency: { build: 200, buildTimeout: 80 } });
    const before = t.count("set_upsampler");
    const done = s.setUpsampler({ target: { ratio: 8 } });
    await until(() => s.getState().upsBuilding?.waiting === true);
    expect(s.getState().upsamplerError).toBeNull();
    expect(await done).toBe(true);
    expect(daemon.config.chain.upsampler.target).toEqual({ ratio: 8 });
    expect(s.getState().upsamplerError).toBeNull();
    expect(s.getState().upsBuilding).toBeNull();
    expect(t.count("set_upsampler") - before).toBe(1);
  });

  it("never undoes a newer change by another client after a timeout", async () => {
    const { s, t, daemon } = await setup({ latency: { build: 150, buildTimeout: 50 } });
    const before = t.count("set_upsampler");
    const done = s.setUpsampler({ target: { ratio: 8 } });
    // Another client sets x16 once x8 has landed.
    await until(() => daemon.config.chain.upsampler.target !== "off" && JSON.stringify(daemon.config.chain.upsampler.target) === JSON.stringify({ ratio: 8 }));
    await daemon.handle("set_upsampler", { ...daemon.config.chain.upsampler, target: { ratio: 16 } });
    await done;
    await sleep(300);
    expect(daemon.config.chain.upsampler.target).toEqual({ ratio: 16 });
    expect(t.count("set_upsampler") - before).toBe(1);
    expect(s.getState().upsamplerError).toBeNull();
  });

  it("clears its timeout once the filter it stopped waiting for lands", async () => {
    // Every wait (the call, then three reads behind it) gives up before the build is done.
    const { s, t, daemon } = await setup({ latency: { build: 400, buildTimeout: 40 } });
    const before = t.count("set_upsampler");
    expect(await s.setUpsampler({ target: { ratio: 8 } })).toBe(false);
    expect(s.getState().upsamplerError).toContain("has not finished");
    await until(() => JSON.stringify(daemon.config.chain.upsampler.target) === JSON.stringify({ ratio: 8 }));
    await until(() => s.getState().upsamplerError === null);
    expect(t.count("set_upsampler") - before).toBe(1);
  });

  it("learns that a build it stopped waiting for was refused, without sending it again", async () => {
    const { s, t } = await setup({ latency: { build: 150, buildTimeout: 50 } });
    const before = t.count("set_upsampler");
    expect(await s.setUpsampler({ target: { rate: 48000 }, preset: "cascade_fir" })).toBe(false);
    expect(s.getState().upsamplerError).toContain("did not apply the filter");
    expect(t.count("set_upsampler") - before).toBe(1);
  });

  it("plays the preset picked while a custom design builds", async () => {
    const { s, daemon } = await setup({ latency: { build: 150 } });
    await s.setUpsamplerMode("preset", "gaussian_long");
    expect(daemon.config.chain.upsampler.preset).toBe("gaussian_long");
    const custom = s.editCustomDesign({ ...defaultDesign(), length: { rule: "fixed", taps: 32768 } });
    await until(() => s.getState().upsBuilding !== null);
    // The preset still plays: the pick must not stop at that.
    expect(await s.setUpsamplerMode("preset", "gaussian_long")).toBe(true);
    await custom;
    await s.whenIdle();
    expect(daemon.config.chain.upsampler).toMatchObject({ preset: "gaussian_long", design: null });
    expect(s.getState().upsDraft).toBeNull();
  });

  it("keeps a newer draft when an older design's build lands", async () => {
    const { s, t, daemon } = await setup({ latency: { build: 150 } });
    const design = defaultDesign();
    const before = t.count("set_upsampler");
    const first = s.editCustomDesign({ ...design, length: { rule: "fixed", taps: 32768 } });
    await until(() => t.count("set_upsampler") === before + 1);
    const bad = { ...design, window: { type: "kaiser" as const, beta: 41, sampling: "symmetric" as const } };
    expect(await s.editCustomDesign(bad)).toBe(false);
    await first;
    await until(() => daemon.config.chain.upsampler.design !== null && (daemon.config.chain.upsampler.design as { length?: { taps?: number } }).length?.taps === 32768);
    await sleep(50);
    expect(s.getState().upsDraft?.design).toEqual(bad);
  });

  it("says why when a long build is refused", async () => {
    const { s } = await setup({ latency: { build: 120 } });
    expect(await s.setUpsampler({ target: { rate: 48000 }, preset: "cascade_fir" })).toBe(false);
    expect(s.getState().upsamplerError).toContain("does not run at 160/147");
    expect(s.getState().upsBuilding).toBeNull();
  });

  it("drops the draft when the user goes back to the preset", async () => {
    const { s, daemon } = await setup();
    await s.setUpsamplerMode("preset", "gaussian_long");
    await s.setUpsampler({ target: { rate: 48000 } });
    await s.setUpsamplerMode("custom");
    expect(s.getState().upsDraft).not.toBeNull();
    expect(await s.setUpsamplerMode("preset")).toBe(true);
    expect(s.getState().upsDraft).toBeNull();
    expect(daemon.config.chain.upsampler.preset).toBe("gaussian_long");
  });

  it("turns upsampling back on in the integer form on an engine without fractional rates", async () => {
    const first = await setup();
    await first.s.setUpsampler({ target: { max_rate_any: 384000 } });
    first.s.setUpsamplingEnabled(false);
    await until(() => first.daemon.config.chain.upsampler.target === "off");
    first.s.disconnect();
    // The same browser, now with an older engine.
    const { s, daemon } = await setup({ legacyRatio: true });
    await s.setUpsampler({ target: "off" });
    s.setUpsamplingEnabled(true);
    await until(() => daemon.config.chain.upsampler.target !== "off");
    expect(daemon.config.chain.upsampler.target).toEqual({ max_rate: 384000 });
    expect(s.getState().upsamplerError).toBeNull();
  });

  it("remembers the last target of each rate kind", async () => {
    const { s } = await setup();
    expect(s.getState().lastUpsTargets.ratio).toEqual({ ratio: 4 });
    await s.setUpsampler({ target: { max_rate_any: 384000 } });
    await s.setUpsampler({ target: { ratio: 8 } });
    expect(s.lastUpsamplerTarget("max")).toEqual({ max_rate_any: 384000 });
    expect(s.lastUpsamplerTarget("ratio")).toEqual({ ratio: 8 });
    expect(s.lastUpsamplerTarget("rate")).toBeUndefined();
  });

  it("routes a stream and remembers the app in Selected-apps mode", async () => {
    const { s, daemon } = await setup();
    await s.setRoutingMode({ mode: "apps", rules: [] });
    const ff = s.getState().streams.find((x) => x.app_name === "Firefox")!;
    await s.routeStream(ff, "direct");
    expect(daemon.config.routing).toEqual({ mode: "apps", rules: [{ app: "Firefox", sink: "direct" }] });
    expect(s.getState().streams.find((x) => x.app_name === "Firefox")?.sink).toBe("direct");
  });

  it("sets the mixed-rate policy and the mock follows it", async () => {
    const { s, daemon } = await setup();
    // Strawberry (44.1k) plays into the DSP sink; Firefox (48k) joins.
    const ff = s.getState().streams.find((x) => x.app_name === "Firefox")!;
    await s.routeStream(ff, "dsp");
    await until(() => s.getState().status?.graph_rate === 44100);
    s.setRateConflict("highest");
    await until(() => daemon.config.rate.conflict === "highest");
    await until(() => s.getState().status?.graph_rate === 48000);
    s.setRateConflict("keep_current");
    await until(() => daemon.config.rate.conflict === "keep_current");
    // Both still play: the current rate (48k) is kept.
    await sleep(50);
    expect(s.getState().status?.graph_rate).toBe(48000);
  });

  it("shows the engine-down state and starts the engine", async () => {
    const daemon = new MockDaemon({ running: false });
    const s = new AppStore(new MockTransport(daemon));
    stores.push(s);
    s.connect();
    await until(() => s.getState().conn === "down");
    await s.startEngine();
    await until(() => s.getState().conn === "up" && !!s.getState().config);
  });

  it("A/B: chooses side B from a preset and switches", async () => {
    const { s, daemon } = await setup();
    await s.setAbSource({ preset: "Room + bass shelf" });
    expect(daemon.config.ab?.chain_b?.eq?.name).toBe("Room + bass shelf");
    expect(daemon.config.ab?.label).toBe("Room + bass shelf");
    expect("chain" in daemon.config.ab!.b).toBe(true);
    await s.abSelect("b");
    expect(s.getState().status?.ab?.current).toBe("b");
    await s.setLevelMatch({ mode: "manual", offset_db: -1.5 });
    expect(s.getState().status?.ab?.offset_db).toBe(-1.5);
  });

  describe("concurrent edits", () => {
    // Slow replies and late events: the first edit's refresh has not come back
    // when the next edit is made, as with a busy daemon.
    const slow: MockLatency = { request: 2, reply: 25, event: 25 };

    it("dither type right after dither depth keeps the new depth", async () => {
      const { s, daemon } = await setup({ latency: slow });
      expect(daemon.config.chain.output).toEqual({ bits: 24, dither: "tpdf", headroom_db: 3 });
      s.setOutput({ bits: 16 });
      // The daemon has applied it; its reply and config event are still on the way.
      await until(() => daemon.config.chain.output.bits === 16, 2000);
      expect(s.getState().config?.chain.output.bits).toBe(24);
      s.setOutput({ dither: "shaped" });
      await s.whenIdle();
      expect(daemon.config.chain.output).toEqual({ bits: 16, dither: "shaped", headroom_db: 3 });
      expect(s.getState().config?.chain.output).toEqual({ bits: 16, dither: "shaped", headroom_db: 3 });
    });

    it("a config edit does not write back the volume set just before it", async () => {
      const { s, daemon } = await setup({ latency: slow });
      expect(daemon.volume.volume_db).toBe(-12);
      s.setVolume({ volume_db: -5 });
      s.setOutput({ dither: "none" });
      await s.whenIdle();
      expect(daemon.volume.volume_db).toBe(-5);
      expect(daemon.config.volume?.volume_db).toBe(-5);
      expect(daemon.config.chain.output.dither).toBe("none");
      expect(s.volume()?.volume_db).toBe(-5);
    });

    it("a config edit does not write back the EQ set just before it", async () => {
      const { s, t, daemon } = await setup({ latency: slow });
      s.updateBand(0, { gain_db: 2 });
      s.setPreferredFormat("s24");
      await s.whenIdle();
      expect(daemon.config.chain.eq?.bands[0].gain_db).toBe(2);
      expect(daemon.config.formats[0]).toBe("s24");
      // The EQ and the formats went out on their own calls; no whole-config write.
      expect(t.calls.find((c) => c.method === "set_formats")!.params).toEqual(["s24", ...daemon.config.formats.slice(1)]);
      expect(t.calls.some((c) => c.method === "set_config")).toBe(false);
    });

    it("an EQ edit made while a preset loads applies to the loaded preset", async () => {
      const { s, daemon } = await setup({ latency: slow });
      void s.loadPreset("Room + bass shelf");
      s.updateBand(0, { gain_db: 1 });
      await s.whenIdle();
      expect(daemon.config.chain.eq?.name).toBe("Room + bass shelf");
      expect(daemon.config.chain.eq?.bands).toHaveLength(4);
      expect(daemon.config.chain.eq?.bands[0]).toMatchObject({ kind: "low_shelf", freq_hz: 80, gain_db: 1 });
      expect(s.getState().eqDraft).toBeNull();
    });

    it("IR edits in a row each apply to the chain the previous one left", async () => {
      const { s, daemon } = await setup({ latency: slow });
      s.setIr(0, { gain_db: -3 });
      s.setIr(1, { enabled: false });
      s.removeIr(0);
      await s.whenIdle();
      expect(daemon.config.chain.convolution).toHaveLength(1);
      expect(daemon.config.chain.convolution[0]).toMatchObject({ gain_db: -1, enabled: false });
    });

    it("a drag against a slow daemon queues at most one send and ends on the final value", async () => {
      const { s, t, daemon } = await setup({ latency: { request: 2, reply: 40, event: 40 } });
      const before = t.count("set_eq");
      s.beginEqDrag();
      for (let i = 0; i < 40; i++) {
        s.updateBand(1, { freq_hz: 60 + i }, "drag");
        await sleep(5);
      }
      s.updateBand(1, { freq_hz: 500 }, "drag");
      s.endEqDrag();
      await s.whenIdle();
      expect(daemon.config.chain.eq?.bands[1].freq_hz).toBe(500);
      expect(s.getState().eqDraft).toBeNull();
      expect(s.getState().config?.chain.eq?.bands[1].freq_hz).toBe(500);
      // ~200 ms of dragging, each send ~90 ms round trip (get_config + set_eq): a handful, not 41.
      expect(t.count("set_eq") - before).toBeLessThanOrEqual(6);
    });

    it("volume drag then mute: the mute keeps the dragged volume", async () => {
      const { s, daemon } = await setup({ latency: slow });
      for (let i = 0; i < 10; i++) {
        s.setVolume({ volume_db: -20 + i }, "drag");
        await sleep(3);
      }
      s.setVolume({ volume_db: -8 });
      s.setVolume({ muted: true });
      await s.whenIdle();
      expect(daemon.volume).toMatchObject({ volume_db: -8, muted: true });
      expect(s.getState().volumeDraft).toBeNull();
    });
  });

  it("subscribes to spectrum only while EQ view is active and enabled", async () => {
    spectrumStore.clear();
    const { s } = await setup();
    expect(s.getState().view).toBe("eq");
    expect(s.getState().spectrum.enabled).toBe(true);
    await until(() => spectrumStore.latest !== null);
    expect(spectrumStore.latest?.spectrum.pre.length).toBeGreaterThan(200);
    // Each tap runs to its own Nyquist (or 100 kHz): the upsampled post one is longer.
    expect(spectrumStore.latest?.spectrum.post.length).toBeGreaterThan(spectrumStore.latest?.spectrum.pre.length ?? 0);
    expect(Object.keys(s.getState())).not.toContain("spectrumData");

    s.setView("ups");
    expect(spectrumStore.latest).toBeNull();
    await sleep(90);
    expect(spectrumStore.latest).toBeNull();

    s.setView("eq");
    await until(() => spectrumStore.latest !== null);
    s.setSpectrumPrefs({ enabled: false });
    expect(spectrumStore.latest).toBeNull();
  });

  it("fetches upsamplerResponse for active filter and flat 1:1 bypass", async () => {
    const { s } = await setup();
    const active = await s.upsamplerResponse(null, 44100);
    expect(active.source_rate).toBe(44100);
    expect(active.output_rate).toBe(176400);
    expect(active.ratio).toBe(4);
    expect(active.impulse).toHaveLength(256);
    expect(active.stopband_atten_db).toBeGreaterThan(100);
    expect(active.latency_samples).toBeGreaterThan(0);

    expect(active.effective_cutoff).toBe(1);

    const nyqStop = await s.upsamplerResponse(
      patchUpsamplerConfig(MANUAL, { target: { ratio: 16 }, taps: 1048576, phase: "linear", kaiser_beta: 14, cutoff: 1, cutoff_mode: "nyquist_stop" }),
      48000,
    );
    expect(nyqStop.effective_cutoff).toBeLessThan(1);
    expect(nyqStop.effective_cutoff).toBeGreaterThan(0.999);

    const flat = await s.upsamplerResponse(
      patchUpsamplerConfig(MANUAL, { target: "off", taps: 65536, phase: "linear", kaiser_beta: 14, cutoff: 1, cutoff_mode: "fraction" }),
      48000,
    );
    expect(flat.ratio).toBe(1);
    expect(flat.effective_cutoff).toBe(1);
    expect(flat.output_rate).toBe(48000);
    expect(flat.impulse).toEqual([1]);
    expect(flat.magnitude_db.every((db) => db === 0)).toBe(true);
  });

  it("fetches a zoom window with crossings and rejects invalid windows", async () => {
    const { s } = await setup();
    const zoomed = await s.upsamplerResponse(null, 48000, { f_lo_hz: 23990, f_hi_hz: 24010, points: 9000 });
    expect(zoomed.window?.freqs_hz).toHaveLength(4096);
    expect(zoomed.window?.freqs_hz[0]).toBe(23990);
    expect(zoomed.window?.freqs_hz[4095]).toBe(24010);
    expect(zoomed.crossings.length).toBeGreaterThan(5);
    expect(zoomed.crossings.map((c) => c.freq_hz)).toEqual([...zoomed.crossings.map((c) => c.freq_hz)].sort((a, b) => a - b));
    expect(zoomed.minus_3_db_hz).toBe(zoomed.crossings.find((c) => c.level_db === -3)?.freq_hz);
    await expect(s.upsamplerResponse(null, 48000, { f_lo_hz: 100, f_hi_hz: 50, points: 10 })).rejects.toMatchObject({ code: ErrorCode.INVALID_PARAMS });
    await expect(s.upsamplerResponse(null, 48000, { f_lo_hz: 0, f_hi_hz: 1e9, points: 10 })).rejects.toMatchObject({ code: ErrorCode.INVALID_PARAMS });
  });

  it("floors auto-preamp cleanly so peak EQ response never exceeds 0 dB", async () => {
    const { s } = await setup();
    s.setManualPreamp(0);
    await s.whenIdle();
    const eqBoosted = s.eq()!;
    expect(profilePeakGainDb(eqBoosted, s.graphRate())).toBeGreaterThan(1);

    s.setAutoPreamp(true);
    await s.whenIdle();
    const eqFixed = s.eq()!;
    expect(profilePeakGainDb(eqFixed, s.graphRate())).toBeLessThanOrEqual(0);
  });

  it("supersedes older analysis calls and keeps control calls responsive during slow measurement", async () => {
    const { s, daemon } = await setup({ latency: { request: 2, analysis: 120 } });
    const firstErr = s.upsamplerResponse(null, 44100).catch((e) => e);
    const second = s.upsamplerResponse(null, 96000);

    // Control call finishes quickly while the 120 ms analysis measurement is still running.
    const t0 = Date.now();
    s.setVolume({ volume_db: -18 });
    await s.whenIdle();
    expect(Date.now() - t0).toBeLessThan(80);
    expect(daemon.volume.volume_db).toBe(-18);

    expect(await firstErr).toMatchObject({ code: ErrorCode.SUPERSEDED });
    const res = await second;
    expect(res.source_rate).toBe(96000);

    const timedOut = new MockTransport(daemon, { analysis: 5, analysisTimeout: true });
    await expect(timedOut.upsamplerResponse(null, 44100)).rejects.toMatchObject({ code: ErrorCode.TIMEOUT });
  });

  it("clears spectrum on device release and sample rate change, and resets clip baseline on reconnect", async () => {
    spectrumStore.clear();
    const { s, daemon } = await setup();
    await until(() => spectrumStore.latest !== null && meterStore.latest !== null);

    // Release device clears spectrum immediately.
    await s.releaseDevice();
    expect(spectrumStore.latest).toBeNull();

    // Acquire again, wait for spectrum frame, then switch rate via stream + config.
    await s.acquireDevice();
    await until(() => spectrumStore.latest !== null);
    spectrumStore.push(
      { f_min_hz: 20, bins_per_octave: 24, pre: [-20, -20], post: [-20, -20], rate: 44100 },
      100,
    );
    expect(spectrumStore.latest).not.toBeNull();
    let rateClearCount = 0;
    const offClear = spectrumStore.onClear(() => rateClearCount++);
    daemon.streams[0].rate = 96000;
    daemon.snapshot(["status"]);
    await until(() => s.getState().status?.graph_rate === 96000);
    offClear();
    expect(rateClearCount).toBeGreaterThanOrEqual(1);

    // Reconnect clears clip baselines so fresh clips in a new session are not masked.
    meterStore.push(
      { peak_db: [-6, -6], rms_db: [-16, -16], clipped: 10, clipped_ch: [10, 0], input_peak_db: [-6, -6], chain_peak_db: [-6, -6], xruns: 0, dsp_load: 0.01 },
      500,
    );
    meterStore.resetClip();
    expect(meterStore.clipBaseline).toBe(10);
    s.disconnect();
    expect(meterStore.clipBaseline).toBeNull();
    expect(meterStore.clipsSinceReset()).toBe(0);
  });
});
