import { describe, expect, it } from "vitest";
import { MockDaemon, MOCK_LIMITS } from "../api/mock";
import { engineLimits, FALLBACK_LIMITS, tapChoices } from "./engineLimits";
import { patchUpsamplerConfig } from "./upsamplerAdapter";

describe("engine limits", () => {
  it("falls back to the limits of a daemon that does not publish them", () => {
    expect(engineLimits(null)).toEqual(FALLBACK_LIMITS);
    expect(engineLimits(undefined)).toEqual(FALLBACK_LIMITS);
    expect(FALLBACK_LIMITS).toEqual({ max_kernel_taps: 4_194_304, intermediate_max_taps: 1_048_576, design_workspace_bytes: 1_610_612_736, cpu_share: 0.5 });
    const published = { max_kernel_taps: 8_388_608, intermediate_max_taps: 2_097_152, design_workspace_bytes: 2 ** 31, cpu_share: 0.75 };
    expect(engineLimits(published)).toBe(published);
  });

  it("offers 8M taps only when the engine allows it", () => {
    expect(tapChoices(4_194_304)).toEqual([4096, 8192, 16384, 32768, 65536, 131072, 262144, 524288, 1048576, 2097152, 4194304]);
    expect(tapChoices(8_388_608).at(-1)).toBe(8_388_608);
    expect(tapChoices(8_388_608)).toHaveLength(12);
    // A limit between powers of two offers the power below it.
    expect(tapChoices(6_000_000).at(-1)).toBe(4_194_304);
  });

  it("the mock reports a 32 GB machine's limits", () => {
    expect(MOCK_LIMITS).toEqual({ max_kernel_taps: 8_388_608, intermediate_max_taps: 2_097_152, design_workspace_bytes: 4 * 2 ** 30, cpu_share: 0.9 });
    const st = new MockDaemon().status();
    expect(st.limits).toEqual(MOCK_LIMITS);
    // The mock validates against them: 8M taps pass, one more is refused.
    const d = new MockDaemon();
    const up = patchUpsamplerConfig(d.config.chain.upsampler, { preset: null, auto_taps: null, taps_unit: "output", target: { ratio: 2 } });
    expect(() => d.handle("set_upsampler", patchUpsamplerConfig(up, { taps: 8_388_609 }))).toThrow(/between 1 and 8388608/);
  });
});
