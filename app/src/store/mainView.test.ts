import { afterEach, describe, expect, it, vi } from "vitest";
import type { MainViewMemory } from "../api/shell";
import { restoreTop, scrollKeeper } from "./mainView";

describe("main view memory", () => {
  afterEach(() => vi.useRealTimers());

  it("saves at most once per delay while scrolling, and the last position always", () => {
    vi.useFakeTimers();
    const saved: MainViewMemory[] = [];
    const k = scrollKeeper((m) => saved.push(m), 250);
    k.note({ view: "eq", scroll_top: 10 });
    k.note({ view: "eq", scroll_top: 20 });
    expect(saved).toEqual([]);
    vi.advanceTimersByTime(250);
    expect(saved).toEqual([{ view: "eq", scroll_top: 20 }]);
    k.note({ view: "eq", scroll_top: 30 });
    // Leaving the view (or the page) sends what is pending at once, once.
    k.flush();
    k.flush();
    vi.advanceTimersByTime(1000);
    expect(saved).toEqual([
      { view: "eq", scroll_top: 20 },
      { view: "eq", scroll_top: 30 },
    ]);
  });

  it("restores only the view it was kept for", () => {
    expect(restoreTop(null, "eq")).toBeNull();
    expect(restoreTop({ view: "ups", scroll_top: 400 }, "eq")).toBeNull();
    expect(restoreTop({ view: "eq", scroll_top: 0 }, "eq")).toBeNull();
    expect(restoreTop({ view: "eq", scroll_top: Number.NaN }, "eq")).toBeNull();
    expect(restoreTop({ view: "ups", scroll_top: 400 }, "ups")).toBe(400);
  });
});
