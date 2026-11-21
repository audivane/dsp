// The effect stages' setters (targeted calls with a draft and the daemon's
// echo), the volume settings (get_config + set_config, never the level) and
// the EQ tilt, against the mock daemon.

import { afterEach, describe, expect, it } from "vitest";
import { MockDaemon, MockTransport } from "../api/mock";
import { DaemonError, ErrorCode } from "../api/transport";
import { AppStore, clearStash } from "./store";
import { meterStore } from "./meters";
import { crossfeedAsRun, crossfeedLevel, defaultCrossfeed, withCrossfeedLevel } from "../lib/effects";

const until = async (pred: () => boolean, ms = 3000) => {
  const t = Date.now();
  while (!pred()) {
    if (Date.now() - t > ms) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 5));
  }
};
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

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
  last(method: string) {
    return [...this.calls].reverse().find((c) => c.method === method)?.params;
  }
}

let stores: AppStore[] = [];
async function setup(opts: { refuse?: string[]; digital?: boolean; legacyCrossfeedLevel?: boolean } = {}) {
  const daemon = new MockDaemon({ digitalVolume: opts.digital, legacyCrossfeedLevel: opts.legacyCrossfeedLevel });
  const t = new SpyTransport(daemon, { request: 1, reply: 1, event: 1 });
  for (const m of opts.refuse ?? []) t.refuse.add(m);
  const s = new AppStore(t);
  stores.push(s);
  s.connect();
  await until(() => s.getState().conn === "up" && !!s.getState().config && !!s.getState().status);
  return { s, t, daemon };
}
const idle = (s: AppStore) => s.whenIdle();
const meterStoreClipped = () => meterStore.latest?.meters.clipped ?? null;

afterEach(() => {
  for (const s of stores) s.disconnect();
  stores = [];
  clearStash();
});

describe("effect stages", () => {
  it("DC block: draft at once, targeted call, echo replaces the draft", async () => {
    const { s, t, daemon } = await setup();
    s.setStageEnabled("dc_block", true);
    expect(s.stage("dc_block")).toEqual({ enabled: true, cutoff_hz: 5 });
    expect(s.getState().stageDrafts.dc_block).toBeDefined();
    await idle(s);
    expect(t.last("set_dc_block")).toEqual({ enabled: true, cutoff_hz: 5 });
    expect(t.count("set_config")).toBe(0);
    expect(daemon.config.chain.dc_block).toEqual({ enabled: true, cutoff_hz: 5 });
    expect(s.getState().stageDrafts.dc_block).toBeUndefined();
    // Off keeps the cutoff for the next time.
    s.editStage("dc_block", (c) => ({ enabled: true, cutoff_hz: c?.cutoff_hz === 5 ? 8 : 5 }));
    s.setStageEnabled("dc_block", false);
    await idle(s);
    expect(daemon.config.chain.dc_block).toEqual({ enabled: false, cutoff_hz: 8 });
    expect(s.getState().stageCaps.dc_block).toBe(true);
  });

  it("a drag throttles, keeps the draft and always sends the final value", async () => {
    const { s, t, daemon } = await setup();
    const before = t.count("set_limiter");
    for (let i = 0; i < 30; i++) {
      s.editStage("limiter", (c) => ({ ...c, ceiling_dbfs: -6 + i * 0.1 }), "drag");
      await sleep(4);
    }
    expect(s.stage("limiter")?.ceiling_dbfs).toBeCloseTo(-3.1, 6);
    s.editStage("limiter", (c) => ({ ...c, ceiling_dbfs: -1 }), "commit");
    await idle(s);
    await until(() => s.getState().stageDrafts.limiter === undefined);
    const sent = t.count("set_limiter") - before;
    expect(sent).toBeLessThan(15);
    expect(sent).toBeGreaterThanOrEqual(2);
    expect(daemon.config.chain.limiter.ceiling_dbfs).toBe(-1);
  });

  it("shows the daemon's refusal inline and drops the draft", async () => {
    const { s, daemon } = await setup();
    s.editStage("crossfeed", () => ({ enabled: true, mode: "bs2b_custom", cutoff_hz: 5000, feed_db: 4.5, level: "matched" }));
    expect(s.stage("crossfeed")?.cutoff_hz).toBe(5000);
    await idle(s);
    expect(s.getState().stageErrors.crossfeed).toMatch(/cutoff_hz must be within 300/);
    expect(s.getState().stageDrafts.crossfeed).toBeUndefined();
    expect(daemon.config.chain.crossfeed ?? null).toBeNull();
    // A later accepted edit clears it.
    s.editStage("crossfeed", () => ({ enabled: true, mode: "bs2b_chu_moy", cutoff_hz: 700, feed_db: 4.5, level: "matched" }));
    await idle(s);
    expect(s.getState().stageErrors.crossfeed).toBeUndefined();
    expect(daemon.config.chain.crossfeed?.mode).toBe("bs2b_chu_moy");
  });

  it("pitch, harmonics and the limiter go out with their own calls", async () => {
    const { s, t, daemon } = await setup();
    s.editStage("pitch", (c) => ({ ...c, enabled: true, cents: -50 }));
    s.setStageEnabled("harmonics", true);
    s.setStageEnabled("limiter", true);
    await idle(s);
    expect(daemon.config.chain.pitch.cents).toBe(-50);
    expect(daemon.config.chain.harmonics?.character).toBe("triode");
    expect(daemon.config.chain.limiter.mode).toBe("auto_headroom");
    expect(t.count("set_pitch")).toBe(1);
    expect(t.count("set_harmonics")).toBe(1);
    expect(t.count("set_limiter")).toBe(1);
    // The stages break passthrough as the engine reports it.
    await until(() => s.getState().status?.passthrough.broken_because.some((b) => b.reason === "dsp_active") ?? false);
    // Limiter off remembers the mode.
    s.editStage("limiter", (c) => ({ ...c, mode: "auto_headroom" }));
    s.setStageEnabled("limiter", false);
    s.setStageEnabled("limiter", true);
    await idle(s);
    expect(daemon.config.chain.limiter.mode).toBe("auto_headroom");
    // Pitch refuses two ways of stating the shift at once.
    s.editStage("pitch", (c) => ({ ...c, semitones: 2, cents: 10 }));
    await idle(s);
    expect(s.getState().stageErrors.pitch).toMatch(/at most one/);
    expect(daemon.config.chain.pitch.cents).toBe(-50);
  });

  it("an engine without the call disables the stage with a plain reason", async () => {
    const { s } = await setup({ refuse: ["set_harmonics"] });
    s.setStageEnabled("harmonics", true);
    await idle(s);
    expect(s.getState().stageCaps.harmonics).toBe(false);
    expect(s.getState().stageErrors.harmonics).toMatch(/Update the engine/);
  });
});

describe("volume settings", () => {
  it("go through set_config and never write the level", async () => {
    const { s, t, daemon } = await setup();
    const level = daemon.volume.volume_db;
    const sets = t.count("set_volume") + t.count("set_volume_guarded");
    await s.setVolumeSettings((v) => ({ ...v, guard: { ...v.guard, mode: "lock" }, sink_volume: "follow", volume_db: 0, muted: true }));
    expect(t.count("set_config")).toBe(1);
    expect(t.count("set_volume") + t.count("set_volume_guarded")).toBe(sets);
    const sent = t.last("set_config") as { volume: Record<string, unknown> };
    expect(sent.volume).not.toHaveProperty("volume_db");
    expect(sent.volume).not.toHaveProperty("muted");
    expect(daemon.config.volume.guard.mode).toBe("lock");
    expect(daemon.volume.volume_db).toBe(level);
    await until(() => s.getState().status?.volume?.guard === "lock" && s.getState().status?.volume?.sink_volume === "follow");
  });

  it("a ceiling below the level lowers it; hybrid needs a DAC level", async () => {
    const { s, daemon } = await setup();
    await s.setVolumeSettings((v) => ({ ...v, max_volume_db: -20 }));
    expect(daemon.volume.volume_db).toBe(-20);
    await until(() => s.getState().status?.volume?.ceiling_db === -20);
    const ok = await s.setVolumeSettings((v) => ({ ...v, mode: "hybrid", hardware_db: null }));
    expect(ok).toBe(false);
    expect(s.getState().stageErrors.volume).toMatch(/Hybrid needs a DAC level/);
  });

  it("No rise limit sends 0 and turning it off restores the last rate, all without the level", async () => {
    const { s, t, daemon } = await setup();
    const level = daemon.volume.volume_db;
    const levelSets = t.count("set_volume") + t.count("set_volume_guarded");
    await s.setVolumeSettings((v) => ({ ...v, guard: { ...v.guard, mode: "limit", max_rise_db_per_s: 12 } }));
    expect(daemon.config.volume.guard.max_rise_db_per_s).toBe(12);
    const n = t.count("set_config");
    expect(await s.setGuardRiseLimit(false)).toBe(true);
    expect(t.count("set_config")).toBe(n + 1);
    const sent = t.last("set_config") as { volume: Record<string, unknown> & { guard: { max_rise_db_per_s: number; mode: string } } };
    expect(sent.volume.guard).toMatchObject({ mode: "limit", max_rise_db_per_s: 0 });
    for (const k of ["volume_db", "balance_db", "muted"]) expect(sent.volume).not.toHaveProperty(k);
    expect(daemon.config.volume.guard.max_rise_db_per_s).toBe(0);
    // Back on: the 12 dB/s it had.
    expect(await s.setGuardRiseLimit(true)).toBe(true);
    expect(daemon.config.volume.guard.max_rise_db_per_s).toBe(12);
    expect(daemon.volume.volume_db).toBe(level);
    expect(t.count("set_volume") + t.count("set_volume_guarded")).toBe(levelSets);
  });

  it("typing 0 as max rise keeps the old rate for No rise limit off", async () => {
    const { s, daemon } = await setup();
    await s.setVolumeSettings((v) => ({ ...v, guard: { ...v.guard, mode: "limit", max_rise_db_per_s: 19 } }));
    expect(await s.setGuardRiseRate(0)).toBe(true);
    expect(daemon.config.volume.guard.max_rise_db_per_s).toBe(0);
    expect(await s.setGuardRiseLimit(true)).toBe(true);
    expect(daemon.config.volume.guard.max_rise_db_per_s).toBe(19);
  });

  it("No rise limit off with no rate remembered goes to 6 dB/s", async () => {
    const { s, daemon } = await setup();
    await s.setVolumeSettings((v) => ({ ...v, guard: { ...v.guard, mode: "limit", max_rise_db_per_s: 0 } }));
    expect(await s.setGuardRiseLimit(true)).toBe(true);
    expect(daemon.config.volume.guard.max_rise_db_per_s).toBe(6);
  });

  it("reports a DAC without a mixer as hardware unavailable", async () => {
    const { s } = await setup({ digital: true });
    await s.setVolumeSettings((v) => ({ ...v, mode: "hardware" }));
    await until(() => s.getState().status?.volume?.hardware_unavailable === true);
  });
});

describe("EQ tilt", () => {
  it("goes out with set_eq, counts toward the auto preamp, and is undoable", async () => {
    const { s, t, daemon } = await setup();
    const preampBefore = daemon.config.chain.eq!.preamp_db;
    const n = t.count("set_eq");
    s.editEq((eq) => (eq ? { ...eq, tilt: { enabled: true, slope_db_per_oct: 3, total_db: null, start_hz: 1000, end_hz: 16000, pivot: "start" } } : eq));
    await idle(s);
    expect(t.count("set_eq")).toBe(n + 1);
    expect(daemon.config.chain.eq?.tilt?.slope_db_per_oct).toBe(3);
    // +12 dB at the top on top of the bands: the preamp comes down.
    expect(daemon.config.chain.eq!.preamp_db).toBeLessThan(preampBefore);
    await until(() => s.getState().status?.eq_tilt?.effective_end_hz === 16000);
    s.undo();
    await idle(s);
    expect(daemon.config.chain.eq?.tilt ?? null).toBeNull();
  });

  it("the engine's clamp note and refusal reach the view", async () => {
    const { s, daemon } = await setup();
    daemon.config.rate = { ...daemon.config.rate, follow_source: false, default_rate: 44100 };
    s.editEq((eq) => (eq ? { ...eq, tilt: { enabled: true, slope_db_per_oct: -1, total_db: null, start_hz: 20, end_hz: 20000, pivot: "center" } } : eq));
    await idle(s);
    await until(() => !!s.getState().status?.eq_tilt?.clamp_note);
    s.editEq((eq) => (eq ? { ...eq, tilt: { ...eq.tilt!, slope_db_per_oct: 9 } } : eq));
    await idle(s);
    expect(daemon.config.chain.eq?.tilt?.slope_db_per_oct).toBe(-1);
  });
});

describe("review fixes", () => {
  it("output, formats, rates and device use their targeted setters", async () => {
    const { s, t, daemon } = await setup();
    s.setOutput({ dither: "feedback5" });
    s.setPreferredFormat("s24");
    s.toggleAllowedRate(96000);
    s.setRateConflict("highest");
    await s.setDevice("hw:CARD=PCH,DEV=0");
    await idle(s);
    expect(t.count("set_config")).toBe(0);
    expect(t.last("set_output")).toEqual({ bits: 24, dither: "feedback5", headroom_db: 3 });
    expect((t.last("set_formats") as string[])[0]).toBe("s24");
    expect(t.count("set_rate")).toBe(2);
    expect(t.last("set_device")).toEqual({ device: "hw:CARD=PCH,DEV=0" });
    expect(daemon.config.chain.output.dither).toBe("feedback5");
    expect(daemon.config.rate.allowed_rates).not.toContain(96000);
    expect(daemon.config.rate.conflict).toBe("highest");
    expect(daemon.config.device).toBe("hw:CARD=PCH,DEV=0");
  });

  it("the bypass resampler setting goes through set_rate", async () => {
    const { s, t, daemon } = await setup();
    expect(daemon.config.rate.bypass_resampler).toBe("engine");
    s.setBypassResampler("pipewire");
    await idle(s);
    expect(t.count("set_config")).toBe(0);
    expect((t.last("set_rate") as { bypass_resampler: string }).bypass_resampler).toBe("pipewire");
    expect(daemon.config.rate.bypass_resampler).toBe("pipewire");
    expect(s.getState().config?.rate.bypass_resampler).toBe("pipewire");
  });

  it("volume settings leave the level keys out of set_config", async () => {
    const { s, t } = await setup();
    await s.setVolumeSettings((v) => ({ ...v, sink_volume: "follow" }));
    const sent = t.last("set_config") as { volume: Record<string, unknown> };
    expect(sent.volume).toBeDefined();
    expect("volume_db" in sent.volume).toBe(false);
    expect("balance_db" in sent.volume).toBe(false);
    expect("muted" in sent.volume).toBe(false);
  });

  it("checks volume setting bounds before sending", async () => {
    const { volumeSettingsProblem } = await import("../lib/volumeSettings");
    const base = { mode: "auto", hardware_db: null, max_volume_db: null, sink_volume: "ignore", guard: { mode: "off", max_rise_db_per_s: 6, stream_max_db: null } } as const;
    expect(volumeSettingsProblem({ ...base })).toBeNull();
    expect(volumeSettingsProblem({ ...base, max_volume_db: 3 })?.field).toBe("ceiling");
    expect(volumeSettingsProblem({ ...base, hardware_db: 6 })?.field).toBe("hardware");
    expect(volumeSettingsProblem({ ...base, mode: "hybrid" })?.field).toBe("hardware");
    expect(volumeSettingsProblem({ ...base, guard: { mode: "limit", max_rise_db_per_s: 2000, stream_max_db: null } })?.field).toBe("rise");
    expect(volumeSettingsProblem({ ...base, guard: { mode: "limit", max_rise_db_per_s: 6, stream_max_db: 1 } })?.field).toBe("stream");
    const { s, t } = await setup();
    const n = t.count("set_config");
    const ok = await s.setVolumeSettings((v) => ({ ...v, max_volume_db: 5 }));
    expect(ok).toBe(false);
    expect(t.count("set_config")).toBe(n);
  });

  it("the limiter's Off in the view and the sidebar power restore the same mode", async () => {
    const { s, daemon } = await setup();
    s.setLimiterMode("auto_headroom");
    s.setLimiterMode("off");
    await idle(s);
    expect(daemon.config.chain.limiter.mode).toBe("off");
    s.setStageEnabled("limiter", true);
    await idle(s);
    expect(daemon.config.chain.limiter.mode).toBe("auto_headroom");
  });

  it("an optional harmonic comes back at its last level", async () => {
    const { s, daemon } = await setup();
    s.setStage("harmonics", { enabled: true, character: "custom", h2_db: -40, h3_db: -58, h4_db: -20, h5_db: null, dc_block: true });
    s.setOptionalHarmonic("h4_db", false);
    await idle(s);
    expect(daemon.config.chain.harmonics?.h4_db ?? null).toBeNull();
    s.setOptionalHarmonic("h4_db", true);
    await idle(s);
    expect(daemon.config.chain.harmonics?.h4_db).toBe(-20);
  });

  it("crossfeed level support is known only from the daemon's config", async () => {
    const { s, t, daemon } = await setup();
    expect(daemon.config.chain.crossfeed ?? null).toBeNull();
    expect(s.getState().stageCaps.crossfeedLevel).toBeNull();
    // Choosing a level with no config sends the default stage, still off.
    s.editStage("crossfeed", (c) => withCrossfeedLevel(c, "peak_safe"));
    await idle(s);
    expect(t.last("set_crossfeed")).toMatchObject({ enabled: false, level: "peak_safe" });
    expect(daemon.config.chain.crossfeed).toMatchObject({ enabled: false, level: "peak_safe" });
    await until(() => s.getState().stageCaps.crossfeedLevel === true);
    // Editing the level of an enabled stage keeps it on.
    s.setStageEnabled("crossfeed", true);
    await idle(s);
    s.editStage("crossfeed", (c) => withCrossfeedLevel(c, "matched"));
    await idle(s);
    expect(daemon.config.chain.crossfeed).toMatchObject({ enabled: true, level: "matched" });
  });

  it("an engine without the crossfeed level: a draft with `level` does not count, the echo does", async () => {
    const { s, daemon } = await setup({ legacyCrossfeedLevel: true });
    s.editStage("crossfeed", () => ({ ...defaultCrossfeed(), level: "matched" }));
    // The draft carries `level`, but nothing is known until the daemon answers.
    expect(s.getState().stageCaps.crossfeedLevel).toBeNull();
    await idle(s);
    await until(() => s.getState().stageCaps.crossfeedLevel === false);
    const echoed = daemon.config.chain.crossfeed!;
    expect("level" in echoed).toBe(false);
    expect(echoed.enabled).toBe(true);
    // Shown as what the engine runs: peak-safe.
    expect(crossfeedLevel(crossfeedAsRun(s.stage("crossfeed")!, s.getState().stageCaps.crossfeedLevel))).toBe("peak_safe");
  });

  it("an older engine (no guard, no limiter) disables those controls instead of crashing", async () => {
    const { s, daemon } = await setup();
    const cfg = JSON.parse(JSON.stringify(daemon.config));
    delete cfg.volume.guard;
    delete cfg.chain.limiter;
    daemon.snapshotConfig(cfg);
    await until(() => s.getState().stageCaps.volumeSettings === false && s.getState().stageCaps.limiter === false);
    expect(() => s.setStageEnabled("limiter", true)).not.toThrow();
    expect(s.getState().stageCaps.ditherKinds).toBe(false);
  });

  it("mock: a disabled tilt is still validated", async () => {
    const { s, daemon } = await setup();
    s.editEq((eq) => (eq ? { ...eq, tilt: { enabled: false, slope_db_per_oct: -1, total_db: null, start_hz: 1, end_hz: 20000, pivot: "center" } } : eq));
    await idle(s);
    expect(daemon.config.chain.eq?.tilt ?? null).toBeNull();
  });

  it("mock: hybrid's digital attenuation breaks passthrough on a flat chain", async () => {
    const { s, daemon } = await setup();
    s.setEqEnabled(false);
    s.setConvEnabled(false);
    s.setUpsamplingEnabled(false);
    s.setOutput({ headroom_db: 0 });
    await idle(s);
    await until(() => s.getState().status?.passthrough.intact === true);
    await s.setVolumeSettings((v) => ({ ...v, mode: "hybrid", hardware_db: -6 }));
    expect(daemon.volume.software_db).toBeLessThan(0);
    await until(() => s.getState().status?.passthrough.intact === false);
    expect(s.getState().status?.passthrough.broken_because.map((b) => b.reason)).toContain("dsp_active");
  });

  it("ending a stage wheel drag after Off never replays enabled: true", async () => {
    const { s, t, daemon } = await setup();
    // Simulate a wheel notch on Crossfeed cutoff (switches to Custom, enabled: true).
    s.editStage("crossfeed", (c) => ({ ...(c ?? { enabled: true, mode: "bs2b_default", cutoff_hz: 700, feed_db: 4.5, level: "matched" }), enabled: true, mode: "bs2b_custom", cutoff_hz: 705 }), "drag");
    // Turn Crossfeed Off within the wheel window before endStageDrag fires.
    s.setStageEnabled("crossfeed", false);
    await idle(s);
    const callsAfterOff = t.count("set_crossfeed");
    expect(daemon.config.chain.crossfeed?.enabled).toBe(false);
    expect(daemon.config.chain.crossfeed?.cutoff_hz).toBe(705);
    // Wheel idle timeout fires: must be a no-op and leave Crossfeed off.
    s.endStageDrag("crossfeed");
    await idle(s);
    expect(t.count("set_crossfeed")).toBe(callsAfterOff);
    expect(daemon.config.chain.crossfeed?.enabled).toBe(false);
    expect(s.getState().stageDrafts.crossfeed).toBeUndefined();
  });

  it("endStageDrag flushes a throttled drag edit and clears the draft", async () => {
    const { s, daemon } = await setup();
    s.editStage("crossfeed", () => ({ enabled: true, mode: "bs2b_custom", cutoff_hz: 705, feed_db: 4.5, level: "matched" }), "drag");
    // Second notch within the throttle interval stays pending until endStageDrag.
    s.editStage("crossfeed", (c) => ({ ...c!, cutoff_hz: 710 }), "drag");
    s.endStageDrag("crossfeed");
    await idle(s);
    expect(daemon.config.chain.crossfeed?.cutoff_hz).toBe(710);
    expect(s.getState().stageDrafts.crossfeed).toBeUndefined();
  });

  it("endEqDrag after turning Tilt off within a wheel window leaves Tilt off", async () => {
    const { s, t, daemon } = await setup();
    s.beginEqDrag();
    s.editEq((eq) => (eq ? { ...eq, tilt: { enabled: true, slope_db_per_oct: 0.5, total_db: null, start_hz: 20, end_hz: 20000, pivot: "center" } } : eq), "drag");
    s.editEq((eq) => (eq ? { ...eq, tilt: { ...eq.tilt!, enabled: false } } : eq), "commit", false);
    await idle(s);
    const callsAfterOff = t.count("set_eq");
    expect(daemon.config.chain.eq?.tilt?.enabled).toBe(false);
    s.endEqDrag();
    await idle(s);
    expect(t.count("set_eq")).toBe(callsAfterOff);
    expect(daemon.config.chain.eq?.tilt?.enabled).toBe(false);
  });
});

describe("varispeed", () => {
  it("warns when the output clips with the limiter off, and one click turns it on", async () => {
    const { s, daemon } = await setup();
    expect(daemon.config.chain.limiter.mode).toBe("off");
    // The mock's meters carry its own clip counters: drive them.
    const clips = daemon as unknown as { clippedCh: number[] };
    const clipTo = (n: number) => (clips.clippedCh = [n, 0]);
    const base = 1_000_000;
    clipTo(base);
    await until(() => meterStoreClipped() === base);
    await new Promise((r) => setTimeout(r, 100));
    s.dismissClipWarning();
    expect(s.getState().clipNoLimiter).toBe(false);
    clipTo(base + 3);
    await until(() => s.getState().clipNoLimiter);
    s.turnOnLimiter();
    expect(s.getState().clipNoLimiter).toBe(false);
    await idle(s);
    expect(daemon.config.chain.limiter.mode).toBe("auto_headroom");
    // With the limiter on, more clips raise no warning.
    clipTo(base + 9);
    await until(() => (meterStoreClipped() ?? 0) >= base + 9);
    await new Promise((r) => setTimeout(r, 100));
    expect(s.getState().clipNoLimiter).toBe(false);
  });

  it("the Pitch page's one switch turns pitch and varispeed on and off together", async () => {
    const { s, t, daemon } = await setup();
    daemon.config.chain.pitch = { enabled: false, cents: 0, quality: "high" };
    daemon.config.varispeed = { ...daemon.config.varispeed, enabled: false };
    await s.refreshConfig();
    expect(s.pitchPageOn()).toBe(false);
    s.setStageEnabled("pitch", true);
    expect(s.pitchPageOn()).toBe(true);
    await idle(s);
    expect(t.last("set_pitch")).toMatchObject({ enabled: true });
    expect(t.last("set_varispeed")).toEqual({ enabled: true });
    expect(daemon.config.chain.pitch.enabled).toBe(true);
    expect(daemon.config.varispeed.enabled).toBe(true);
    // On when either is on; off turns both off, and only what changes is sent.
    daemon.config.chain.pitch = { ...daemon.config.chain.pitch, enabled: false };
    await s.refreshConfig();
    expect(s.pitchPageOn()).toBe(true);
    const pitchCalls = t.count("set_pitch");
    s.setPitchPageEnabled(false);
    await idle(s);
    expect(t.count("set_pitch")).toBe(pitchCalls);
    expect(t.last("set_varispeed")).toEqual({ enabled: false });
    expect(daemon.config.chain.pitch.enabled).toBe(false);
    expect(daemon.config.varispeed.enabled).toBe(false);
    expect(s.pitchPageOn()).toBe(false);
  });

  it("sends set_varispeed with only the changed fields and throttles a drag", async () => {
    const { s, t, daemon } = await setup();
    s.editVarispeed({ enabled: true });
    expect(s.varispeed().enabled).toBe(true);
    await idle(s);
    expect(t.last("set_varispeed")).toEqual({ enabled: true });
    expect(daemon.config.varispeed.enabled).toBe(true);
    for (let i = 0; i < 30; i++) {
      s.editVarispeed({ speed: 1 + i * 0.002 }, "drag");
      await sleep(4);
    }
    s.endVarispeedDrag();
    await idle(s);
    await until(() => s.getState().varispeedDraft === null);
    expect(t.count("set_varispeed")).toBeLessThan(16);
    expect(daemon.config.varispeed.speed).toBeCloseTo(1.058, 6);
    expect(s.getState().status?.varispeed?.active).toBe(true);
  });

  it("toggles Keep pitch with its own field and keeps it across speed edits", async () => {
    const { s, t, daemon } = await setup();
    s.editVarispeed({ enabled: true, speed: 1.1 });
    await idle(s);
    s.editVarispeed({ keep_pitch: true });
    await idle(s);
    expect(t.last("set_varispeed")).toEqual({ keep_pitch: true });
    expect(daemon.config.varispeed).toMatchObject({ enabled: true, speed: 1.1, keep_pitch: true });
    await until(() => s.getState().status?.varispeed?.keep_pitch === true);
    // The stretch's delay (about 135 ms) is what status reports.
    expect(s.getState().status?.varispeed?.latency_frames).toBeGreaterThan(6000);
    s.editVarispeed({ speed: 1.05 });
    await idle(s);
    expect(daemon.config.varispeed.keep_pitch).toBe(true);
    expect(s.varispeed().keep_pitch).toBe(true);
  });

  it("shows the Keep pitch refusal at a rate over the budget", async () => {
    const { s, daemon } = await setup();
    daemon.config.rate = { ...daemon.config.rate, follow_source: false, default_rate: 192000 };
    s.editVarispeed({ enabled: true, keep_pitch: true });
    await idle(s);
    expect(s.getState().varispeedError).toMatch(/Keep pitch is not available at 192000 Hz/);
    expect(daemon.config.varispeed.keep_pitch).toBe(false);
    expect(daemon.config.varispeed.enabled).toBe(false);
  });

  it("shows the refusal and drops the draft", async () => {
    const { s, daemon } = await setup();
    s.editVarispeed({ speed: 3 });
    await idle(s);
    expect(s.getState().varispeedError).toMatch(/between 0.5 and 2/);
    expect(s.getState().varispeedDraft).toBeNull();
    expect(daemon.config.varispeed.speed).toBe(1);
  });
});
