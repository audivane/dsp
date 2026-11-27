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

});
