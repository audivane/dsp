import { useCallback, useEffect, useState } from "react";

export const MAX_PLOT_HEIGHT = 6000;
export const EQ_MIN_HEIGHT = 160;
export const UPS_TRANS_MIN_HEIGHT = 160;
export const UPS_TRANS_DEFAULT_HEIGHT = 192;
export const UPS_IMPULSE_MIN_HEIGHT = 120;
export const UPS_IMPULSE_DEFAULT_HEIGHT = 160;

export const PLOT_HEIGHT_KEYS = {
  eq: "avdsp-height-eq",
  upsTrans: "avdsp-height-ups-trans",
  upsImpulse: "avdsp-height-ups-impulse",
} as const;

export function clampPlotHeight(height: number, min: number, max = MAX_PLOT_HEIGHT): number {
  const lo = Math.max(1, Math.round(min));
  const hi = Math.max(lo, Math.round(max));
  if (!Number.isFinite(height)) return lo;
  return Math.round(Math.max(lo, Math.min(hi, height)));
}

export function loadPlotHeight(storageKey: string, min: number, max = MAX_PLOT_HEIGHT): number | null {
  try {
    const raw = localStorage.getItem(storageKey);
    if (raw === null || raw.trim() === "") return null;
    const n = Number(raw);
    if (!Number.isFinite(n) || n <= 0) return null;
    return clampPlotHeight(n, min, max);
  } catch {
    return null;
  }
}

export function savePlotHeight(storageKey: string, height: number | null, min: number, max = MAX_PLOT_HEIGHT): void {
  try {
    if (height === null) {
      localStorage.removeItem(storageKey);
    } else {
      localStorage.setItem(storageKey, String(clampPlotHeight(height, min, max)));
    }
  } catch {
    /* storage unavailable */
  }
}

export interface HeightModel {
  readonly height: number | null;
  readonly min: number;
  readonly max: number;
  readonly defaultHeight: number | null;
  set(next: number): number;
  commit(next: number): number;
  step(delta: number, fallbackCurrent?: number): number;
  reset(): number | null;
}

/**
 * Pure controller for persistent plot heights (used by `usePersistentHeight`
 * and directly tested in Vitest).
 */
export function createHeightModel(opts: {
  storageKey: string;
  defaultHeight: number | null;
  minHeight: number;
  maxHeight?: number;
}): HeightModel {
  const max = opts.maxHeight ?? MAX_PLOT_HEIGHT;
  const min = opts.minHeight;
  const clampedDefault = opts.defaultHeight === null ? null : clampPlotHeight(opts.defaultHeight, min, max);
  let current: number | null = loadPlotHeight(opts.storageKey, min, max) ?? clampedDefault;

  return {
    get height() {
      return current;
    },
    min,
    max,
    defaultHeight: clampedDefault,
    set(next: number) {
      const clamped = clampPlotHeight(next, min, max);
      current = clamped;
      return clamped;
    },
    commit(next: number) {
      const clamped = clampPlotHeight(next, min, max);
      current = clamped;
      savePlotHeight(opts.storageKey, clamped, min, max);
      return clamped;
    },
    step(delta: number, fallbackCurrent?: number) {
      const base = current ?? fallbackCurrent ?? clampedDefault ?? min;
      const clamped = clampPlotHeight(base + delta, min, max);
      current = clamped;
      savePlotHeight(opts.storageKey, clamped, min, max);
      return clamped;
    },
    reset() {
      current = clampedDefault;
      savePlotHeight(opts.storageKey, null, min, max);
      return clampedDefault;
    },
  };
}

export interface PersistentHeightState {
  height: number | null;
  minHeight: number;
  maxHeight: number;
  defaultHeight: number | null;
  setHeight: (next: number) => void;
  commitHeight: (next: number) => void;
  stepHeight: (delta: number, fallbackCurrent?: number) => void;
  resetHeight: () => void;
}

export function usePersistentHeight(opts: {
  storageKey: string;
  defaultHeight: number | null;
  minHeight: number;
  maxHeight?: number;
}): PersistentHeightState {
  const { storageKey, defaultHeight, minHeight, maxHeight = MAX_PLOT_HEIGHT } = opts;
  const clampedDefault = defaultHeight === null ? null : clampPlotHeight(defaultHeight, minHeight, maxHeight);
  const [height, setHeightState] = useState<number | null>(
    () => loadPlotHeight(storageKey, minHeight, maxHeight) ?? clampedDefault,
  );

  useEffect(() => {
    setHeightState(loadPlotHeight(storageKey, minHeight, maxHeight) ?? clampedDefault);
  }, [storageKey, minHeight, maxHeight, clampedDefault]);

  const setHeight = useCallback(
    (next: number) => {
      setHeightState(clampPlotHeight(next, minHeight, maxHeight));
    },
    [minHeight, maxHeight],
  );

  const commitHeight = useCallback(
    (next: number) => {
      const clamped = clampPlotHeight(next, minHeight, maxHeight);
      setHeightState(clamped);
      savePlotHeight(storageKey, clamped, minHeight, maxHeight);
    },
    [storageKey, minHeight, maxHeight],
  );

  const stepHeight = useCallback(
    (delta: number, fallbackCurrent?: number) => {
      setHeightState((prev) => {
        const base = prev ?? fallbackCurrent ?? clampedDefault ?? minHeight;
        const clamped = clampPlotHeight(base + delta, minHeight, maxHeight);
        savePlotHeight(storageKey, clamped, minHeight, maxHeight);
        return clamped;
      });
    },
    [storageKey, minHeight, maxHeight, clampedDefault],
  );

  const resetHeight = useCallback(() => {
    setHeightState(clampedDefault);
    savePlotHeight(storageKey, null, minHeight, maxHeight);
  }, [storageKey, clampedDefault, minHeight, maxHeight]);

  return {
    height,
    minHeight,
    maxHeight,
    defaultHeight: clampedDefault,
    setHeight,
    commitHeight,
    stepHeight,
    resetHeight,
  };
}
