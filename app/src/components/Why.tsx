// A control that is unavailable for a reason. A native `disabled` button
// leaves the tab order and screen readers never get its title, so these
// stay focusable, are marked `aria-disabled`, point `aria-describedby` at
// the reason and show it while focused (`.why-tip` in app.css). Callers
// ignore the click themselves.

export interface WhyAttrs {
  "aria-disabled"?: true;
  "aria-describedby"?: string;
}

export function whyFor(id: string, off: boolean, reason: string | null | undefined): { attrs: WhyAttrs; tip: React.ReactNode } {
  if (!off) return { attrs: {}, tip: null };
  if (!reason) return { attrs: { "aria-disabled": true }, tip: null };
  return {
    attrs: { "aria-disabled": true, "aria-describedby": id },
    tip: (
      <span id={id} className="why-tip">
        {reason}
      </span>
    ),
  };
}
