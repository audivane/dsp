import { Fragment, useId } from "react";
import { whyFor } from "./Why";

/** Segmented control. An option's optional 4th element makes just that
 * button unavailable: it stays focusable and its title, the reason, is its
 * description (see `Why`). `disabled` turns the whole group off. The 5th
 * element is a class for that button. */
export function Seg<T extends string | number>({ label, value, options, onChange, disabled, className }: { label: string; value: T | null; options: [T, string, string?, boolean?, string?][]; onChange: (v: T) => void; disabled?: boolean; className?: string }) {
  const id = useId();
  return (
    <div className={className ? "seg " + className : "seg"} role="group" aria-label={label}>
      {options.map(([v, text, title, optDisabled, optClass], i) => {
        const off = !disabled && !!optDisabled;
        const why = whyFor(`${id}-${i}`, off, title);
        return (
          <Fragment key={String(v)}>
            <button type="button" className={optClass} aria-pressed={value === v} title={title} disabled={disabled} {...why.attrs} onClick={() => !off && value !== v && onChange(v)}>
              {text}
            </button>
            {why.tip}
          </Fragment>
        );
      })}
    </div>
  );
}
