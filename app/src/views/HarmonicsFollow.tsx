// The Harmonics page's "Follow music" controls: a toggle beside a test-sine
// graph's slider with what following does now, and the slider's mark.

import type { ReactNode } from "react";
import { FOLLOW_STATUS_TEXT, FOLLOW_TITLE, type FollowStatus } from "../lib/harmonicsFollow";

/** A followed slider: marked while the music moves it (dragging it stops following). */
export function Followed({ on, children }: { on: boolean; children: ReactNode }) {
  return (
    <span className={"harm-follow-slider" + (on ? " on" : "")} title={on ? "Set by the music; drag to set it yourself (stops following)" : undefined}>
      {children}
    </span>
  );
}

/** A graph's "Follow music" toggle, with what following does now. */
export function FollowButton({ on, status, what, onClick }: { on: boolean; status: FollowStatus; what: string; onClick: () => void }) {
  return (
    <>
      <button type="button" className="btn small" aria-pressed={on} onClick={onClick} title={`Follow the music: the test sine takes ${what}. ${FOLLOW_TITLE}`}>
        Follow music
      </button>
      {on && (
        <span className={"hint harm-follow-status " + status} title={FOLLOW_TITLE} role="status">
          {FOLLOW_STATUS_TEXT[status]}
        </span>
      )}
    </>
  );
}
