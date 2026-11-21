// A dB slider with a taper, 0.1 dB steps and keyboard/wheel/typing:
// arrows move 0.5 dB (Shift 0.1), Page Up/Down 6 dB, the wheel (vertical only)
// moves like the arrows, no step ever goes against its direction, double-clicking the
// value lets you type one, and double-clicking the track resets (balance:
// centre). The native range input is not used because its value would be the
// travel, not the dB; a `role="slider"` keeps the accessible value in dB.

import { useEffect, useRef, useState } from "react";
import { parseNumber } from "../lib/format";
import { keyStepDb, quantiseDb, stepDb, wheelStepDb, type Taper } from "../lib/taper";
import {
  WHEEL_IDLE_MS,
  WheelAccumulator,
  attachSliderWheelGate,
  wheelDirection,
} from "./sliderWheel";

export interface FineSliderProps {
  id?: string;
  /** Shown to screen readers: "Volume", "Balance". */
  label: string;
  /** Current value in dB; `null` while it is not known (no fill, no knob,
   *  and nothing to move). */
  value: number | null;
  taper: Taper;
  disabled?: boolean;
  /** Where the filled part starts: the left end, or the centre (balance). */
  fillFrom?: "start" | "centre";
  /** The readout text ("−12.3 dB", "C", "L 1.2"). */
  text: string;
  /** Begins a gesture from what this render shows (pointer-down, the
   *  first wheel step, a key, a typed value), even when the value then
   *  stays put; its handle comes back with each change of that gesture.
   *  Null: nothing to change from. */
  onBegin?: () => unknown;
  /** Called while moving ("drag", throttled by the store) and once on
   *  release, with the gesture's handle. Returns false when the gesture
   *  was dropped (its later changes are ignored up to its release). */
  onChange: (db: number, mode: "drag" | "commit", gesture: unknown) => boolean | void;
  /** Double-click on the slider (balance: centre). */
  onReset?: () => void;
  /** When the slider goes away mid-gesture (a wheel burst still waiting
   *  for its pause, a held drag), commit the gesture's last value instead
   *  of dropping it, so the owner's draft ends. Off for sliders whose
   *  gestures must not outlive them (the top bar's, keyed by connection). */
  commitOnUnmount?: boolean;
  title?: string;
}

export function FineSlider(props: FineSliderProps) {
  const { id, label, taper, fillFrom = "start", text, title } = props;
  const known = props.value !== null;
  const value = props.value ?? taper.max;
  const disabled = (props.disabled ?? false) || !known;
  const track = useRef<HTMLDivElement>(null);
  const dragging = useRef<number | null>(null);
  const wheelTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Each input has its own gesture, alive until that input completes (a
  // pointer until its release, a wheel burst until its pause): a wheel
  // burst during a held drag never hands its gesture to the pointer.
  const pointer = useRef<{ gesture: unknown; last: number } | null>(null);
  const wheel = useRef<{ gesture: unknown; last: number } | null>(null);
  const wheelAcc = useRef(new WheelAccumulator());
  const [draft, setDraft] = useState<string | null>(null);

  // The wheel listener must be non-passive, so it is attached by hand; the
  // ref keeps it looking at the newest props without re-attaching.
  const now = useRef(props);
  now.current = props;
  useEffect(() => {
    const el = track.current;
    if (!el) return;
    const gate = attachSliderWheelGate(el);
    const onWheel = (e: WheelEvent) => {
      const p = now.current;
      if (e.ctrlKey || p.disabled || p.value === null) return;
      const dir = wheelDirection(e.deltaY, e.deltaX);
      if (dir === 0) return;
      const ts = performance.now();
      if (!gate.canTakeWheel(ts)) {
        wheelAcc.current.reset();
        return;
      }
      const cur = wheel.current ? wheel.current.last : p.value;
      // Never against the wheel's direction (see `stepDb`), and leave
      // scrolling past the end to the page.
      const probe = wheelStepDb(dir > 0 ? -1 : 1, e.shiftKey, cur, p.taper);
      if (probe === null || probe === cur) {
        wheelAcc.current.reset();
        return;
      }
      const { steps } = wheelAcc.current.push(e.deltaY, e.deltaMode, ts, e.deltaX);
      if (steps === 0) return;
      let next = cur;
      const stepDeltaY = steps > 0 ? -1 : 1;
      for (let i = 0; i < Math.abs(steps); i++) {
        const stepped = wheelStepDb(stepDeltaY, e.shiftKey, next, p.taper);
        if (stepped === null || stepped === next) break;
        next = stepped;
      }
      if (next === cur) return;
      // A burst is one gesture, begun from the render its first step was
      // computed from.
      if (wheelTimer.current) clearTimeout(wheelTimer.current);
      wheelTimer.current = null;
      if (!wheel.current) {
        const g = p.onBegin ? p.onBegin() : undefined;
        if (g === null) return;
        wheel.current = { gesture: g, last: p.value };
      }
      const burst = wheel.current;
      burst.last = next;
      if (p.onChange(next, "drag", burst.gesture) === false) {
        // Dropped (the level changed under it): the next step begins anew
        // from what is shown then.
        wheel.current = null;
        return;
      }
      e.preventDefault();
      wheelTimer.current = setTimeout(() => {
        wheelTimer.current = null;
        wheel.current = null;
        // The store writes nothing when this step is already written.
        now.current.onChange(burst.last, "commit", burst.gesture);
      }, WHEEL_IDLE_MS);
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      el.removeEventListener("wheel", onWheel);
      gate.dispose();
      if (wheelTimer.current) clearTimeout(wheelTimer.current);
      wheelTimer.current = null;
      const burst = wheel.current;
      wheel.current = null;
      const drag = pointer.current;
      pointer.current = null;
      if (!now.current.commitOnUnmount) return;
      // Unmounted mid-gesture: end it with what it last showed.
      if (burst) now.current.onChange(burst.last, "commit", burst.gesture);
      if (drag) now.current.onChange(drag.last, "commit", drag.gesture);
    };
  }, []);

  /** A pointer step: only a change of value is sent. */
  const drag = (db: number) => {
    const d = pointer.current;
    if (!d || db === d.last) return;
    d.last = db;
    props.onChange(db, "drag", d.gesture);
  };

  /** A one-shot change (a key, a typed value): its own gesture, begun
   *  from this render. */
  const once = (db: number) => {
    const g = props.onBegin ? props.onBegin() : undefined;
    if (g === null) return;
    props.onChange(db, "commit", g);
  };

  const posOf = (clientX: number): number => {
    const r = track.current!.getBoundingClientRect();
    return r.width > 0 ? Math.max(0, Math.min(1, (clientX - r.left) / r.width)) : 0;
  };

  const onPointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    if (disabled || e.button !== 0) return;
    e.preventDefault();
    e.currentTarget.focus();
    // The gesture's base is what is shown now, also when the press lands
    // on the knob and the value does not change until the first move.
    const g = props.onBegin ? props.onBegin() : undefined;
    if (g === null) return;
    pointer.current = { gesture: g, last: value };
    dragging.current = e.pointerId;
    e.currentTarget.setPointerCapture(e.pointerId);
    drag(quantiseDb(taper.db(posOf(e.clientX)), taper));
  };

  const onPointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    if (dragging.current !== e.pointerId) return;
    drag(quantiseDb(taper.db(posOf(e.clientX)), taper));
  };

  const endDrag = (e: React.PointerEvent<HTMLDivElement>) => {
    if (dragging.current !== e.pointerId) return;
    dragging.current = null;
    const d = pointer.current;
    pointer.current = null;
    if (d) props.onChange(d.last, "commit", d.gesture);
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (disabled) return;
    // A step down never ends higher, even below the reported range.
    const db = keyStepDb(e.key, e.shiftKey, value, taper);
    if (db === null) return;
    e.preventDefault();
    if (db !== value) once(db);
  };

  const commitDraft = () => {
    const t = draft;
    setDraft(null);
    if (t === null) return;
    const v = parseNumber(t);
    if (v === null) return;
    const db = stepDb(value, v, taper);
    if (db !== value) once(db);
  };

  const pct = Math.max(0, Math.min(100, taper.pos(value) * 100));
  const from = fillFrom === "centre" ? 50 : 0;
  const left = Math.min(from, pct);
  const width = Math.abs(pct - from);

  return (
    <div className="fine">
      <div
        id={id}
        ref={track}
        className="fine-slider"
        role="slider"
        tabIndex={disabled ? -1 : 0}
        aria-label={label}
        aria-orientation="horizontal"
        aria-valuemin={taper.min}
        aria-valuemax={taper.max}
        aria-valuenow={known ? Math.round(value * 10) / 10 : undefined}
        aria-valuetext={text}
        aria-disabled={disabled || undefined}
        title={title}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={endDrag}
        onPointerCancel={endDrag}
        onLostPointerCapture={endDrag}
        onKeyDown={onKeyDown}
        onDoubleClick={() => !disabled && props.onReset?.()}
      >
        <div className="fine-track">
          {known && (
            <>
              <span className="fine-fill" style={{ left: `${left}%`, width: `${width}%` }} />
              <span className="fine-knob" style={{ left: `${pct}%` }} />
            </>
          )}
        </div>
      </div>
      {draft === null ? (
        <output htmlFor={id} title={disabled ? title : "Double-click to type a value"} onDoubleClick={() => !disabled && setDraft(text)}>
          {text}
        </output>
      ) : (
        <input
          className="fine-input"
          autoFocus
          aria-label={`${label} in dB`}
          value={draft}
          onFocus={(e) => e.currentTarget.select()}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commitDraft}
          onKeyDown={(e) => {
            if (e.key === "Enter") (e.target as HTMLInputElement).blur();
            else if (e.key === "Escape") setDraft(null);
          }}
        />
      )}
    </div>
  );
}
