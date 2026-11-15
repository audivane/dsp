import { afterEach, describe, expect, it } from "vitest";
import { MockDaemon, MockTransport, type MockLatency } from "../api/mock";
import { DaemonError, ErrorCode } from "../api/transport";
import { profilePeakGainDb } from "../lib/eqmath";
import { AppStore } from "./store";
import { meterStore } from "./meters";
import { spectrumStore } from "./spectrum";

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
  override async call<T>(method: string, params?: unknown): Promise<T> {
    this.calls.push({ method, params });
    if (this.refuse.has(method)) throw new DaemonError(ErrorCode.INVALID_REQUEST, `unknown method "${method}"`);
    return super.call<T>(method, params);
  }
  count(method: string) {
    return this.calls.filter((c) => c.method === method).length;
  }
}

let stores: AppStore[] = [];
async function setup(opts: { refuse?: string[]; latency?: MockLatency } = {}) {
  const daemon = new MockDaemon();
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

  it("imports a ParametricEQ.txt as a preset and loads it", async () => {
    const { s, t } = await setup();
    await s.importApoPreset("Preamp: -4 dB\nFilter 1: ON PK Fc 1000 Hz Gain 3 dB Q 1\n", "My HP ParametricEQ.txt");
    expect(t.calls.some((c) => c.method === "import_apo" && (c.params as { name: string }).name === "My HP")).toBe(true);
    await until(() => s.activePreset() === "My HP");
    expect(s.getState().config?.chain.eq?.bands).toHaveLength(1);
    // An imported preset is EQ only: loading it turns convolution and upsampling off, and says so.
    expect(s.getState().config?.chain.convolution).toEqual([]);
    expect(s.getState().config?.chain.upsampler.target).toBe("off");
    expect(s.getState().toast?.text).toMatch(/EQ only: convolution and upsampling off/);
    expect(s.getState().presets.find((p) => p.name === "My HP")).toMatchObject({ bands: 1, convolution: 0 });
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
    const before = t.count("set_volume");
    for (let i = 0; i < 20; i++) {
      s.setVolume({ volume_db: -20 + i * 0.5 }, "drag");
      await sleep(2);
    }
    s.setVolume({ volume_db: -5 });
    await until(() => s.getState().volumeDraft === null);
    expect(t.count("set_volume") - before).toBeLessThan(8);
    expect(daemon.volume.volume_db).toBe(-5);
    s.setVolume({ muted: true, balance_db: -2 });
    await until(() => daemon.volume.muted && daemon.volume.balance_db === -2);
  });

  it("degrades when the engine lacks the phase-4a calls", async () => {
    const { s } = await setup({ refuse: ["list_presets", "list_devices"] });
    expect(s.getState().caps.presets).toBe(false);
    expect(s.getState().caps.devices).toBe(false);
    // Import still works on the EQ alone.
    await s.importApoPreset("Filter 1: ON PK Fc 500 Hz Gain -2 dB Q 2\n", "x.txt");
    await until(() => s.getState().config?.chain.eq?.bands.length === 1);
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
    expect(daemon.config.ab?.chain_b.eq?.name).toBe("Room + bass shelf");
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
      expect(daemon.config.chain.output).toEqual({ bits: 24, dither: "tpdf" });
      s.setOutput({ bits: 16 });
      // The daemon has applied it; its reply and config event are still on the way.
      await until(() => daemon.config.chain.output.bits === 16, 2000);
      expect(s.getState().config?.chain.output.bits).toBe(24);
      s.setOutput({ dither: "shaped" });
      await s.whenIdle();
      expect(daemon.config.chain.output).toEqual({ bits: 16, dither: "shaped" });
      expect(s.getState().config?.chain.output).toEqual({ bits: 16, dither: "shaped" });
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
      // The EQ went out on its own call; set_config carried the EQ the daemon already had.
      const setConfig = t.calls.find((c) => c.method === "set_config")!.params as { chain: { eq: { bands: { gain_db: number }[] } } };
      expect(setConfig.chain.eq.bands[0].gain_db).toBe(2);
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
    expect(spectrumStore.latest?.spectrum.post.length).toBe(spectrumStore.latest?.spectrum.pre.length);
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
      { target: { ratio: 16 }, taps: 1048576, phase: "linear", kaiser_beta: 14, cutoff: 1, cutoff_mode: "nyquist_stop" },
      48000,
    );
    expect(nyqStop.effective_cutoff).toBeLessThan(1);
    expect(nyqStop.effective_cutoff).toBeGreaterThan(0.999);

    const flat = await s.upsamplerResponse({ target: "off", taps: 65536, phase: "linear", kaiser_beta: 14, cutoff: 1, cutoff_mode: "fraction" }, 48000);
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
