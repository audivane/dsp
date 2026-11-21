import { afterEach, describe, expect, it } from "vitest";
import { MockDaemon, MockTransport } from "../api/mock";
import { DaemonError, ErrorCode } from "../api/transport";
import type { StreamInfo } from "../api/types";
import { STREAM_VOLUME_NEEDS_UPDATE, reportsStreamLevels, streamDbText, streamLimitNote, streamVolumeBlock } from "../lib/streamVolume";
import { AppStore, clearStash } from "./store";

const until = async (pred: () => boolean, ms = 2000) => {
  const t = Date.now();
  while (!pred()) {
    if (Date.now() - t > ms) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 5));
  }
};

/** Records the `set_stream_volume` calls; can refuse the method. */
class Spy extends MockTransport {
  sent: unknown[] = [];
  unknownMethod = false;
  /** The next `set_stream_volume` answer waits for this (the call itself
   *  runs at once). */
  holdNextReply: Promise<void> | null = null;
  override async call<T>(method: string, params?: unknown): Promise<T> {
    if (method === "set_stream_volume") {
      this.sent.push(params);
      if (this.unknownMethod) throw new DaemonError(ErrorCode.INVALID_REQUEST, `unknown method "${method}"`);
      const hold = this.holdNextReply;
      this.holdNextReply = null;
      const r = await super.call<T>(method, params);
      if (hold) await hold;
      return r;
    }
    return super.call<T>(method, params);
  }
}

let stores: AppStore[] = [];
async function setup(opts: { legacy?: boolean } = {}) {
  const daemon = new MockDaemon({ legacyStreamVolume: opts.legacy });
  const t = new Spy(daemon);
  const s = new AppStore(t);
  stores.push(s);
  s.connect();
  await until(() => s.getState().conn === "up" && !!s.getState().config && s.getState().streams.length > 0);
  return { s, t, daemon };
}

afterEach(() => {
  for (const s of stores) s.disconnect();
  stores = [];
  clearStash();
});

const stream = (s: AppStore, node: number) => s.getState().streams.find((x) => x.node === node)!;

describe("stream volume", () => {
  it("learns the engine has it from volume_db", async () => {
    const { s } = await setup();
    expect(s.getState().caps.streamVolume).toBe(true);
  });

  it("sets a level, shows the draft at once and the answer after", async () => {
    const { s, t, daemon } = await setup();
    s.setStreamLevel(81, { volume_db: -9.5 });
    // The draft shows before the answer.
    expect(s.streamLevel(stream(s, 81)).db).toBe(-9.5);
    await until(() => Object.keys(s.getState().streamDrafts).length === 0);
    expect(t.sent).toEqual([{ node: 81, volume_db: -9.5 }]);
    expect(daemon.streams.find((x) => x.node === 81)?.volume_db).toBe(-9.5);
    expect(stream(s, 81).volume_db).toBe(-9.5);
    expect(s.getState().streamMessages[81]).toBeUndefined();
  });

  it("throttles a drag and always sends the final value", async () => {
    const { s, t } = await setup();
    for (let i = 1; i <= 30; i++) s.setStreamLevel(81, { volume_db: -i * 0.5 }, "drag");
    // Still dragging: the draft stays up.
    expect(s.streamLevel(stream(s, 81)).db).toBe(-15);
    s.setStreamLevel(81, { volume_db: -15 }, "commit");
    await until(() => Object.keys(s.getState().streamDrafts).length === 0);
    expect(t.sent.length).toBeLessThan(10);
    expect(t.sent.at(-1)).toEqual({ node: 81, volume_db: -15 });
    expect(stream(s, 81).volume_db).toBe(-15);
  });

  it("mutes without touching the level, and merges a level and a mute", async () => {
    const { s, t } = await setup();
    s.setStreamLevel(81, { muted: true });
    await until(() => stream(s, 81).muted);
    expect(t.sent).toEqual([{ node: 81, muted: true }]);
    expect(stream(s, 81).volume_db).toBe(0);
    s.setStreamLevel(81, { volume_db: -3 }, "drag");
    s.setStreamLevel(81, { muted: false }, "commit");
    await until(() => Object.keys(s.getState().streamDrafts).length === 0);
    expect(stream(s, 81)).toMatchObject({ volume_db: -3, muted: false });
  });

  it("says when the app stream cap held the level", async () => {
    const { s, daemon } = await setup();
    daemon.config.volume.guard.stream_max_db = -18;
    s.setStreamLevel(81, { volume_db: -6 });
    await until(() => !!s.getState().streamMessages[81]);
    expect(stream(s, 81).volume_db).toBe(-18);
    expect(s.getState().streamMessages[81]).toEqual({ text: "Held at −18.0 dB: the app stream cap (Device, Volume settings).", error: false });
    expect(s.getState().status?.volume?.blocked ?? daemon.volume.blocked).toBeGreaterThan(0);
    // A level that is met clears the note.
    s.setStreamLevel(81, { volume_db: -24 });
    await until(() => !s.getState().streamMessages[81]);
  });

  it("shows the engine's refusal beside the stream and drops the draft", async () => {
    const { s } = await setup();
    // Firefox is not on our sinks.
    s.setStreamLevel(94, { volume_db: -6 });
    await until(() => !!s.getState().streamMessages[94]);
    const m = s.getState().streamMessages[94];
    expect(m.error).toBe(true);
    expect(m.text).toMatch(/^Stream 94 \(Firefox\) is not playing on an Audivane sink/);
    expect(s.getState().streamDrafts[94]).toBeUndefined();
    expect(stream(s, 94).volume_db).toBe(0);
    s.clearStreamMessage(94);
    expect(s.getState().streamMessages[94]).toBeUndefined();
  });

  it("an engine without the call: no volume_db, and an unknown method turns it off", async () => {
    const { s, t } = await setup({ legacy: true });
    expect(stream(s, 81).volume_db).toBeUndefined();
    expect(s.getState().caps.streamVolume).toBe(null);
    t.unknownMethod = true;
    s.setStreamLevel(81, { volume_db: -6 });
    await until(() => s.getState().caps.streamVolume === false);
    expect(s.getState().streamMessages[81]).toEqual({ text: STREAM_VOLUME_NEEDS_UPDATE, error: true });
    // Nothing more is sent.
    s.setStreamLevel(81, { volume_db: -7 });
    await new Promise((r) => setTimeout(r, 50));
    expect(t.sent).toHaveLength(1);
  });

  it("an answer older than a newer streams event does not overwrite it", async () => {
    const { s, t } = await setup();
    let release!: () => void;
    t.holdNextReply = new Promise<void>((r) => (release = r));
    s.setStreamLevel(81, { volume_db: -12 });
    await until(() => t.sent.length === 1);
    // Another client sets -20 while our answer (-12) is on its way.
    await t.call("set_stream_volume", { node: 81, volume_db: -20 });
    await until(() => stream(s, 81).volume_db === -20);
    release();
    await until(() => Object.keys(s.getState().streamDrafts).length === 0);
    await new Promise((r) => setTimeout(r, 20));
    expect(stream(s, 81).volume_db).toBe(-20);
    expect(s.streamLevel(stream(s, 81)).db).toBe(-20);
  });

  it("forgets drafts and messages of streams that are gone", async () => {
    const { s, daemon } = await setup();
    s.setStreamLevel(94, { volume_db: -6 });
    await until(() => !!s.getState().streamMessages[94]);
    daemon.streams = daemon.streams.filter((x) => x.node !== 94);
    await s.routeStream(stream(s, 81), "dsp");
    await until(() => !s.getState().streams.some((x) => x.node === 94));
    expect(s.getState().streamMessages[94]).toBeUndefined();
  });
});

describe("stream volume helpers", () => {
  const base: StreamInfo = { node: 1, app_name: "A", binary: null, media_name: null, rate: null, channels: null, format: null, sink: "dsp", volume: 1, muted: false, volume_db: -6 };

  it("says why a stream cannot be set", () => {
    expect(streamVolumeBlock(base, true)).toBeNull();
    expect(streamVolumeBlock(base, null)).toBeNull();
    expect(streamVolumeBlock(base, false)).toBe(STREAM_VOLUME_NEEDS_UPDATE);
    expect(streamVolumeBlock({ ...base, sink: null }, true)).toMatch(/through Audivane/);
    expect(streamVolumeBlock({ ...base, sink: "default" }, true)).toMatch(/through Audivane/);
    const { volume_db: _, ...old } = base;
    expect(streamVolumeBlock(old, null)).toBe(STREAM_VOLUME_NEEDS_UPDATE);
    expect(streamVolumeBlock(old, true)).toMatch(/not reported/);
    expect(reportsStreamLevels([old])).toBe(false);
    expect(reportsStreamLevels([old, base])).toBe(true);
  });

  it("notes a held level and formats dB only", () => {
    expect(streamLimitNote({ node: 1, volume_db: -6, muted: false, max_db: 0 })).toBeNull();
    expect(streamLimitNote({ node: 1, volume_db: -12, muted: false, max_db: -12, limited_by: "app_volume" })).toMatch(/app's own volume/);
    expect(streamLimitNote({ node: 1, volume_db: -12, muted: false, max_db: -12, limited_by: "stream_max" })).toMatch(/^Held at −12\.0 dB: the app stream cap/);
    expect(streamDbText(null)).toBe("—");
    expect(streamDbText(-3.25)).toBe("−3.3 dB");
    expect(streamDbText(0)).toBe("0.0 dB");
    expect(streamDbText(-120)).toBe("−∞ dB");
  });
});
