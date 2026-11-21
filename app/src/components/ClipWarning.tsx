import { store, useApp } from "../store/store";

/** Shown when the output clipped while the limiter was off: one click turns
 *  it on in auto headroom. Nothing otherwise. */
export function ClipWarning() {
  const st = store();
  const show = useApp((s) => s.clipNoLimiter);
  if (!show) return null;
  return (
    <div className="notice warn clip-warn" role="status">
      <span>The output clipped with the limiter off.</span>{" "}
      <button type="button" className="btn small" onClick={() => st.turnOnLimiter()}>
        Turn on limiter
      </button>{" "}
      <button type="button" className="link-btn" onClick={() => st.dismissClipWarning()}>
        Dismiss
      </button>
    </div>
  );
}
