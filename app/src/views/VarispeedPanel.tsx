import { useState } from "react";
import { store, useApp } from "../store/store";
import { Seg } from "../components/Seg";
import { ValueSlider } from "../components/ValueSlider";
import { Close } from "../components/Icons";
import {
  VARISPEED_RANGES,
  clampSpeed,
  keepPitchSupported,
  rangeBounds,
  rangeFor,
  speedReadout,
  transientsSoften,
  varispeedNote,
  type VarispeedRange,
} from "../lib/varispeed";

const RANGE_TITLE: Record<VarispeedRange, string> = {
  8: "0.92× to 1.08×, as on a turntable",
  16: "0.84× to 1.16×",
  50: "0.5× to 1.5×",
};

const KEEP_PITCH_TITLE = "Changes the tempo only. Adds about 135 ms of delay and more CPU; up to 96 kHz.";
const DIRECT_TITLE = "Audivane Direct plays bit-perfect, without the DSP: no speed change there.";
const KEEP_PITCH_OLD_ENGINE = "This engine version has no Keep pitch. Update the engine to use it.";

/**
 * Varispeed: tempo and pitch change together, as on a tape machine; with
 * Keep pitch, the tempo only. It has no switch of its own: the Pitch page's
 * switch turns it on and off with the pitch shift.
 */
export function VarispeedPanel() {
  const st = store();
  useApp((s) => s.config?.varispeed);
  useApp((s) => s.varispeedDraft);
  const status = useApp((s) => s.status?.varispeed ?? null);
  const error = useApp((s) => s.varispeedError);
  const hasConfig = useApp((s) => s.config !== null);
  // Direct bypasses the DSP, varispeed included.
  const direct = useApp((s) => s.status?.active_sink === "direct");
  const kpSupported = useApp((s) => keepPitchSupported(s.config?.varispeed));
  const v = st.varispeed();
  const keepPitch = kpSupported && v.keep_pitch === true;
  const [range, setRange] = useState<VarispeedRange>(() => rangeFor(v.speed));
  const shownRange = Math.abs(v.speed - 1) > range / 100 + 1e-9 ? rangeFor(v.speed) : range;
  const { min, max } = rangeBounds(shownRange);
  const on = st.pitchPageOn();
  const disabled = !hasConfig || direct;
  const note = varispeedNote(status);
  const soften = transientsSoften(v.speed, keepPitch);

  const toggleKeepPitch = (next: boolean) => {
    // Key lock starts on the narrow range, as on a deck; it can be widened.
    if (next) setRange(rangeFor(v.speed));
    st.editVarispeed({ keep_pitch: next });
  };
  const applied = status?.active ? status.speed : null;

  const chooseRange = (r: VarispeedRange) => {
    setRange(r);
    const clamped = clampSpeed(v.speed, r);
    if (clamped !== v.speed) st.editVarispeed({ speed: clamped });
  };

  return (
    <div className={"panel varispeed-panel" + (on && !direct ? "" : " off")} aria-labelledby="varispeed-title" title={direct ? DIRECT_TITLE : undefined}>
      <div className="varispeed-head">
        <h2 id="varispeed-title">Varispeed</h2>
      </div>
      <p className="varispeed-note">Changes speed and pitch together.</p>
      <div className="row">
        <span className="k" title="How far the slider reaches">
          Range
        </span>
        <Seg<VarispeedRange>
          label="Varispeed range"
          value={shownRange}
          disabled={disabled}
          onChange={chooseRange}
          options={VARISPEED_RANGES.map((r) => [r, `±${r} %`, RANGE_TITLE[r]] as [VarispeedRange, string, string])}
        />
        <label
          className={"ups-check varispeed-kp" + (disabled || !kpSupported ? " disabled" : "")}
          title={direct ? DIRECT_TITLE : kpSupported ? KEEP_PITCH_TITLE : KEEP_PITCH_OLD_ENGINE}
        >
          <input
            type="checkbox"
            checked={keepPitch}
            disabled={disabled || !kpSupported}
            onChange={(e) => toggleKeepPitch(e.target.checked)}
          />
          <span>Keep pitch</span>
        </label>
      </div>
      <div className="row">
        <label className="k" htmlFor="varispeed-speed" title="Arrow keys: 0.1 %. Double-click the value to type it.">
          Speed
        </label>
        <ValueSlider
          id="varispeed-speed"
          label="Varispeed speed"
          wide
          value={v.speed}
          min={min}
          max={max}
          step={0.001}
          disabled={disabled}
          format={(s) => `${s.toFixed(3)}×`}
          title={direct ? DIRECT_TITLE : RANGE_TITLE[shownRange]}
          onChange={(s, mode) => {
            // Moving the speed turns the page on, as a pitch shift does.
            if (!on) st.setPitchPageEnabled(true);
            st.editVarispeed({ speed: clampSpeed(s, shownRange) }, mode);
          }}
          onEnd={() => st.endVarispeedDrag()}
        />
        <span className="hint mono" aria-live="polite">
          {speedReadout(v.speed, keepPitch)}
          {on && applied !== null && Math.abs(applied - v.speed) > 5e-4 ? ` (now ${applied.toFixed(3)}×)` : ""}
        </span>
        <button className="btn small" disabled={disabled || v.speed === 1} title="Back to normal speed" onClick={() => st.editVarispeed({ speed: 1 })}>
          Reset to 1×
        </button>
      </div>
      {soften && (
        <p className="varispeed-note">Transients soften this far from 1× (drums and plucked notes sound less sharp). Within ±25 % they stay crisp.</p>
      )}
      {note && <p className="varispeed-note warn">{note}</p>}
      {error && (
        <div className="inline-err" role="alert">
          <span>{error}</span>
          <button className="icon-btn" aria-label="Dismiss" onClick={() => st.clearVarispeedError()}>
            <Close />
          </button>
        </div>
      )}
    </div>
  );
}
