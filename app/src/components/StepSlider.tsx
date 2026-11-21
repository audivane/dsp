// A plain range slider that commits once: on release, key-up or a pause in
// the mouse wheel. While it moves it keeps a local draft (reported through
// `onDraft` so a field next to it can show the moving value).

import { useEffect, useRef, useState } from "react";
import { WHEEL_HINT, WHEEL_IDLE_MS, WheelAccumulator, attachSliderWheelGate, wheelDirection } from "./sliderWheel";
import { stepValue } from "./valueSliderKeys";

export interface StepRange {
  min: number;
  max: number;
  step: number;
}

export function StepSlider({
  value,
  onCommit,
  onDraft,
  label,
  range,
  format,
  showValue = false,
  disabled = false,
  className,
  id,
}: {
  value: number;
  onCommit: (v: number) => void;
  onDraft?: (v: number | null) => void;
  label: string;
  range: StepRange;
  /** The readout and the screen-reader value, e.g. "3.0 dB". */
  format: (v: number) => string;
  showValue?: boolean;
  disabled?: boolean;
  className?: string;
  id?: string;
}) {
  const [draft, setDraftState] = useState<number | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const lastRef = useRef<number | null>(null);
  const wheelTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const wheelAcc = useRef(new WheelAccumulator());
  const now = useRef({ value, onCommit, onDraft, range, disabled });
  now.current = { value, onCommit, onDraft, range, disabled };
  const setDraft = (v: number | null) => {
    setDraftState(v);
    now.current.onDraft?.(v);
  };
  const setDraftRef = useRef(setDraft);
  setDraftRef.current = setDraft;

  useEffect(() => {
    const el = inputRef.current;
    if (!el) return;
    const gate = attachSliderWheelGate(el);
    const onWheel = (e: WheelEvent) => {
      if (e.ctrlKey || now.current.disabled) return;
      const dir = wheelDirection(e.deltaY, e.deltaX);
      if (dir === 0) return;
      const ts = performance.now();
      if (!gate.canTakeWheel(ts)) {
        wheelAcc.current.reset();
        return;
      }
      const r = now.current.range;
      const cur = lastRef.current ?? now.current.value;
      if (stepValue(dir, cur, r) === cur) {
        wheelAcc.current.reset();
        return;
      }
      const { steps } = wheelAcc.current.push(e.deltaY, e.deltaMode, ts, e.deltaX);
      if (steps === 0) return;
      const next = stepValue(steps, cur, r);
      if (next === cur) return;
      e.preventDefault();
      lastRef.current = next;
      setDraftRef.current(next);
      if (wheelTimer.current) clearTimeout(wheelTimer.current);
      wheelTimer.current = setTimeout(() => {
        wheelTimer.current = null;
        const finalV = lastRef.current;
        lastRef.current = null;
        setDraftRef.current(null);
        if (finalV !== null && finalV !== now.current.value) now.current.onCommit(finalV);
      }, WHEEL_IDLE_MS);
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      el.removeEventListener("wheel", onWheel);
      gate.dispose();
      if (wheelTimer.current) {
        clearTimeout(wheelTimer.current);
        wheelTimer.current = null;
        const finalV = lastRef.current;
        lastRef.current = null;
        if (finalV !== null && finalV !== now.current.value) now.current.onCommit(finalV);
      }
    };
  }, []);

  const v = draft ?? value;
  const finish = () => {
    if (wheelTimer.current) {
      clearTimeout(wheelTimer.current);
      wheelTimer.current = null;
    }
    const finalV = lastRef.current ?? draft;
    lastRef.current = null;
    if (finalV !== null && finalV !== value) onCommit(finalV);
    if (draft !== null) setDraft(null);
  };
  return (
    <div className={className}>
      <input
        ref={inputRef}
        id={id}
        type="range"
        min={range.min}
        max={range.max}
        step={range.step}
        value={v}
        disabled={disabled}
        aria-label={label}
        aria-valuetext={format(v)}
        title={WHEEL_HINT}
        onPointerDown={(e) => e.currentTarget.focus()}
        onChange={(e) => {
          if (wheelTimer.current) {
            clearTimeout(wheelTimer.current);
            wheelTimer.current = null;
          }
          const next = Number(e.target.value);
          lastRef.current = next;
          setDraft(next);
        }}
        onPointerUp={finish}
        onKeyUp={finish}
      />
      {showValue && <output>{format(v)}</output>}
    </div>
  );
}
