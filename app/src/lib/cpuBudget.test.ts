import { describe, expect, it } from "vitest";
import type { DaemonEvent, EngineConfig, UpsamplerConfig } from "../api/types";
import { MockDaemon } from "../api/mock";
import { DaemonError } from "../api/transport";
import { cpuRefusal, upsamplerLoad } from "./cpuBudget";
import { patchUpsamplerConfig } from "./upsamplerAdapter";

const custom = (taps: number, ratio: number, base: UpsamplerConfig): UpsamplerConfig =>
  patchUpsamplerConfig(base, { preset: null, auto_taps: null, taps, taps_unit: "output", target: { ratio } });

function at48(d: MockDaemon): EngineConfig {
  const cfg = structuredClone(d.config);
  cfg.rate.allowed_rates = [48000];
  return cfg;
}

describe("mock CPU admission", () => {
  it("refuses a design too heavy for real time with the daemon's message", () => {
    const d = new MockDaemon();
    const old = at48(d);
    const heavy = { ...old, chain: { ...old.chain, upsampler: custom(4 << 20, 16, old.chain.upsampler) } };
    const msg = cpuRefusal(old, heavy);
    expect(msg).toMatch(/^Too heavy for this CPU at 768 kHz: about [\d.]+ G MAC\/s needed \(\d+% of one core\), this machine sustains about 9\.0 G MAC\/s in real time\. Use fewer taps or a lower output rate\.$/);
    // Light designs and presets pass; an edit that keeps the upsampler passes.
    const light = { ...old, chain: { ...old.chain, upsampler: custom(4095, 4, old.chain.upsampler) } };
    expect(cpuRefusal(old, light)).toBeNull();
    expect(upsamplerLoad({ ...heavy.chain.upsampler, preset: "gaussian_long" }, 48000, 1024)).toBe(0);
    const eqOnly = { ...heavy, chain: { ...heavy.chain, eq: null } };
    expect(cpuRefusal(heavy, eqOnly)).toBeNull();
  });

  it("a simulated overload is reported until the upsampler changes", () => {
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
});
