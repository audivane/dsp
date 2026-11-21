import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it } from "vitest";
import {
  MAX_PLOT_HEIGHT,
  clampPlotHeight,
  createHeightModel,
  loadPlotHeight,
  savePlotHeight,
  usePersistentHeight,
} from "./plotHeight";

function withLocalStorage(mock: Partial<Storage> | null, fn: () => void) {
  const prev = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    value: mock,
  });
  try {
    fn();
  } finally {
    if (prev) Object.defineProperty(globalThis, "localStorage", prev);
    else delete (globalThis as Record<string, unknown>).localStorage;
  }
}

function makeMemoryStorage() {
  const map = new Map<string, string>();
  return {
    map,
    storage: {
      getItem: (k: string) => map.get(k) ?? null,
      setItem: (k: string, v: string) => {
        map.set(k, v);
      },
      removeItem: (k: string) => {
        map.delete(k);
      },
    },
  };
}

describe("plotHeight", () => {
  afterEach(() => {
    // Ensure globalThis.localStorage is restored even if a test fails unexpectedly.
  });

  it("clamps heights to [min, max] and rejects non-finite values", () => {
    expect(clampPlotHeight(100, 160)).toBe(160);
    expect(clampPlotHeight(160, 160)).toBe(160);
    expect(clampPlotHeight(350.6, 160)).toBe(351);
    expect(clampPlotHeight(99999, 160)).toBe(MAX_PLOT_HEIGHT);
    expect(clampPlotHeight(3000, 120, 4000)).toBe(3000);
    expect(clampPlotHeight(Number.NaN, 120)).toBe(120);
    expect(clampPlotHeight(Number.POSITIVE_INFINITY, 120)).toBe(120);
  });

  it("loads, validates, and persists heights in localStorage", () => {
    const { map, storage } = makeMemoryStorage();
    withLocalStorage(storage, () => {
      expect(loadPlotHeight("k", 160)).toBeNull();

      savePlotHeight("k", 420.4, 160);
      expect(map.get("k")).toBe("420");
      expect(loadPlotHeight("k", 160)).toBe(420);

      // Clamps out-of-range stored values.
      map.set("k", "40");
      expect(loadPlotHeight("k", 160)).toBe(160);
      map.set("k", "50000");
      expect(loadPlotHeight("k", 160)).toBe(MAX_PLOT_HEIGHT);

      // Invalid stored values fall back to null.
      for (const bad of ["", "   ", "abc", "NaN", "0", "-50", "Infinity"]) {
        map.set("k", bad);
        expect(loadPlotHeight("k", 160)).toBeNull();
      }

      // Null clears storage.
      savePlotHeight("k", null, 160);
      expect(map.has("k")).toBe(false);
    });
  });

  it("handles localStorage failures gracefully without throwing", () => {
    const throwingStorage: Partial<Storage> = {
      getItem: () => {
        throw new Error("SecurityError: storage disabled");
      },
      setItem: () => {
        throw new Error("QuotaExceededError");
      },
      removeItem: () => {
        throw new Error("SecurityError: storage disabled");
      },
    };

    withLocalStorage(throwingStorage, () => {
      expect(loadPlotHeight("avdsp-height-eq", 160)).toBeNull();
      expect(() => savePlotHeight("avdsp-height-eq", 300, 160)).not.toThrow();
      expect(() => savePlotHeight("avdsp-height-eq", null, 160)).not.toThrow();

      const model = createHeightModel({
        storageKey: "avdsp-height-ups-trans",
        defaultHeight: 192,
        minHeight: 160,
      });
      expect(model.height).toBe(192);
      expect(model.commit(500)).toBe(500);
      expect(model.height).toBe(500);
      expect(model.step(-400)).toBe(160);
      expect(model.reset()).toBe(192);
      expect(model.height).toBe(192);
    });
  });

  it("supports commit, keyboard stepping, clamping, and reset to default", () => {
    const { map, storage } = makeMemoryStorage();
    withLocalStorage(storage, () => {
      // Fixed default (upsampling plot).
      const ups = createHeightModel({
        storageKey: "avdsp-height-ups-impulse",
        defaultHeight: 140,
        minHeight: 120,
      });
      expect(ups.height).toBe(140);
      expect(ups.step(20)).toBe(160);
      expect(map.get("avdsp-height-ups-impulse")).toBe("160");
      expect(ups.step(100)).toBe(260);
      expect(ups.step(-200)).toBe(120);
      expect(ups.reset()).toBe(140);
      expect(ups.height).toBe(140);
      expect(map.has("avdsp-height-ups-impulse")).toBe(false);

      // Flex default (EQ graph, defaultHeight = null).
      const eq = createHeightModel({
        storageKey: "avdsp-height-eq",
        defaultHeight: null,
        minHeight: 160,
      });
      expect(eq.height).toBeNull();
      // Stepping from measured flex height of 520 px.
      expect(eq.step(100, 520)).toBe(620);
      expect(map.get("avdsp-height-eq")).toBe("620");
      // Resetting returns to null (flex default) and clears storage.
      expect(eq.reset()).toBeNull();
      expect(eq.height).toBeNull();
      expect(map.has("avdsp-height-eq")).toBe(false);
    });
  });

  it("initializes usePersistentHeight in React with stored or fallback height", () => {
    function Probe({ storageKey, defaultHeight, minHeight }: { storageKey: string; defaultHeight: number | null; minHeight: number }) {
      const h = usePersistentHeight({ storageKey, defaultHeight, minHeight });
      return createElement("div", { "data-h": String(h.height) });
    }

    const { map, storage } = makeMemoryStorage();
    map.set("avdsp-height-ups-trans", "80");
    withLocalStorage(storage, () => {
      const htmlClamped = renderToStaticMarkup(
        createElement(Probe, { storageKey: "avdsp-height-ups-trans", defaultHeight: 192, minHeight: 160 }),
      );
      expect(htmlClamped).toContain('data-h="160"');

      const htmlDefault = renderToStaticMarkup(
        createElement(Probe, { storageKey: "avdsp-height-eq", defaultHeight: null, minHeight: 160 }),
      );
      expect(htmlDefault).toContain('data-h="null"');
    });

    withLocalStorage(
      {
        getItem: () => {
          throw new Error("blocked");
        },
      },
      () => {
        const htmlFallback = renderToStaticMarkup(
          createElement(Probe, { storageKey: "avdsp-height-ups-trans", defaultHeight: 192, minHeight: 160 }),
        );
        expect(htmlFallback).toContain('data-h="192"');
      },
    );
  });
});
