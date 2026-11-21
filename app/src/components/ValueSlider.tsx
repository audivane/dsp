// A slider for a stage setting in its own unit (Hz, cents, ms, dB): a native
// range input (arrows and mouse wheel step, Page Up/Down jump, Home/End) with
// an optional log scale, and a readout you can double-click to type a value.
// While it moves it reports "drag" (the store throttles and shows a draft);
// the release, key-up, wheel pause or a typed value reports "commit", which
// always goes out.

import { useEffect, useRef, useState } from "react";
import { parseNumber } from "../lib/format";
import {
  WHEEL_HINT,
  WHEEL_IDLE_MS,
  WheelAccumulator,
  attachSliderWheelGate,
  wheelDirection,
} from "./sliderWheel";
import { keyStepValue, stepValue } from "./valueSliderKeys";

const STEPS = 1000;

export interface ValueSliderProps {
  id?: string;
  /** Accessible name: "Cutoff", "Release". */
  label: string;
  value: number;
  min: number;
  max: number;
  /** Resolution of the value (also the arrow-key step on a linear scale). */
  step: number;
  /** Logarithmic travel (frequencies, times). */
  log?: boolean;
  /** The readout, e.g. "700 Hz". */
  format: (v: number) => string;
  disabled?: boolean;
  title?: string;
  /** Wider track for wide ranges (pitch). */
  wide?: boolean;
  onChange: (v: number, mode: "drag" | "commit") => void;
  /** Ends a drag, arrow-key or wheel gesture: flush any pending write and
   *  settle the draft without replaying the edit. */
  onEnd?: () => void;
}

export function ValueSlider(p: ValueSliderProps) {
  const { min, max, step, log = false } = p;
  const [typing, setTyping] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const last = useRef<number | null>(null);
  const prevProp = useRef<number>(p.value);
  const wheelTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const wheelAcc = useRef(new WheelAccumulator());
  const now = useRef(p);
  now.current = p;

  const clampV = (v: number) => Math.max(min, Math.min(max, v));
  const quant = (v: number) => clampV(Number((Math.round(v / step) * step).toFixed(6)));
  const toPos = (v: number) => (log ? Math.round((Math.log(clampV(v) / min) / Math.log(max / min)) * STEPS) : clampV(v));
  const fromPos = (pos: number) => quant(log ? min * Math.pow(max / min, pos / STEPS) : pos);

  const clearWheelTimer = () => {
    if (wheelTimer.current) {
      clearTimeout(wheelTimer.current);
      wheelTimer.current = null;
    }
  };

  // If the value changed externally (e.g. a reset or preset button) while a
  // wheel or drag gesture was open, drop our cached step base and timer.
  if (p.value !== prevProp.current) {
    prevProp.current = p.value;
    if (last.current !== null && p.value !== last.current) {
      clearWheelTimer();
      last.current = null;
    }
  }

  const endGesture = () => {
    clearWheelTimer();
    if (last.current === null) return;
    last.current = null;
    now.current.onEnd?.();
  };

  useEffect(() => {
    const el = inputRef.current;
    if (!el) return;
    const gate = attachSliderWheelGate(el);
    const onWheel = (e: WheelEvent) => {
      const curP = now.current;
      if (e.ctrlKey || curP.disabled) return;
      const dir = wheelDirection(e.deltaY, e.deltaX);
      if (dir === 0) return;
      const ts = performance.now();
      if (!gate.canTakeWheel(ts)) {
        wheelAcc.current.reset();
        return;
      }
      const cur = last.current ?? curP.value;
      // Leave scrolling past the end to the page.
      if (stepValue(dir, cur, curP) === cur) {
        wheelAcc.current.reset();
        return;
      }
      const { steps } = wheelAcc.current.push(e.deltaY, e.deltaMode, ts, e.deltaX);
      if (steps === 0) return;
      const next = stepValue(steps, cur, curP);
      if (next === cur) return;
      e.preventDefault();
      last.current = next;
      curP.onChange(next, "drag");
      if (wheelTimer.current) clearTimeout(wheelTimer.current);
      wheelTimer.current = setTimeout(() => {
        wheelTimer.current = null;
        if (last.current === null) return;
        last.current = null;
        now.current.onEnd?.();
      }, WHEEL_IDLE_MS);
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      el.removeEventListener("wheel", onWheel);
      gate.dispose();
      if (wheelTimer.current) {
        clearTimeout(wheelTimer.current);
        wheelTimer.current = null;
        if (last.current !== null) {
          last.current = null;
          now.current.onEnd?.();
        }
      }
    };
  }, []);

  const commitTyped = () => {
    const t = typing;
    setTyping(null);
    if (t === null) return;
    const v = parseNumber(t);
    if (v === null) return;
    const q = quant(v);
    if (q !== p.value) p.onChange(q, "commit");
  };

  return (
    <div className={"vslider" + (p.wide ? " wide" : "")}>
      <input
        ref={inputRef}
        id={p.id}
        type="range"
        min={log ? 0 : min}
        max={log ? STEPS : max}
        step={log ? 1 : step}
        value={toPos(p.value)}
        disabled={p.disabled}
        aria-label={p.label}
        aria-valuetext={p.format(p.value)}
        title={p.disabled ? p.title : p.title ? `${p.title} ${WHEEL_HINT}` : WHEEL_HINT}
        onPointerDown={(e) => {
          if (!p.disabled) e.currentTarget.focus();
        }}
        onChange={(e) => {
          clearWheelTimer();
          const v = fromPos(Number(e.target.value));
          if (v === p.value && last.current === null) return;
          last.current = v;
          p.onChange(v, "drag");
        }}
        onKeyDown={(e) => {
          // Arrows move one displayed step, also on a log scale (the native
          // step there is a fraction of a step and would snap back). Page
          // Up/Down and Home/End keep the native jumps.
          if (!e.key.startsWith("Arrow")) return;
          const v = keyStepValue(e.key, last.current ?? p.value, { min, max, step });
          if (v === null) return;
          e.preventDefault();
          clearWheelTimer();
          if (v === (last.current ?? p.value)) return;
          last.current = v;
          p.onChange(v, "drag");
        }}
        onPointerUp={endGesture}
        onKeyUp={endGesture}
        onBlur={endGesture}
      />
      {typing === null ? (
        <output
          htmlFor={p.id}
          tabIndex={p.disabled ? -1 : 0}
          title={p.disabled ? p.title : "Double-click (or Enter) to type a value"}
          onDoubleClick={() => !p.disabled && setTyping(String(Number(p.value.toFixed(3))))}
          onKeyDown={(e) => {
            if (!p.disabled && e.key === "Enter") {
              e.preventDefault();
              setTyping(String(Number(p.value.toFixed(3))));
            }
          }}
        >
          {p.format(p.value)}
        </output>
      ) : (
        <input
          className="fine-input"
          autoFocus
          aria-label={`${p.label}, type a value`}
          value={typing}
          onFocus={(e) => e.currentTarget.select()}
          onChange={(e) => setTyping(e.target.value)}
          onBlur={commitTyped}
          onKeyDown={(e) => {
            if (e.key === "Enter") (e.target as HTMLInputElement).blur();
            else if (e.key === "Escape") setTyping(null);
          }}
        />
      )}
    </div>
  );
}
