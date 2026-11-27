import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { beforeEach, expect, it, vi } from "vitest";
import { MockDaemon } from "../api/mock";
import type { AppState, VolumeDraft } from "../store/store";
import { VolumeControls } from "./TopBar";

type Fixture = Pick<AppState, "status" | "config" | "volumeDraft" | "volumeFresh" | "connEpoch">;
const fixture = vi.hoisted(() => ({ state: {} as Fixture }));
vi.mock("../store/store", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../store/store")>()),
  useApp: (select: (s: Fixture) => unknown) => select(fixture.state),
  store: () => ({
    volumeView(): { level: VolumeDraft } | null {
      const s = fixture.state;
      if (!s.volumeFresh || !s.status?.volume?.instance) return null;
      if (s.volumeDraft) return { level: s.volumeDraft };
      const v = s.status?.volume;
      return v ? { level: { volume_db: v.volume_db, balance_db: v.balance_db, muted: v.muted } } : null;
    },
    beginVolumeGesture() {
      return null;
    },
    setVolume() {},
  }),
}));

function volumeSlider(): string {
  const html = renderToStaticMarkup(createElement(VolumeControls));
  return html.split('aria-label="Volume"')[1].split("</output>")[0];
}

beforeEach(() => {
  const daemon = new MockDaemon();
  fixture.state = { status: daemon.status(), config: null, volumeDraft: null, volumeFresh: true, connEpoch: 1 };
});

it("draws nothing until the daemon has reported a level", () => {
  fixture.state.status = null;
  const s = volumeSlider();
  expect(s).not.toContain("fine-knob");
  expect(s).not.toContain("fine-fill");
  expect(s).not.toContain("aria-valuenow");
  expect(s).not.toContain("0.0");
  expect(s).toMatch(/>-$/);
});

it("draws nothing for a status without volume", () => {
  fixture.state.status = { ...fixture.state.status!, volume: null };
  const s = volumeSlider();
  expect(s).not.toContain("fine-knob");
  expect(s).toMatch(/>-$/);
});

it("shows the reported level once it is known", () => {
  fixture.state.status = { ...fixture.state.status!, volume: { ...fixture.state.status!.volume!, volume_db: -23.5 } };
  const s = volumeSlider();
  expect(s).toContain("fine-knob");
  expect(s).toContain('aria-valuenow="-23.5"');
  expect(s).toContain("23.5");
});

function controls(): { vol: string; bal: string; mute: string } {
  const html = renderToStaticMarkup(createElement(VolumeControls));
  const after = (label: string) => html.split(`aria-label="${label}"`)[1];
  return {
    vol: after("Volume").split("</output>")[0],
    bal: after("Balance").split("</output>")[0],
    mute: html.split("<button")[1].split(">")[0],
  };
}

it("after a reconnect, volume, balance and mute wait for the new daemon's status", () => {
  // The old daemon's status is still there, but not from this connection.
  fixture.state.status = { ...fixture.state.status!, volume: { ...fixture.state.status!.volume!, volume_db: -12, balance_db: -2 } };
  fixture.state.volumeFresh = false;
  const c = controls();
  expect(c.vol).not.toContain("fine-knob");
  expect(c.vol).not.toContain("12.0");
  expect(c.bal).not.toContain("fine-knob");
  expect(c.bal).toMatch(/>-$/);
  expect(c.mute).toContain("disabled");
  expect(renderToStaticMarkup(createElement(VolumeControls))).toContain("Waiting for the engine");
  fixture.state.volumeFresh = true;
  const d = controls();
  expect(d.vol).toContain("fine-knob");
  expect(d.bal).toContain("fine-knob");
  expect(d.mute).not.toContain("disabled");
});

it("the hardware badge claims bit-perfect only while nothing is applied digitally", () => {
  const base = fixture.state.status!.volume!;
  fixture.state.status = { ...fixture.state.status!, volume: { ...base, hardware: true, volume_db: -20, hardware_db: -20, software_db: 0 } };
  let html = renderToStaticMarkup(createElement(VolumeControls));
  expect(html).toContain("stays bit-perfect");
  // -90 dB on a card that stops at -64 dB: -26 dB is digital.
  fixture.state.status = { ...fixture.state.status!, volume: { ...base, hardware: true, volume_db: -90, hardware_db: -64, software_db: -26, min_db: -120 } };
  html = renderToStaticMarkup(createElement(VolumeControls));
  expect(html).not.toContain("stays bit-perfect");
  expect(html).toContain("not bit-perfect");
  expect(html).toMatch(/[-\u2212]26\.0 dB digitally/);
});

it("a daemon without volume revisions: controls disabled, asking for an engine update", () => {
  const { instance: _i, revision: _r, ...legacy } = fixture.state.status!.volume!;
  fixture.state.status = { ...fixture.state.status!, volume: legacy };
  const html = renderToStaticMarkup(createElement(VolumeControls));
  expect(html).toMatch(/[Uu]pdate the engine/);
  expect(controls().vol).not.toContain("fine-knob");
  expect(controls().mute).toContain("disabled");
});

it("the hardware badge does not claim bit-perfect with digital balance", () => {
  const base = fixture.state.status!.volume!;
  fixture.state.status = { ...fixture.state.status!, volume: { ...base, hardware: true, hardware_db: -20, software_db: 0, volume_db: -20, balance_db: -2 } };
  const html = renderToStaticMarkup(createElement(VolumeControls));
  expect(html).not.toContain("stays bit-perfect");
  expect(html).toContain("not bit-perfect");
});

it("the hardware badge does not claim bit-perfect while muted", () => {
  const base = fixture.state.status!.volume!;
  fixture.state.status = { ...fixture.state.status!, volume: { ...base, hardware: true, hardware_db: -20, software_db: 0, volume_db: -20, balance_db: 0, muted: true } };
  const html = renderToStaticMarkup(createElement(VolumeControls));
  expect(html).not.toContain("stays bit-perfect");
  expect(html).toContain("master volume. Muted:");
});
