import { describe, expect, it, vi } from "vitest";
import { renderToString } from "react-dom/server";
import { createElement } from "react";
import presetData from "../../../crates/protocol/src/harmonics_presets.json";
import type { HarmonicsConfig, HarmonicsLayer, HarmonicsPreset } from "./types";
import { mockHarmonicsLive } from "./mockHarmonicsLive";
import { LiveHistoryChart, LiveStats, useLiveGraphs } from "../views/HarmonicsLivePanel";
import { LiveHistory, liveHistory } from "../lib/harmonicsLive";
import { initStore } from "../store/store";

// The canvas plot is not this test's: a placeholder.
vi.mock("../views/ZoomPlot", () => ({ ZoomPlot: () => null }));

// Server rendering has no store subscription: read the state once.
vi.mock("../store/store", async (orig) => {
  const actual = await orig<typeof import("../store/store")>();
  return { ...actual, useApp: <T,>(sel: (s: ReturnType<ReturnType<typeof actual.store>["getState"]>) => T) => sel(actual.store().getState()) };
});
import { MockTransport } from "./mock";

const presets = presetData as unknown as HarmonicsPreset[];
const layer = (id: string): HarmonicsLayer => presets.find((p) => p.id === id)!.layer;
const stage = (ids: string[], routing: HarmonicsConfig["routing"] = "parallel"): HarmonicsConfig => ({ enabled: true, dc_block: true, routing, layers: ids.map(layer) }) as HarmonicsConfig;

describe("mock harmonics_live", () => {
  it("answers through the mock daemon", async () => {
    const t = new MockTransport();
    const f = await t.call<{ state: string }>("harmonics_live");
    expect(["playing", "idle", "off"]).toContain(f.state);
  });

  it("hides during a blind test in the mock", async () => {
    const t = new MockTransport();
    await t.call("set_ab", { b: { patch: { dc_block: { cutoff_hz: 7 } } } });
    await t.call("ab_blind", { enabled: true });
    const f = await t.call<{ state: string }>("harmonics_live");
    expect(f.state).toBe("hidden");
  });

  it("animates plausible frames and splits as the engine would", () => {
    const f = mockHarmonicsLive(stage(["se_triode_high_mu"]), 96000, true, 1000, 1);
    expect(f.state).toBe("playing");
    expect(f.music.length).toBe(f.added.length);
    expect(f.music.length).toBeGreaterThan(200);
    expect(f.added_db).toBeLessThan(-10);
    expect(f.split).toBe("orders");
    expect(f.parts.some((p) => p.key === "h2")).toBe(true);
    const g = mockHarmonicsLive(stage(["se_triode_high_mu"]), 96000, true, 5000, 2);
    expect(g.music_level_dbfs).not.toBe(f.music_level_dbfs);
    expect(mockHarmonicsLive(stage(["se_triode_high_mu", "tape_master"], "series"), 96000, true, 1000, 3).split).toBe("total");
    expect(mockHarmonicsLive(stage(["se_triode_high_mu", "triode"], "series"), 96000, true, 1000, 3).split).toBe("even_odd");
    expect(mockHarmonicsLive(stage(["se_triode_high_mu"]), 96000, false, 1000, 4).state).toBe("idle");
    expect(mockHarmonicsLive(null, 96000, true, 1000, 5).state).toBe("off");
  });

  it("renders the panel from a frame and its history", () => {
    initStore(new MockTransport());
    liveHistory.clear();
    const h = new LiveHistory();
    const c = stage(["se_triode_high_mu"]);
    for (let i = 0; i < 50; i++) {
      const f = mockHarmonicsLive(c, 96000, true, i * 200, i + 1);
      liveHistory.push(f, Date.now() - (50 - i) * 200);
      h.push(f, i);
    }
    const frame = mockHarmonicsLive(c, 96000, true, 10_000, 99);
    const live = (f: typeof frame, version: number) => ({ frame: f, error: null, version });
    const html = renderToString(createElement(LiveStats, { live: live(frame, 1), unit: "db" }));
    expect(html).toContain("Adding");
    expect(html).toContain("Level change");
    expect(html).toContain("Music");
    const pct = renderToString(createElement(LiveStats, { live: live(frame, 1), unit: "pct" }));
    expect(pct).toMatch(/Adding<\/div><div class="v">[0-9.]+ %/);
    const History = () => createElement(LiveHistoryChart, { frame, version: 1, g: useLiveGraphs() });
    expect(renderToString(createElement(History))).toContain("History");
    const hidden = renderToString(createElement(LiveStats, { live: live(frame, 3), blind: true, unit: "db" }));
    expect(hidden).toContain("Hidden during the blind test");
    expect(hidden).not.toContain("Adding");
    const idle = renderToString(createElement(LiveStats, { live: live({ ...frame, state: "idle" }, 2), unit: "db" }));
    expect(idle).toContain("Nothing is playing");
  });
});
