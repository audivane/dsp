// The tray's volume popup: its own small window next to the tray icon
// (src-tauri/src/app.rs opens it on a left click). The level in dB, a
// slider, mute, what is playing and whether it is bit-perfect, and a link
// to the main window. It writes through its own store with the same
// guarded volume gestures as the top bar (`useVolumeModel`), so the
// ceiling and the guard apply exactly as there. Escape or a click
// elsewhere closes it.

import { useEffect, useRef } from "react";
import { fmtDb } from "../lib/format";
import { verdict } from "../lib/passthrough";
import { useVolumeModel, VolumeModeTag } from "../components/TopBar";
import { HeldHeadroomTag } from "../components/HeldHeadroom";
import { FineSlider } from "../components/FineSlider";
import { Speaker } from "../components/Icons";
import { useApp, type VolumeGesture } from "../store/store";
import type { Transport } from "../api/transport";

function nowPlayingText(np: { player: string; artist?: string | null; title?: string | null } | null): string | null {
  if (!np || !(np.title || np.artist)) return null;
  return [np.artist, np.title].filter(Boolean).join(" · ");
}

export function TrayPopup({ transport }: { transport: Transport }) {
  const conn = useApp((s) => s.conn);
  const status = useApp((s) => s.status);
  const streams = useApp((s) => s.streams);
  const np = useApp((s) => s.nowPlaying);
  const { st, vs, v, epoch, begin, bitPerfect, off, title, volTaper, volText } = useVolumeModel();
  const root = useRef<HTMLDivElement>(null);

  // Opening the popup puts the focus on the slider, so arrows work at once
  // (also when the level arrives just after the window opened).
  const focusSlider = () => root.current?.querySelector<HTMLElement>("#popup-vol")?.focus();
  useEffect(() => {
    return transport.onShell((e) => {
      if (e.kind === "popup-shown") focusSlider();
    });
  }, [transport]);
  useEffect(() => {
    if (!off) focusSlider();
  }, [off, epoch]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      // Escape inside the typed-value box only leaves the box.
      if ((e.target as HTMLElement | null)?.classList?.contains("fine-input")) return;
      transport.hidePopup();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [transport]);

  const verdictNow = verdict(status?.passthrough, streams);
  const npText = nowPlayingText(np);
  const ceiling = vs?.ceiling_db ?? null;
  const up = conn === "up" && !!status;
  const hint = "Arrow keys or wheel: 0.5 dB, with Shift 0.1 dB, with Alt 0.01 dB; Page Up/Down: 6 dB. Double-click the value to type it.";

  return (
    <div className="popup" ref={root} role="dialog" aria-label="Audivane DSP volume">
      <div className="popup-head">
        <span className="popup-title">Volume</span>
        {up && (
          <span className={"badge " + verdictNow.kind} title={[verdictNow.summary, ...verdictNow.reasons].join(" · ")}>
            {verdictNow.label}
          </span>
        )}
      </div>
      {up ? (
        <>
          <div className={"popup-level" + (v?.muted ? " muted" : "")} title={off ? title : hint}>
            <FineSlider
              key={`popup-vol-${epoch}`}
              id="popup-vol"
              label="Volume"
              value={v && vs ? v.volume_db : null}
              taper={volTaper}
              disabled={off}
              text={volText}
              title={off ? title : hint}
              onBegin={begin}
              onChange={(db, mode, g) => st.setVolume({ volume_db: db }, mode, g as VolumeGesture | null)}
            />
            <div className="popup-side ctl">
              <VolumeModeTag vs={vs} v={v} bitPerfect={bitPerfect} />
              {!bitPerfect && <HeldHeadroomTag />}
              <button
                className="icon-btn mute"
                aria-pressed={!!v?.muted}
                aria-label="Mute"
                title={!v || !vs ? title : v.muted ? "Unmute" : "Mute"}
                disabled={!v || !vs}
                onClick={() => v && st.setVolume({ muted: !v.muted }, "commit", begin())}
              >
                <Speaker muted={!!v?.muted} />
              </button>
            </div>
          </div>
          <div className="popup-np" title={npText ?? undefined}>
            {npText ?? (status.state === "playing" ? "Playing" : "Nothing playing")}
          </div>
        </>
      ) : (
        <div className="popup-down" role="status">
          {conn === "down" ? "The engine is not running." : "Connecting to the engine…"}
        </div>
      )}
      <div className="popup-foot">
        <button
          className="link-btn"
          onClick={() => {
            transport.showMainWindow();
            transport.hidePopup();
          }}
          title="Open the main window"
        >
          Open Audivane DSP
        </button>
        {ceiling !== null && ceiling !== undefined && (
          <span className="hint" title="The volume ceiling (Device → Volume): the loudest the volume can go, from any source">
            Ceiling {fmtDb(ceiling, 1, false)} dB
          </span>
        )}
      </div>
    </div>
  );
}
