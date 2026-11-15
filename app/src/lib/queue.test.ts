import { describe, expect, it } from "vitest";
import { MutationQueue } from "./queue";

const tick = (ms = 1) => new Promise((r) => setTimeout(r, ms));

describe("MutationQueue", () => {
  it("runs jobs one at a time, in order", async () => {
    const q = new MutationQueue();
    const log: string[] = [];
    const job = (name: string, ms: number) => async () => {
      log.push(`${name}>`);
      await tick(ms);
      log.push(`<${name}`);
      return name;
    };
    const a = q.run(job("a", 10));
    const b = q.run(job("b", 1));
    expect(q.idle).toBe(false);
    expect(await Promise.all([a, b])).toEqual(["a", "b"]);
    expect(log).toEqual(["a>", "<a", "b>", "<b"]);
    await q.drained();
    expect(q.idle).toBe(true);
  });

  it("a failing job does not stop the queue", async () => {
    const q = new MutationQueue();
    const a = q.run(async () => {
      throw new Error("no");
    });
    const b = q.run(async () => 2);
    await expect(a).rejects.toThrow("no");
    expect(await b).toBe(2);
  });

  it("latest() folds values into the waiting job and runs it with the newest", async () => {
    const q = new MutationQueue();
    const sent: number[] = [];
    const send = async (v: number) => {
      sent.push(v);
      await tick(5);
    };
    void q.latest("v", 1, send); // starts at once
    await tick(0);
    void q.latest("v", 2, send); // waits behind 1
    void q.latest("v", 3, send); // replaces 2
    expect(q.waiting("v")).toBe(1);
    await q.latest("v", 4, send); // replaces 3, resolves when sent
    expect(sent).toEqual([1, 4]);
  });

  it("latest() merges with a merge function", async () => {
    const q = new MutationQueue();
    const sent: string[] = [];
    void q.run(() => tick(5));
    void q.latest("s", "a", async (v) => void sent.push(v), (x, y) => x + y);
    await q.latest("s", "b", async (v) => void sent.push(v), (x, y) => x + y);
    expect(sent).toEqual(["ab"]);
  });

  it("another job seals the waiting one: later values queue after it", async () => {
    const q = new MutationQueue();
    const log: string[] = [];
    void q.run(() => tick(5));
    void q.latest("v", 1, async (v) => void log.push(`v${v}`));
    void q.run(async () => void log.push("other"));
    await q.latest("v", 2, async (v) => void log.push(`v${v}`));
    expect(log).toEqual(["v1", "other", "v2"]);
  });
});
