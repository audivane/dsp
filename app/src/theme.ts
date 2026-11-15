// Skin choice (System / Midnight / Graphite / Light), persisted per viewer.

import { useSyncExternalStore } from "react";

export type Skin = "system" | "midnight" | "graphite" | "light";
export const SKINS: { id: Skin; label: string }[] = [
  { id: "system", label: "System" },
  { id: "midnight", label: "Midnight" },
  { id: "graphite", label: "Graphite" },
  { id: "light", label: "Light" },
];

const KEY = "avdsp-skin";
const listeners = new Set<() => void>();
let current: Skin = read();

function read(): Skin {
  try {
    const v = localStorage.getItem(KEY);
    if (v === "system" || v === "midnight" || v === "graphite" || v === "light") return v;
  } catch {
    /* ignore */
  }
  return "system";
}

function prefersDark(): boolean {
  try {
    return window.matchMedia("(prefers-color-scheme: dark)").matches;
  } catch {
    return true;
  }
}

export function resolvedSkin(s: Skin = current): Exclude<Skin, "system"> {
  return s === "system" ? (prefersDark() ? "midnight" : "light") : s;
}

function apply() {
  document.documentElement.setAttribute("data-skin", resolvedSkin());
  for (const l of listeners) l();
}

export function setSkin(s: Skin) {
  current = s;
  try {
    localStorage.setItem(KEY, s);
  } catch {
    /* ignore */
  }
  apply();
}

export function initTheme() {
  apply();
  try {
    window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
      if (current === "system") apply();
    });
  } catch {
    /* ignore */
  }
}

export function useSkin(): Skin {
  return useSyncExternalStore(
    (fn) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    () => current,
  );
}

/** Read a CSS custom property from the root (for canvas painters). */
export function cssVar(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}
