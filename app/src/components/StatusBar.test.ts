import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { beforeEach, expect, it, vi } from "vitest";
import { MockDaemon } from "../api/mock";
import type { StreamInfo } from "../api/types";
import type { AppState } from "../store/store";
import { StatusBar, fmtXruns } from "./StatusBar";

const fixture = vi.hoisted(() => ({ state: {} as Pick<AppState, "status" | "streams" | "nowPlaying"> }));
vi.mock("../store/store", () => ({ useApp: (select: (s: typeof fixture.state) => unknown) => select(fixture.state) }));
vi.mock("./Meters", () => ({ Meters: () => null }));

function stream(app_name: string, rate: number, format: string, sink: StreamInfo["sink"] = "dsp"): StreamInfo {
  return { node: rate, app_name, binary: null, media_name: null, rate, format, channels: 2, sink, volume: 1, muted: false };
}
function path() {
  return renderToStaticMarkup(createElement(StatusBar)).split('aria-label="Rate path">')[1].split('<span class="badge-wrap"')[0];
}
beforeEach(() => {
  fixture.state = {
    status: {
      ...new MockDaemon().status(),
      graph_rate: 96000,
      output_rate: 384000,
      ratio: 4,
      rate_ratio: { up: 4, down: 1 },
      active_sink: "dsp",
    },
    streams: [stream("Brave", 48000, "F32LE"), stream("haruna", 96000, "S32LE")],
    nowPlaying: { player: "org.mpris.MediaPlayer2.haruna", title: "96 kHz track", artist: null, album: null, status: "Playing", native_rate: 96000, native_bits: 24 },
  };
});

it("uses the live engine input rate rather than the first stream rate", () => {
  expect(path()).toContain("IN 96k");
  expect(path()).toContain("×4");
  expect(path()).toContain("OUT 384k");
  expect(path()).not.toContain("IN 48k");
  fixture.state.streams.reverse();
  expect(path()).toContain("IN 96k");
});

it("updates both ends of the rate path when the graph rate changes", () => {
  expect(path()).toContain("IN 96k");
  fixture.state.status!.graph_rate = 48000;
  fixture.state.status!.output_rate = 192000;
  expect(path()).toContain("IN 48k");
  expect(path()).toContain("OUT 192k");
  expect(path()).not.toContain("IN 96k");
});

it("uses the graph rate even when a single stream is resampled", () => {
  fixture.state.streams = [stream("haruna", 48000, "S32LE")];
  expect(path()).toContain("IN 96k");
});

it("uses the graph rate without an active sink", () => {
  fixture.state.status!.active_sink = null;
  expect(path()).toContain("IN 96k");
});

it("shows a fractional ratio, whose integer ratio reads 0", () => {
  fixture.state.status = { ...fixture.state.status!, graph_rate: 44100, output_rate: 48000, ratio: 0, rate_ratio: { up: 160, down: 147 } };
  expect(path()).toContain("160/147");
  expect(path()).not.toContain("×0");
});

it("gives DSP load, xruns and latency their own fixed-width readout slots", () => {
  fixture.state.status = { ...fixture.state.status!, dsp_load: 0.08, xruns: 1, latency_ms: 9.5 };
  const html = renderToStaticMarkup(createElement(StatusBar));
  expect(html).toContain('class="stat-load"');
  expect(html).toContain("DSP 8%");
  expect(html).toContain('class="stat-xruns"');
  expect(html).toContain("1 xrun");
  expect(html).toContain('class="stat-latency"');
  expect(html).toContain("9.5 ms");
});

it("formats five-digit xrun counts and caps above 99999", () => {
  expect(fmtXruns(0)).toBe("0 xruns");
  expect(fmtXruns(1)).toBe("1 xrun");
  expect(fmtXruns(9999)).toBe("9999 xruns");
  expect(fmtXruns(10000)).toBe("10000 xruns");
  expect(fmtXruns(99999)).toBe("99999 xruns");
  expect(fmtXruns(100000)).toBe("99999+ xruns");
  fixture.state.status = { ...fixture.state.status!, xruns: 100000 };
  const html = renderToStaticMarkup(createElement(StatusBar));
  expect(html).toContain("99999+ xruns");
});
