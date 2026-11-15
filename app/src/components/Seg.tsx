/** Segmented control. An option's optional 4th element disables just that
 * button; its title then carries the reason. */
export function Seg<T extends string | number>({ label, value, options, onChange, disabled }: { label: string; value: T | null; options: [T, string, string?, boolean?][]; onChange: (v: T) => void; disabled?: boolean }) {
  return (
    <div className="seg" role="group" aria-label={label}>
      {options.map(([v, text, title, optDisabled]) => (
        <button key={String(v)} aria-pressed={value === v} title={title} disabled={disabled || optDisabled} onClick={() => value !== v && onChange(v)}>
          {text}
        </button>
      ))}
    </div>
  );
}
