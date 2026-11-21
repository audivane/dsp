// Shared mouse-wheel and touchpad support for sliders:
// - Normalises pixel, line and page wheel deltas into whole steps (one notch =
//   one step), accumulating fractional touchpad deltas until a notch completes.
// - Takes the wheel only when:
//     1. the slider's scroll container cannot scroll at all (hover works at once),
//     2. the slider has keyboard focus (clicked or tabbed to), or
//     3. the pointer has rested on the slider for at least 300 ms with no page
//        scroll in that time (restarting the timer whenever a page scroll happens).
// - Never intercepts Ctrl+wheel (browser zoom), and callers preventDefault only
//   when a step is actually applied.

export const WHEEL_NOTCH_PX = 100;
export const WHEEL_IDLE_MS = 350;
export const POINTER_REST_MS = 300;
export const SCROLL_GUARD_MS = POINTER_REST_MS;

export const WHEEL_HINT =
  "Wheel: adjusts after a short pause over the slider, after clicking it, or at once when the page doesn't scroll.";

/**
 * Convert a wheel event's vertical delta into pixel-equivalent notch units
 * where `WHEEL_NOTCH_PX` equals one discrete mouse-wheel notch.
 */
export function deltaToNotchPx(deltaY: number, deltaMode: number): number {
  if (!Number.isFinite(deltaY) || deltaY === 0) return 0;
  // DOM_DELTA_LINE (1): 3 lines is one standard notch.
  if (deltaMode === 1) return deltaY * (WHEEL_NOTCH_PX / 3);
  // DOM_DELTA_PAGE (2): 1 page is one notch.
  if (deltaMode === 2) return deltaY * WHEEL_NOTCH_PX;
  // DOM_DELTA_PIXEL (0): standard discrete wheels report ±100 (Chromium) or
  // exact multiples of ±120 (X11/WebKit/Windows); normalise exact 120-multiples
  // to one notch so five clicks never accumulate a phantom sixth step.
  if (Number.isInteger(deltaY) && deltaY % 120 === 0) {
    return (deltaY / 120) * WHEEL_NOTCH_PX;
  }
  return deltaY;
}

/**
 * Direction of a vertical wheel gesture: +1 for wheel-up (increase), -1 for
 * wheel-down (decrease), or 0 for a horizontal / zero gesture to ignore.
 */
export function wheelDirection(deltaY: number, deltaX = 0): -1 | 0 | 1 {
  if (!Number.isFinite(deltaY) || deltaY === 0) return 0;
  if (Number.isFinite(deltaX) && Math.abs(deltaX) > Math.abs(deltaY)) return 0;
  return deltaY < 0 ? 1 : -1;
}

/**
 * Accumulates fractional wheel deltas (touchpads, high-resolution mice) until
 * whole notches are reached. Resets leftover fraction on direction reversal or
 * after an idle gap.
 */
export class WheelAccumulator {
  private acc = 0;
  private lastAt = -Infinity;

  push(deltaY: number, deltaMode: number, now: number, deltaX = 0): { steps: number; dir: -1 | 0 | 1 } {
    const dir = wheelDirection(deltaY, deltaX);
    if (dir === 0) return { steps: 0, dir: 0 };
    const px = deltaToNotchPx(deltaY, deltaMode);
    if (px === 0) return { steps: 0, dir: 0 };
    const signed = -px; // wheel up (deltaY < 0) → positive step
    if (now - this.lastAt > WHEEL_IDLE_MS || (this.acc !== 0 && Math.sign(this.acc) !== dir)) {
      this.acc = 0;
    }
    this.lastAt = now;
    this.acc += signed;
    const steps = Math.trunc(this.acc / WHEEL_NOTCH_PX) || 0;
    if (steps !== 0) {
      this.acc -= steps * WHEEL_NOTCH_PX;
    }
    return { steps, dir };
  }

  reset() {
    this.acc = 0;
    this.lastAt = -Infinity;
  }
}

// ---------- ancestor scroll guard & wheel capture gate ----------

const recentScrolls = new WeakMap<object, number>();
let lastGlobalScrollAt = -Infinity;
let scrollListeners = 0;

/** Record that `target` scrolled at `at` ms (exposed for tests). */
export function recordScroll(target: EventTarget | null, at = performance.now()) {
  if (!target || (typeof document !== "undefined" && target === document) || (typeof window !== "undefined" && target === window)) {
    lastGlobalScrollAt = at;
  } else {
    recentScrolls.set(target, at);
  }
}

/** Reset global scroll timestamp (for tests). */
export function resetScrollGuard() {
  lastGlobalScrollAt = -Infinity;
}

function onAnyScroll(e: Event) {
  recordScroll(e.target, performance.now());
}

/**
 * Keep a capture-phase scroll listener active on `window` while any slider is
 * mounted. Capture phase sees non-bubbling `scroll` events from `.main` and
 * every other scrollable ancestor as well as the window itself.
 */
export function retainScrollGuard(): () => void {
  if (typeof window !== "undefined") {
    if (scrollListeners === 0) {
      window.addEventListener("scroll", onAnyScroll, { capture: true, passive: true });
    }
    scrollListeners++;
  }
  return () => {
    if (typeof window !== "undefined" && scrollListeners > 0) {
      scrollListeners--;
      if (scrollListeners === 0) {
        window.removeEventListener("scroll", onAnyScroll, { capture: true });
      }
    }
  };
}

/**
 * True if the document/window or any ancestor of `el` scrolled within the last
 * `guardMs` milliseconds.
 */
export function wasAncestorScrolledRecently(
  el: { parentNode: object | null } | null,
  now = performance.now(),
  guardMs = SCROLL_GUARD_MS,
): boolean {
  if (now - lastGlobalScrollAt < guardMs) return true;
  let cur: { parentNode?: object | null } | null = el ? (el.parentNode as { parentNode?: object | null } | null) : null;
  while (cur) {
    const at = recentScrolls.get(cur);
    if (at !== undefined && now - at < guardMs) return true;
    cur = (cur.parentNode ?? null) as { parentNode?: object | null } | null;
  }
  return false;
}

/**
 * True if any ancestor scroll container of `el` (including the document
 * scrolling element) can currently scroll vertically.
 */
export function hasScrollableAncestor(el: Element | null): boolean {
  if (!el || typeof getComputedStyle === "undefined") return false;
  let cur = el.parentElement;
  while (cur) {
    if (cur === document.documentElement) {
      const se = document.scrollingElement;
      if (se && se.scrollHeight > se.clientHeight + 1) return true;
    } else if (cur !== document.body) {
      const oy = getComputedStyle(cur).overflowY;
      if ((oy === "auto" || oy === "scroll" || oy === "overlay") && cur.scrollHeight > cur.clientHeight + 1) {
        return true;
      }
    }
    cur = cur.parentElement;
  }
  return false;
}

export interface WheelGateInput {
  focused: boolean;
  scrollableAncestor: boolean;
  restSince: number | null;
  ancestorScrolledRecently: boolean;
  now: number;
  restMs?: number;
}

/**
 * Pure decision for whether a slider may take a wheel gesture:
 * - focused (clicked or tabbed to), OR
 * - no scrollable ancestor container (takes wheel at once on hover), OR
 * - pointer rested on the slider for >= restMs with no ancestor scroll in that time.
 */
export function canSliderTakeWheel(opts: WheelGateInput): boolean {
  if (opts.focused) return true;
  if (!opts.scrollableAncestor) return true;
  if (opts.ancestorScrolledRecently) return false;
  const restMs = opts.restMs ?? POINTER_REST_MS;
  return opts.restSince !== null && opts.now - opts.restSince >= restMs;
}

/**
 * Attach pointer-rest tracking and the ancestor scroll guard to a slider
 * element. Returns `{ canTakeWheel(now), dispose() }`.
 */
export function attachSliderWheelGate(el: HTMLElement): {
  canTakeWheel: (now: number) => boolean;
  dispose: () => void;
} {
  const releaseScroll = retainScrollGuard();
  let restSince: number | null = null;
  const onEnter = () => {
    restSince = performance.now();
  };
  const onMouseEnter = () => {
    if (restSince === null) restSince = performance.now();
  };
  const onMove = () => {
    restSince = performance.now();
  };
  const onLeave = () => {
    restSince = null;
  };
  el.addEventListener("pointerenter", onEnter, { passive: true });
  el.addEventListener("mouseenter", onMouseEnter, { passive: true });
  el.addEventListener("pointermove", onMove, { passive: true });
  el.addEventListener("pointerleave", onLeave, { passive: true });
  return {
    canTakeWheel(now: number) {
      const active = typeof document !== "undefined" ? document.activeElement : null;
      const focused = active === el || (!!active && el.contains(active));
      return canSliderTakeWheel({
        focused,
        scrollableAncestor: hasScrollableAncestor(el),
        restSince,
        ancestorScrolledRecently: wasAncestorScrolledRecently(el, now, POINTER_REST_MS),
        now,
      });
    },
    dispose() {
      el.removeEventListener("pointerenter", onEnter);
      el.removeEventListener("mouseenter", onMouseEnter);
      el.removeEventListener("pointermove", onMove);
      el.removeEventListener("pointerleave", onLeave);
      releaseScroll();
    },
  };
}
