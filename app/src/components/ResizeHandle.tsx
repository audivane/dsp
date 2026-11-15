import { useEffect, useRef, useState } from "react";
import { clampPlotHeight } from "../lib/plotHeight";

export interface ResizeHandleProps {
  label: string;
  /** Controlled height in px, or null when using flex/CSS default. */
  value: number | null;
  /** Current measured height in px (used for aria-valuenow and initial step when value is null). */
  measured?: number;
  min: number;
  max: number;
  className?: string;
  onChange: (height: number) => void;
  onCommit: (height: number) => void;
  onStep: (delta: number, fallbackCurrent?: number) => void;
  onReset: () => void;
  /** Measure the plot container height at drag/keyboard start when value is null. */
  measure?: () => number;
}

interface DragState {
  pointerId: number;
  startY: number;
  startHeight: number;
  latestHeight: number;
  moved: boolean;
}

export function ResizeHandle({
  label,
  value,
  measured,
  min,
  max,
  className,
  onChange,
  onCommit,
  onStep,
  onReset,
  measure,
}: ResizeHandleProps) {
  const dragRef = useRef<DragState | null>(null);
  const rafRef = useRef<number>(0);
  const [dragging, setDragging] = useState(false);

  const callbacksRef = useRef({ onChange, onCommit, onStep, onReset, measure });
  callbacksRef.current = { onChange, onCommit, onStep, onReset, measure };

  useEffect(() => {
    return () => {
      if (rafRef.current) {
        cancelAnimationFrame(rafRef.current);
        rafRef.current = 0;
      }
    };
  }, []);

  const currentForAria = Math.round(clampPlotHeight(value ?? measured ?? min, min, max));

  const onPointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    e.preventDefault();
    e.currentTarget.focus();
    const startHeight = value ?? callbacksRef.current.measure?.() ?? measured ?? min;
    dragRef.current = {
      pointerId: e.pointerId,
      startY: e.clientY,
      startHeight,
      latestHeight: clampPlotHeight(startHeight, min, max),
      moved: false,
    };
    setDragging(true);
    e.currentTarget.setPointerCapture(e.pointerId);
  };

  const onPointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    const d = dragRef.current;
    if (!d || d.pointerId !== e.pointerId) return;
    const dy = e.clientY - d.startY;
    if (dy !== 0) d.moved = true;
    d.latestHeight = clampPlotHeight(d.startHeight + dy, min, max);
    if (!rafRef.current) {
      rafRef.current = requestAnimationFrame(() => {
        rafRef.current = 0;
        const active = dragRef.current;
        if (active) callbacksRef.current.onChange(active.latestHeight);
      });
    }
  };

  const finishDrag = (e: React.PointerEvent<HTMLDivElement>) => {
    const d = dragRef.current;
    if (!d || d.pointerId !== e.pointerId) return;
    dragRef.current = null;
    setDragging(false);
    if (rafRef.current) {
      cancelAnimationFrame(rafRef.current);
      rafRef.current = 0;
    }
    if (e.currentTarget.hasPointerCapture(e.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId);
    }
    if (d.moved) {
      callbacksRef.current.onCommit(d.latestHeight);
    }
  };

  const onDoubleClick = (e: React.MouseEvent<HTMLDivElement>) => {
    e.preventDefault();
    callbacksRef.current.onReset();
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    const step = e.shiftKey ? 100 : 20;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      callbacksRef.current.onStep(step, callbacksRef.current.measure?.() ?? measured);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      callbacksRef.current.onStep(-step, callbacksRef.current.measure?.() ?? measured);
    }
  };

  return (
    <div
      className={"resize-handle" + (dragging ? " dragging" : "") + (className ? ` ${className}` : "")}
      role="separator"
      aria-orientation="horizontal"
      aria-label={label}
      aria-valuemin={min}
      aria-valuemax={max}
      aria-valuenow={currentForAria}
      tabIndex={0}
      title="Drag to resize height · double-click to reset · Arrow keys adjust"
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={finishDrag}
      onPointerCancel={finishDrag}
      onLostPointerCapture={finishDrag}
      onDoubleClick={onDoubleClick}
      onKeyDown={onKeyDown}
    >
      <span className="resize-grip" aria-hidden="true" />
    </div>
  );
}
