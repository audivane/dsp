// Closing the main window to the tray destroys its webview (see
// src-tauri/src/app.rs), so a page made again starts afresh. The view comes
// back from local storage like after a restart; the scroll position of the
// view is kept by the app process (`saveMainView` / `loadMainView`), so it
// returns too, but not after a real restart.

import { useEffect, type RefObject } from "react";
import type { MainViewMemory } from "../api/shell";
import { shellTransport } from "./shell";

/** Saves at most once per `delayMs` while scrolling; the last value always goes out. */
export function scrollKeeper(save: (m: MainViewMemory) => void, delayMs = 250) {
  let timer: ReturnType<typeof setTimeout> | null = null;
  let pending: MainViewMemory | null = null;
  const send = () => {
    timer = null;
    if (pending) save(pending);
    pending = null;
  };
  return {
    note(m: MainViewMemory) {
      pending = m;
      if (!timer) timer = setTimeout(send, delayMs);
    },
    flush() {
      if (timer) clearTimeout(timer);
      send();
    },
  };
}

/** Where to scroll `view` back to, when the memory is about it. */
export function restoreTop(m: MainViewMemory | null, view: string): number | null {
  if (!m || m.view !== view || !Number.isFinite(m.scroll_top) || m.scroll_top <= 0) return null;
  return m.scroll_top;
}

let restored = false;
/** The view area's place right now (set while the view area is mounted). */
let current: (() => MainViewMemory) | null = null;

/** Where the main view is now, for the close handshake (null before the
 *  view area is shown, e.g. on the engine-down screen). */
export function currentMainView(): MainViewMemory | null {
  return current ? current() : null;
}

/** Tests: a fresh page. */
export function resetMainViewMemory() {
  restored = false;
}

/** Keep `el`'s scroll position for `view`, and put it back once per page. */
export function useMainViewMemory(el: RefObject<HTMLElement | null>, view: string) {
  useEffect(() => {
    const t = shellTransport();
    const node = el.current;
    if (!t || !node || restored) return;
    restored = true;
    let cancelled = false;
    const stop = () => {
      cancelled = true;
    };
    // The user moving first wins over the restore.
    const opts = { passive: true, once: true } as const;
    node.addEventListener("wheel", stop, opts);
    node.addEventListener("pointerdown", stop, opts);
    node.addEventListener("keydown", stop, opts);
    void t.loadMainView().then((m) => {
      const top = restoreTop(m, view);
      if (top === null) return;
      // The view may still be filling in (plots, lists): try for a moment.
      const until = performance.now() + 1500;
      const step = () => {
        if (cancelled) return;
        node.scrollTop = top;
        if (Math.abs(node.scrollTop - top) > 1 && performance.now() < until) requestAnimationFrame(step);
      };
      step();
    });
    return () => {
      node.removeEventListener("wheel", stop);
      node.removeEventListener("pointerdown", stop);
      node.removeEventListener("keydown", stop);
    };
    // Once per page: the first view shown is the one restored.
  }, []);

  useEffect(() => {
    const t = shellTransport();
    const node = el.current;
    if (!t || !node) return;
    const keeper = scrollKeeper((m) => t.saveMainView(m));
    const here = () => ({ view, scroll_top: node.scrollTop });
    const onScroll = () => keeper.note(here());
    node.addEventListener("scroll", onScroll, { passive: true });
    current = here;
    return () => {
      node.removeEventListener("scroll", onScroll);
      if (current === here) current = null;
      keeper.flush();
    };
  }, [el, view]);
}
