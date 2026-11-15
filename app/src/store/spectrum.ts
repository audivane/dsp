// Live spectrum data lives outside the React store: it arrives at ~10-20 Hz
// and is read by the canvas painter in requestAnimationFrame.

import type { Spectrum } from "../api/types";

export interface SpectrumFrame {
  spectrum: Spectrum;
  at: number;
}

class SpectrumStore {
  latest: SpectrumFrame | null = null;
  private listeners = new Set<() => void>();
  private clearListeners = new Set<() => void>();

  push(s: Spectrum, now: number) {
    if (this.latest && this.latest.spectrum.rate !== s.rate) {
      this.clear();
    }
    this.latest = { spectrum: s, at: now };
    for (const fn of this.listeners) fn();
  }

  onFrame(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  onClear(fn: () => void): () => void {
    this.clearListeners.add(fn);
    return () => this.clearListeners.delete(fn);
  }

  clear() {
    this.latest = null;
    for (const fn of this.clearListeners) fn();
  }
}

export const spectrumStore = new SpectrumStore();
