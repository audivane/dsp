// Bass and treble knobs (`EqProfile.tone`): two fixed shelves for every
// channel, sent with the rest of the EQ (`set_eq`), so they are in the curve,
// the auto preamp and undo. Not part of the band list.

import { useRef } from "react";
import type { EqProfile } from "../api/types";
import { EQ_PROFILE_VERSION } from "../lib/chain";
import { TONE_MAX_DB, TONE_STEP_DB, toneOn, withTone } from "../lib/tone";
import { store, useApp } from "../store/store";
import { ValueSlider } from "../components/ValueSlider";

type Mode = "drag" | "commit";
type Knob = "bass_db" | "treble_db";

const KNOBS: { knob: Knob; label: string; hint: string; title: string }[] = [
  { knob: "bass_db", label: "Bass", hint: "below ~150 Hz", title: "A gentle low shelf at 140 Hz (half its gain there), ±6 dB" },
  { knob: "treble_db", label: "Treble", hint: "above ~5 kHz", title: "A gentle high shelf at 4.9 kHz (half its gain there), ±6 dB" },
];

const fmt = (v: number) => (v === 0 ? "0 dB" : `${v > 0 ? "+" : "−"}${Math.abs(v).toFixed(1)} dB`);

export function TonePanel() {
  const st = store();
  useApp((s) => s.eqDraft);
  useApp((s) => s.config?.chain.eq);
  const dragging = useRef(false);
  const eq = st.eq();
  const on = toneOn(eq);

  const set = (knob: Knob, db: number, mode: Mode = "commit") => {
    const fn = (cur: EqProfile | null): EqProfile | null => (cur ? { ...withTone(cur, knob, db), version: EQ_PROFILE_VERSION } : cur);
    if (mode === "drag") {
      if (!dragging.current) {
        dragging.current = true;
        st.beginEqDrag();
      }
      st.editEq(fn, "drag");
      return;
    }
    if (dragging.current) {
      dragging.current = false;
      st.editEq(fn, "commit", false);
      return;
    }
    st.editEq(fn, "commit");
  };
  const endDrag = () => {
    if (!dragging.current) return;
    dragging.current = false;
    st.endEqDrag();
  };

  return (
    <div className={"panel tone-panel" + (on ? "" : " off")} aria-labelledby="tone-title">
      <h2 id="tone-title" title="Two fixed shelves for every channel, outside the band list; they count toward the auto preamp">
        Tone
      </h2>
      {KNOBS.map(({ knob, label, hint, title }) => {
        const v = eq?.tone?.[knob] ?? 0;
        return (
          <div key={knob} className="tone-knob">
            <span className="k">{label}</span>
            <ValueSlider
              id={`tone-${knob}`}
              label={label}
              value={v}
              min={-TONE_MAX_DB}
              max={TONE_MAX_DB}
              step={TONE_STEP_DB}
              disabled={!eq}
              format={fmt}
              title={title}
              onChange={(x, mode) => set(knob, x, mode)}
              onEnd={endDrag}
            />
            <button type="button" className="btn small tone-reset" disabled={!eq || v === 0} title={`${label} back to 0 dB`} aria-label={`Reset ${label.toLowerCase()} to 0 dB`} onClick={() => set(knob, 0)}>
              0
            </button>
            <span className="hint">{hint}</span>
          </div>
        );
      })}
    </div>
  );
}
