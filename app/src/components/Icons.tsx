import type { FilterType } from "../api/types";

type P = { className?: string };
const I = ({ d, className, vb = "0 0 24 24" }: { d: string | string[]; className?: string; vb?: string }) => (
  <svg className={"i" + (className ? " " + className : "")} viewBox={vb} aria-hidden="true">
    {(Array.isArray(d) ? d : [d]).map((x) => (
      <path key={x} d={x} />
    ))}
  </svg>
);

export const Power = (p: P) => <I {...p} d={["M12 3v8", "M6.3 7.2a8 8 0 1 0 11.4 0"]} />;
export const Trash = (p: P) => <I {...p} d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3" />;
export const Chevron = (p: P) => <I {...p} d="m9 6 6 6-6 6" />;
export const ChevronDown = (p: P) => <I {...p} d="m6 9 6 6 6-6" />;
export const Close = (p: P) => <I {...p} d="M6 6l12 12M18 6 6 18" />;
export const Undo = (p: P) => <I {...p} d={["M9 14 4 9l5-5", "M4 9h10a6 6 0 0 1 0 12h-3"]} />;
export const Redo = (p: P) => <I {...p} d={["m15 14 5-5-5-5", "M20 9H10a6 6 0 0 0 0 12h3"]} />;
export const Download = (p: P) => <I {...p} d="M12 3v12M7 10l5 5 5-5M5 21h14" />;
export const Plus = (p: P) => <I {...p} d="M12 5v14M5 12h14" />;
export const Arrow = (p: P) => <I {...p} d="M4 12h16M14 6l6 6-6 6" />;
export const Device = (p: P) => (
  <svg className={"i" + (p.className ? " " + p.className : "")} viewBox="0 0 24 24" aria-hidden="true">
    <rect x="4" y="3" width="16" height="18" rx="2" />
    <circle cx="12" cy="14" r="3.5" />
    <circle cx="12" cy="7" r="1" />
  </svg>
);
export const Speaker = ({ muted, className }: P & { muted: boolean }) => (
  <svg className={"i" + (className ? " " + className : "")} viewBox="0 0 24 24" aria-hidden="true">
    <path d="M4 9h4l5-4v14l-5-4H4z" />
    {muted ? <path d="m16.5 9.5 5 5m0-5-5 5" /> : <path d="M16.5 8.5a5 5 0 0 1 0 7" />}
  </svg>
);
export const Check = (p: P) => <I {...p} d="m5 12 5 5 9-10" />;
/** Theme (light/dark) icon: an outlined circle with its right half filled. */
export const ThemeIcon = ({ className }: P) => (
  <svg className={"i" + (className ? " " + className : "")} viewBox="0 0 24 24" aria-hidden="true">
    <circle cx="12" cy="12" r="8.5" />
    <path d="M12 3.5a8.5 8.5 0 0 1 0 17z" fill="currentColor" stroke="none" />
  </svg>
);
export const Layers = (p: P) => <I {...p} d={["m12 3 9 5-9 5-9-5 9-5", "m3 13 9 5 9-5", "m3 17 9 5 9-5"]} />;
export const Eye = (p: P) => <I {...p} d={["M2 12s3.6-6 10-6 10 6 10 6-3.6 6-10 6-10-6-10-6z", "M12 9.2a2.8 2.8 0 1 0 0 5.6 2.8 2.8 0 0 0 0-5.6"]} />;
export const EyeOff = (p: P) => <I {...p} d={["M2 12s3.6-6 10-6 10 6 10 6-3.6 6-10 6-10-6-10-6z", "M4 4l16 16"]} />;

/** Brand mark: four level bars in a rounded tile (an abstract EQ, not a letter). */
export const Logo = () => (
  <svg viewBox="0 0 24 24" aria-hidden="true">
    <rect x="1.5" y="1.5" width="21" height="21" rx="6" fill="currentColor" opacity="0.16" />
    <g fill="currentColor">
      <rect x="5.5" y="10" width="2.4" height="7.5" rx="1.2" />
      <rect x="9.3" y="6.5" width="2.4" height="11" rx="1.2" />
      <rect x="13.1" y="8.5" width="2.4" height="9" rx="1.2" />
      <rect x="16.9" y="12" width="2.4" height="5.5" rx="1.2" />
    </g>
  </svg>
);

export const FILTER_TYPES: { kind: FilterType; label: string; d: string }[] = [
  { kind: "peaking", label: "Peak", d: "M2 12C6 12 7 4 10 4s4 8 8 8" },
  { kind: "low_shelf", label: "Low shelf", d: "M2 5c4 0 6 0 8 3.5S14 12 18 12" },
  { kind: "high_shelf", label: "High shelf", d: "M2 12c4 0 6 0 8-3.5S14 5 18 5" },
  { kind: "low_pass", label: "Low-pass", d: "M2 6h9c3 0 4 3 7 8" },
  { kind: "high_pass", label: "High-pass", d: "M2 14c3-5 4-8 7-8h9" },
  { kind: "notch", label: "Notch", d: "M2 6h6c1 0 1.2 8 2 8s1-8 2-8h6" },
  { kind: "band_pass", label: "Band-pass", d: "M2 14c3 0 5-8 8-8s5 8 8 8" },
  { kind: "all_pass", label: "All-pass", d: "M2 9h16M2 9" },
];

export function filterLabel(kind: FilterType): string {
  return FILTER_TYPES.find((t) => t.kind === kind)?.label ?? kind;
}

export const FilterIcon = ({ kind }: { kind: FilterType }) => {
  const t = FILTER_TYPES.find((x) => x.kind === kind) ?? FILTER_TYPES[0];
  return <I vb="0 0 20 16" d={t.d} />;
};
