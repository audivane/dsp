import { afterEach, describe, expect, it } from "vitest";
import type { DaemonEvent, EngineConfig, UpsamplerConfig } from "../api/types";
import { MockDaemon } from "../api/mock";
import { DaemonError } from "../api/transport";
import { cpuRefusal, rateAdmissions, RT_BUDGET, upsamplerLoad } from "./cpuBudget";
import { patchUpsamplerConfig } from "./upsamplerAdapter";
import { MOCK_MACHINE } from "./polyphasePlan";

const DESKTOP = { ...MOCK_MACHINE };
/** A slow laptop: on the desktop the FFT polyphase engine runs every kernel up to 8M taps. */
function slowMachine() {
  MOCK_MACHINE.spectralRate = DESKTOP.spectralRate / 10;
  MOCK_MACHINE.fftUnit = DESKTOP.fftUnit * 10;
}

const custom = (taps: number, ratio: number, base: UpsamplerConfig): UpsamplerConfig =>
  patchUpsamplerConfig(base, { preset: null, auto_taps: null, taps, taps_unit: "output", target: { ratio } });

function at48(d: MockDaemon): EngineConfig {
  const cfg = structuredClone(d.config);
  cfg.rate.allowed_rates = [48000];
  return cfg;
}

describe("mock CPU admission", () => {
  afterEach(() => Object.assign(MOCK_MACHINE, DESKTOP));

  it("refuses a design too heavy for real time with the daemon's message", () => {
    slowMachine();
    const d = new MockDaemon();
    const old = at48(d);
    const heavy = { ...old, chain: { ...old.chain, upsampler: custom(4 << 20, 16, old.chain.upsampler) } };
    const msg = cpuRefusal(old, heavy);
    expect(msg).toMatch(/^Too heavy for this CPU at 768 kHz from a 48 kHz source: about [\d.]+ G MAC\/s needed \(\d+% of one core\), this machine sustains about 9\.0 G MAC\/s in real time\. Use fewer taps or a lower output rate\.$/);
    // Light designs and presets pass; an edit that keeps the upsampler passes.
    const light = { ...old, chain: { ...old.chain, upsampler: custom(4095, 4, old.chain.upsampler) } };
    expect(cpuRefusal(old, light)).toBeNull();
    expect(upsamplerLoad({ ...heavy.chain.upsampler, preset: "gaussian_long" }, 48000, 1024)).toBe(0);
    const eqOnly = { ...heavy, chain: { ...heavy.chain, eq: null } };
    expect(cpuRefusal(heavy, eqOnly)).toBeNull();
  });

  it("a simulated overload is reported until the upsampler changes", () => {
    slowMachine();
    const d = new MockDaemon();
    const events: DaemonEvent[] = [];
    d.subscribe((e) => events.push(e));
    d.handle("set_upsampler", custom(65536, 16, d.config.chain.upsampler));
    d.simulateOverload(1.4);
    const ev = events.find((e) => e.event === "overload");
    expect(ev?.event === "overload" && ev.data.action).toBe("upsampler_off");
    const st = d.status();
    // The engine runs without the upsampler; the saved setting is kept.
    expect(st.rate_ratio).toEqual({ up: 1, down: 1 });
    expect(d.config.chain.upsampler.target).toEqual({ ratio: 16 });
    expect(st.overload?.message).toBe(
      "The engine switched the upsampler off: it needed more CPU than real time allows (load 140%). Your setting is kept; lower the taps or the output rate.",
    );
    // A refused heavy design leaves it in place.
    expect(() => d.handle("set_upsampler", custom(4 << 20, 16, d.config.chain.upsampler))).toThrow(DaemonError);
    expect(d.status().overload).not.toBeNull();
    // A new upsampler setting ends it.
    d.handle("set_upsampler", custom(4095, 4, d.config.chain.upsampler));
    expect(d.status().overload).toBeNull();
  });

  it("admits what fits the rate playing and falls back at the others", () => {
    const d = new MockDaemon();
    const old = structuredClone(d.config);
    old.rate.allowed_rates = [48000, 192000];
    // To 768 kHz: x16 from 48 kHz, x4 from 192 kHz (four times the cost).
    let cfg: EngineConfig | null = null;
    for (let k = 1; k <= 256 && !cfg; k++) {
      const up = { ...custom(k << 16, 16, old.chain.upsampler), target: { max_rate: 768000 } } as UpsamplerConfig;
      const next = { ...old, chain: { ...old.chain, upsampler: up } };
      const rows = rateAdmissions(next);
      const l = (r: number) => rows.find((w) => w.source_rate === r)!.load;
      if (l(48000) <= RT_BUDGET && l(192000) > RT_BUDGET) cfg = next;
    }
    expect(cfg).not.toBeNull();
    const next = cfg!;
    expect(cpuRefusal(old, next, 48000)).toBeNull();
    expect(cpuRefusal(old, next, 192000)).toMatch(/^Too heavy for this CPU at 768 kHz from a 192 kHz source with A\/B: side B's upsampler/);
    expect(cpuRefusal(old, next)).toMatch(/from a 192 kHz source/);
    const rows = rateAdmissions(next);
    expect(rows.find((r) => r.source_rate === 48000)?.fallback).toBeUndefined();
    expect(rows.find((r) => r.source_rate === 192000)?.fallback).toMatchObject({ action: "light_upsampler", cause: "estimate" });
    // A rate that overloaded falls back too.
    expect(rateAdmissions(next, [48000]).find((r) => r.source_rate === 48000)?.fallback?.cause).toBe("overload");
    expect(rateAdmissions({ ...next, dsp_enabled: false })).toEqual([]);
  });
});
