import { describe, expect, it } from "vitest";
import { throttleLatest, type Clock } from "./throttle";

function fakeClock() {
  let t = 0;
  let timers: { at: number; fn: () => void; id: number }[] = [];
  let next = 1;
  const clock: Clock = {
    now: () => t,
    setTimeout: (fn, ms) => {
      const id = next++;
      timers.push({ at: t + ms, fn, id });
      return id;
    },
    clearTimeout: (h) => {
      timers = timers.filter((x) => x.id !== h);
    },
  };
  const advance = (ms: number) => {
    const end = t + ms;
    for (;;) {
      timers.sort((a, b) => a.at - b.at);
      const due = timers[0];
      if (!due || due.at > end) break;
      timers.shift();
      t = due.at;
      due.fn();
    }
    t = end;
  };
  return { clock, advance };
}

describe("throttleLatest", () => {

  it("flush sends the final value immediately and only once", () => {
    const { clock, advance } = fakeClock();
    const sent: number[] = [];
    const th = throttleLatest((v: number) => sent.push(v), 40, clock);
    th.push(1);
    th.push(2);
    expect(th.pending).toBe(true);
    th.flush();
    expect(sent).toEqual([1, 2]);
    expect(th.pending).toBe(false);
    advance(100);
    expect(sent).toEqual([1, 2]);
    th.flush();
    expect(sent).toEqual([1, 2]);
  });

});
