// The on-screen volume display: a small window that never takes the focus
// (src-tauri/src/app.rs shows it for about 1.2 s after a change and hides
// it). Matches the tray popup's level row (large dB readout, read-only bar
// on the exact same taper and tokens, muted state, small "Audivane DSP"
// label) with no buttons or focusable controls.

import { useEffect, useState } from "react";
import type { OsdPayload } from "../api/shell";
import type { Transport } from "../api/transport";
import { VOLUME_MAX_DB, VOLUME_MIN_DB } from "../api/types";
import { Speaker } from "../components/Icons";
import { fmtDb } from "../lib/format";
import { volumeSliderTaper } from "../lib/taper";
import { osdText } from "../lib/volumeKeys";

export function Osd({ transport }: { transport: Transport }) {
  const [p, setP] = useState<OsdPayload | null>(null);
  useEffect(() => {
    let live = true;
    const off = transport.onShell((e) => {
      if (e.kind === "osd") setP(e.payload);
    });
    // The latest change, in case it came before this page listened.
    void transport.osdReady().then((x) => live && x && setP((cur) => cur ?? x));
    return () => {
      live = false;
      off();
    };
  }, [transport]);

  if (!p) return <div className="popup osd" />;
  const hasLevel = p.volume_db !== null && !p.note;
  const taper = volumeSliderTaper(p.min_db, p.max_db, p.volume_db, VOLUME_MIN_DB, VOLUME_MAX_DB);
  const pct = hasLevel ? Math.max(0, Math.min(100, taper.pos(p.volume_db!) * 100)) : 0;
  const valueText = p.note ? p.note : p.volume_db === null ? "-" : `${fmtDb(p.volume_db, 1, false)} dB`;
  const aria = osdText(p, (db) => fmtDb(db, 1, false));

  return (
    <div
      className={"popup osd" + (p.muted ? " muted" : "") + (p.note ? " note" : "")}
      role="status"
      aria-live="polite"
      aria-label={`Audivane DSP: ${aria}`}
    >
      <div className="popup-head">
        <span className="popup-title">Audivane DSP</span>
        {p.muted && !p.note && <span className="osd-muted-tag">Muted</span>}
      </div>
      <div className={"popup-level" + (p.muted ? " muted" : "")}>
        <div className="fine">
          {!p.note && (
            <div className="osd-bar" aria-hidden="true">
              <div className="fine-track">
                {hasLevel && <span className="fine-fill" style={{ left: "0%", width: `${pct}%` }} />}
              </div>
            </div>
          )}
          <output className="osd-value">{valueText}</output>
        </div>
        {!p.note && (
          <div className="popup-side ctl osd-icon" aria-hidden="true">
            <Speaker muted={p.muted} />
          </div>
        )}
      </div>
    </div>
  );
}
