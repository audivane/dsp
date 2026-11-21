import { describe, expect, it } from "vitest";
import { MockDaemon } from "./mock";

// Mock parity with the engine bypass resampler (control-plane.md, "The
// engine bypass resampler"): Direct and DSP off at a rate the DAC lacks.
describe("mock engine bypass resampler", () => {
  it("keeps 44.1 kHz on Direct and converts it on a 48 kHz-only DAC", () => {
    const d = new MockDaemon({ device: "hw:CARD=USB48,DEV=0" });
    d.handle("route", { target: { node: 81 }, sink: "direct" });
    const st = d.status();
    expect(st.graph_rate).toBe(44100);
    expect(st.output_rate).toBe(48000);
    expect(st.rate_ratio).toEqual({ up: 160, down: 147 });
    expect(st.bypass_resampler).toEqual({ mode: "engine", active: true, reason: null });
    expect(st.passthrough.broken_because).toContainEqual({ reason: "rate_conversion", graph_rate: 44100, output_rate: 48000 });
    // The conversion (and its dither) counts as active DSP, at unity volume too.
    expect(st.passthrough.broken_because).toContainEqual({ reason: "dsp_active" });
    expect(st.direct_rates).toContain(44100);
    expect(st.effective_rates).toContain(44100);

    // PipeWire mode: the graph moves to 48 kHz and the stream is resampled.
    d.handle("set_rate", { ...d.config.rate, bypass_resampler: "pipewire" });
    const pw = d.status();
    expect(pw.graph_rate).toBe(48000);
    expect(pw.rate_ratio).toEqual({ up: 1, down: 1 });
    expect(pw.bypass_resampler).toEqual({ mode: "pipewire", active: false, reason: null });
    expect(pw.passthrough.broken_because).toContainEqual({ reason: "resampled", stream_rate: 44100, graph_rate: 48000 });
    expect(pw.direct_rates).not.toContain(44100);

    // A missing field is the default; an unknown one is refused.
    const { bypass_resampler: _drop, ...old } = d.config.rate;
    void _drop;
    d.handle("set_rate", old);
    expect(d.config.rate.bypass_resampler).toBe("engine");
    expect(() => d.handle("set_rate", { ...d.config.rate, bypass_resampler: "alsa" })).toThrow(/unknown variant/);
    d.stop();
  });

  it("DSP off converts on the DSP sink too", () => {
    const d = new MockDaemon({ device: "hw:CARD=USB48,DEV=0" });
    d.handle("set_dsp_enabled", { enabled: false });
    const st = d.status();
    expect(st.active_sink).toBe("dsp");
    expect(st.graph_rate).toBe(44100);
    expect(st.rate_ratio).toEqual({ up: 160, down: 147 });
    expect(st.bypass_resampler?.active).toBe(true);
    d.stop();
  });

  it("a native rate stays x1 and bit-perfect", () => {
    const d = new MockDaemon();
    d.handle("route", { target: { node: 81 }, sink: "direct" });
    const st = d.status();
    expect(st.graph_rate).toBe(44100);
    expect(st.rate_ratio).toEqual({ up: 1, down: 1 });
    expect(st.bypass_resampler).toEqual({ mode: "engine", active: false, reason: null });
    expect(st.passthrough.broken_because.some((b) => b.reason === "rate_conversion" || b.reason === "resampled" || b.reason === "dsp_active")).toBe(false);
    d.stop();
  });

  it("adds converted rates to the native ones, never in their place", () => {
    // Allowed 44.1 and 88.2 kHz on a 48 kHz-only DAC with DSP off: 48 kHz
    // (what the policy widens to) stays, so a native source plays untouched
    // and nothing is converted twice.
    const d = new MockDaemon({ device: "hw:CARD=USB48,DEV=0" });
    d.handle("set_dsp_enabled", { enabled: false });
    d.handle("set_rate", { ...d.config.rate, allowed_rates: [44100, 88200], default_rate: 44100 });
    const at = (rate: number) => {
      d.streams = d.streams.map((s) => (s.node === 81 ? { ...s, rate } : s));
      return d.status();
    };
    let st = at(48000);
    expect(st.effective_rates).toEqual([44100, 48000, 88200]);
    expect(st.effective_default_rate).toBe(48000);
    expect([st.graph_rate, st.rate_ratio]).toEqual([48000, { up: 1, down: 1 }]);
    expect(st.bypass_resampler?.active).toBe(false);
    st = at(44100);
    expect([st.graph_rate, st.rate_ratio]).toEqual([44100, { up: 160, down: 147 }]);
    st = at(88200);
    expect([st.graph_rate, st.rate_ratio]).toEqual([88200, { up: 80, down: 147 }]);
    // Any other rate is resampled by PipeWire into 48 kHz, once.
    st = at(96000);
    expect([st.graph_rate, st.rate_ratio]).toEqual([48000, { up: 1, down: 1 }]);
    expect(st.passthrough.broken_because).toContainEqual({ reason: "resampled", stream_rate: 96000, graph_rate: 48000 });
    expect(st.passthrough.broken_because.some((b) => b.reason === "rate_conversion")).toBe(false);
    d.stop();
  });

  it("keeps Direct's current rate while a client plays at it, whatever the conflict mode", () => {
    // Like the arbiter: Direct is first come, first served (keep_current).
    const d = new MockDaemon();
    d.handle("set_rate", { ...d.config.rate, conflict: "highest" });
    d.handle("route", { target: { node: 81 }, sink: "direct" });
    expect(d.status().graph_rate).toBe(44100);
    d.handle("route", { target: { node: 94 }, sink: "direct" });
    const st = d.status();
    expect(st.active_sink).toBe("direct");
    expect(st.graph_rate).toBe(44100);
    expect(st.passthrough.broken_because).toContainEqual({ reason: "resampled", stream_rate: 48000, graph_rate: 44100 });
    // The DSP sink still follows `highest`.
    d.handle("route", { target: { node: 81 }, sink: "dsp" });
    d.handle("route", { target: { node: 94 }, sink: "dsp" });
    expect(d.status().graph_rate).toBe(48000);
    d.stop();
  });

  it("moves Direct to the first client that started when the one at the current rate leaves", () => {
    // Started in the order Firefox (48 kHz), Discord (96 kHz), Strawberry
    // (44.1 kHz, stopped and started again), against the fixture order
    // Strawberry, Firefox, Discord.
    const d = new MockDaemon();
    const strawberry = d.streams.find((s) => s.node === 81)!;
    d.streams = d.streams.filter((s) => s.node !== 81).map((s) => (s.node === 102 ? { ...s, rate: 96000 } : s));
    d.streams = [...d.streams, { ...strawberry, sink: null }];
    for (const node of [94, 102, 81]) d.handle("route", { target: { node }, sink: "direct" });
    expect(d.status().active_sink).toBe("direct");
    expect(d.status().graph_rate).toBe(48000);
    // Discord moves away and back while it plays: it keeps its age.
    d.handle("route", { target: { node: 102 }, sink: null });
    d.handle("route", { target: { node: 102 }, sink: "direct" });
    expect(d.status().graph_rate).toBe(48000);
    // Firefox, at the current rate, stops: Discord started first of the
    // rest, so the graph follows it (the arbiter's first-started rule).
    d.streams = d.streams.filter((s) => s.node !== 94);
    expect(d.status().graph_rate).toBe(96000);
    // Discord stops and starts again: now Strawberry is the oldest.
    const discord = d.streams.find((s) => s.node === 102)!;
    d.streams = d.streams.filter((s) => s.node !== 102);
    d.streams = [...d.streams, discord];
    d.handle("route", { target: { node: 81 }, sink: null });
    d.handle("route", { target: { node: 81 }, sink: "direct" });
    d.streams = d.streams.map((s) => (s.node === 102 ? { ...s, rate: 88200 } : s));
    expect(d.status().graph_rate).toBe(44100);
    d.stop();
  });
});
