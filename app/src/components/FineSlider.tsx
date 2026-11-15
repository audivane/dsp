// A dB slider with a taper, 0.1 dB steps and keyboard/wheel/typing:
// arrows move 0.5 dB (Shift 0.1), the wheel does the same, double-clicking the
// value lets you type one, and double-clicking the track resets (balance:
// centre). The native range input is not used because its value would be the
// travel, not the dB; a `role="slider"` keeps the accessible value in dB.

import { useEffect, useRef, useState } from "react";
import { parseNumber } from "../lib/format";
import { quantiseDb, type Taper } from "../lib/taper";

export interface FineSliderProps {
  id?: string;
  /** Shown to screen readers: "Volume", "Balance". */
  label: string;
  /** Current value in dB. */
  value: number;
  taper: Taper;
  disabled?: boolean;
  /** Where the filled part starts: the left end, or the centre (balance). */
  fillFrom?: "start" | "centre";
  /** The readout text ("−12.3 dB", "C", "L 1.2"). */
  text: string;
  /** Called while moving ("drag", throttled by the store) and once on release. */
  onChange: (db: number, mode: "drag" | "commit") => void;
  /** Double-click on the slider (balance: centre). */
  onReset?: () => void;
  title?: string;
}

export function FineSlider(props: FineSliderProps) {
  const { id, label, value, taper, disabled = false, fillFrom = "start", text, title } = props;
  const track = useRef<HTMLDivElement>(null);
  const dragging = useRef<number | null>(null);
  const wheelTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const latest = useRef(value);
  latest.current = value;
  const [draft, setDraft] = useState<string | null>(null);

  // The wheel listener must be non-passive, so it is attached by hand; the
  // ref keeps it looking at the newest props without re-attaching.
  const now = useRef(props);
  now.current = props;
  useEffect(() => {
    const el = track.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      const p = now.current;
      if (p.disabled) return;
      e.preventDefault();
      const step = e.shiftKey ? 0.1 : 0.5;
      const next = quantiseDb(p.value + (e.deltaY < 0 ? step : -step), p.taper);
      if (next === p.value) return;
      latest.current = next;
      p.onChange(next, "drag");
      if (wheelTimer.current) clearTimeout(wheelTimer.current);
      wheelTimer.current = setTimeout(() => {
        wheelTimer.current = null;
        now.current.onChange(latest.current, "commit");
      }, 350);
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      el.removeEventListener("wheel", onWheel);
      if (wheelTimer.current) clearTimeout(wheelTimer.current);
    };
  }, []);

  const set = (db: number, mode: "drag" | "commit") => {
    if (db === latest.current && mode === "drag") return;
    latest.current = db;
    props.onChange(db, mode);
  };

  const posOf = (clientX: number): number => {
    const r = track.current!.getBoundingClientRect();
    return r.width > 0 ? Math.max(0, Math.min(1, (clientX - r.left) / r.width)) : 0;
  };

  const onPointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    if (disabled || e.button !== 0) return;
    e.preventDefault();
    e.currentTarget.focus();
    dragging.current = e.pointerId;
    e.currentTarget.setPointerCapture(e.pointerId);
    set(quantiseDb(taper.db(posOf(e.clientX)), taper), "drag");
  };

  const onPointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    if (dragging.current !== e.pointerId) return;
    set(quantiseDb(taper.db(posOf(e.clientX)), taper), "drag");
  };

  const endDrag = (e: React.PointerEvent<HTMLDivElement>) => {
    if (dragging.current !== e.pointerId) return;
    dragging.current = null;
    props.onChange(latest.current, "commit");
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (disabled) return;
    const step = e.shiftKey ? 0.1 : 0.5;
    let next: number | null = null;
    if (e.key === "ArrowRight" || e.key === "ArrowUp") next = value + step;
    else if (e.key === "ArrowLeft" || e.key === "ArrowDown") next = value - step;
    else if (e.key === "Home") next = taper.min;
    else if (e.key === "End") next = taper.max;
    if (next === null) return;
    e.preventDefault();
    const db = quantiseDb(next, taper);
    if (db !== value) set(db, "commit");
  };

  const commitDraft = () => {
    const t = draft;
    setDraft(null);
    if (t === null) return;
    const v = parseNumber(t);
    if (v === null) return;
    const db = quantiseDb(v, taper);
    if (db !== value) set(db, "commit");
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
        aria-valuenow={Math.round(value * 10) / 10}
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
          <span className="fine-fill" style={{ left: `${left}%`, width: `${width}%` }} />
          <span className="fine-knob" style={{ left: `${pct}%` }} />
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
