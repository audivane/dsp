import { afterEach, describe, expect, it } from "vitest";
import { MockDaemon, MockTransport } from "../api/mock";
import { AppStore, clearStash } from "./store";

const until = async (pred: () => boolean, ms = 2000) => {
  const t = Date.now();
  while (!pred()) {
    if (Date.now() - t > ms) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 5));
  }
};

class SpyTransport extends MockTransport {
  calls: { method: string; params: unknown }[] = [];
  override async call<T>(method: string, params?: unknown): Promise<T> {
    this.calls.push({ method, params });
    return super.call<T>(method, params);
  }
  of(method: string) {
    return this.calls.filter((c) => c.method === method).map((c) => c.params);
  }
}

let stores: AppStore[] = [];
async function setup(followers?: string[]) {
  const daemon = new MockDaemon({ followers });
  const t = new SpyTransport(daemon);
  const s = new AppStore(t);
  stores.push(s);
  s.connect();
  await until(() => s.getState().conn === "up" && !!s.getState().config && !!s.getState().status);
  await until(() => s.getState().caps.devices !== null);
  return { s, t, daemon };
}

afterEach(() => {
  for (const s of stores) s.disconnect();
  stores = [];
  clearStash();
});

const USB = "hw:CARD=USB,DEV=0";
const PCH = "hw:CARD=PCH,DEV=0";

describe("followers", () => {
  it("adds, edits and removes a follower with set_follower", async () => {
    const { s, t, daemon } = await setup();
    s.addFollower(USB);
    await s.whenIdle();
    expect(t.of("set_follower")).toEqual([{ device: USB }]);
    expect(s.followers()).toEqual([{ device: USB, enabled: true, trim_ms: 0, volume_db: 0 }]);
    expect(s.getState().status?.outputs.followers[0].state).toBe("opening");

    s.editFollower(USB, { trim_ms: 1.5 });
    s.editFollower(USB, { enabled: false });
    await s.whenIdle();
    expect(daemon.config.outputs.followers[0]).toMatchObject({ trim_ms: 1.5, enabled: false });
    expect(s.getState().followerDrafts).toEqual({});

    s.removeFollower(USB);
    await s.whenIdle();
    expect(t.of("set_follower").at(-1)).toEqual({ device: USB, remove: true });
    expect(s.followers()).toEqual([]);
  });

  it("shows a refusal next to the follower and drops the draft", async () => {
    const { s } = await setup(["USB"]);
    s.editFollower(USB, { trim_ms: 80 });
    await s.whenIdle();
    expect(s.getState().followerErrors[USB]).toMatch(/trim_ms must be between -50 and 50/);
    expect(s.getState().followerDrafts).toEqual({});
    expect(s.followers()[0].trim_ms).toBe(0);
    s.editFollower(USB, { trim_ms: 3 });
    await s.whenIdle();
    expect(s.getState().followerErrors[USB]).toBeUndefined();
  });

  it("refuses a fourth follower in the add slot", async () => {
    const { s } = await setup(["USB", "PCH", "SPK"]);
    s.addFollower("hw:CARD=HDMI,DEV=3");
    await s.whenIdle();
    expect(s.getState().followerErrors[""]).toMatch(/at most 3 followers/);
    expect(s.followers()).toHaveLength(3);
  });

  it("holds a drag as a draft and sends the last value", async () => {
    const { s, t } = await setup(["USB"]);
    for (const v of [-1, -2, -3, -4]) s.editFollower(USB, { volume_db: v }, "drag");
    expect(s.getState().followerDrafts[USB]).toEqual({ volume_db: -4 });
    s.endFollowerDrag(USB);
    await s.whenIdle();
    expect(s.followers()[0].volume_db).toBe(-4);
    expect(t.of("set_follower").length).toBeLessThan(4);
    await until(() => !(USB in s.getState().followerDrafts));
  });

  it("will not move the main device onto a follower, and offers to remove it first", async () => {
    const { s, t, daemon } = await setup(["USB"]);
    s.setView("device");
    await s.setDevice(USB);
    expect(s.getState().deviceConflict).toBe(USB);
    expect(t.of("set_device")).toEqual([]);
    await s.moveDeviceToFollower(USB);
    expect(daemon.config.device).toBe(USB);
    expect(daemon.config.outputs.followers).toEqual([]);
    expect(s.getState().deviceConflict).toBeNull();
  });

  it("walks the status from locking to in sync, muted while disabled", async () => {
    const { s, daemon } = await setup();
    let now = 1_000_000;
    daemon.followerClock = () => now;
    s.addFollower(PCH);
    await s.whenIdle();
    now += 1000;
    daemon.snapshot(["status"]);
    await until(() => s.getState().status?.outputs.followers[0]?.state === "locking");
    now += 5000;
    daemon.snapshot(["status"]);
    await until(() => s.getState().status?.outputs.followers[0]?.state === "locked");
    expect(s.getState().status?.outputs.alignment_ms).toBeGreaterThan(0);
    s.editFollower(PCH, { enabled: false });
    await s.whenIdle();
    await until(() => s.getState().status?.outputs.followers[0]?.state === "muted");
  });

  it("refuses varispeed past 1.996× while followers play", async () => {
    const { s, daemon } = await setup(["USB"]);
    s.editVarispeed({ enabled: true, speed: 1.998 });
    await s.whenIdle();
    expect(s.getState().varispeedError).toMatch(/at most 1.996 while followers play/);
    expect(daemon.config.varispeed.speed).toBe(1);
  });
});
