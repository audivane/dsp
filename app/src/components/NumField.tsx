// A labelled number field: shows the value formatted, takes typed text on
// focus, commits on Enter or blur (Escape restores). Used by the band editor
// and the upsampler's numeric settings.

import { useEffect, useRef, useState } from "react";
import { parseNumber } from "../lib/format";

export function NumField({ label, unit, value, format, parse = parseNumber, onCommit, disabled, title, className }: { label: string; unit?: string; value: number; format: (v: number) => string; parse?: (text: string) => number | null; onCommit: (v: number) => void; disabled?: boolean; title?: string; className?: string }) {
  const [text, setText] = useState(format(value));
  const [focused, setFocused] = useState(false);
  // Escape cancels: the blur it causes must not commit the typed text.
  const cancelled = useRef(false);
  useEffect(() => {
    if (!focused) setText(format(value));
  }, [value, focused, format]);
  const commit = () => {
    const v = parse(text);
    if (v === null) setText(format(value));
    else if (v !== value) onCommit(v);
  };
  return (
    <label className={className ? "field " + className : "field"} title={title}>
      <span className="lbl">{label}</span>
        <input
          value={disabled ? "-" : text}
          disabled={disabled}
          inputMode={parse === parseNumber ? "decimal" : "text"}
          autoComplete="off"
          spellCheck={false}
          onFocus={() => setFocused(true)}
          onBlur={() => {
            setFocused(false);
            if (cancelled.current) cancelled.current = false;
            else commit();
          }}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") (e.target as HTMLInputElement).blur();
            if (e.key === "Escape") {
              cancelled.current = true;
              setText(format(value));
              (e.target as HTMLInputElement).blur();
            }
          }}
        />
        {unit && <span className="unit">{unit}</span>}
    </label>
  );
}
