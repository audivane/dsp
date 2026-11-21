import { useApp } from "../store/store";
import { fmtDb } from "../lib/format";

/** "−1.5 dB held" next to the volume, only when the limiter holds headroom.
 *  From `Status.limiter_held_db`, so it shows without the meters too (the
 *  tray popup with the main window closed). */
export function HeldHeadroomTag() {
  // Tenths of a dB: the status changes in the limiter's 0.5 dB steps.
  const held = useApp((s) => Math.round((s.status?.limiter_held_db ?? 0) * 10) / 10);
  if (!(held > 0)) return null;
  return (
    <span className="held mono" title="Auto headroom: the limiter holds the level this much lower for the rest of the stream, on top of the volume, because the music needs it.">
      {fmtDb(-held, 1)} dB held
    </span>
  );
}
