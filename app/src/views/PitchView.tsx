import type { PitchConfig, PitchQuality } from "../api/types";
import {
  PITCH_CENTS,
  PITCH_QUALITY,
  effectivePitchQuality,
  fmtCents,
  pitchCents,
  pitchUsesTuning,
  pitchWithCents,
  pitchWithTuning,
  pitchAsCents,
  pitchAsTuning,
} from "../lib/effects";
import { fmtRate } from "../lib/format";
import { store, useApp } from "../store/store";
import { Seg } from "../components/Seg";
import { CompareButton, InlineError, MissingNotice, NumField, StageHead, useStageMissing } from "../components/StageParts";
import { ValueSlider } from "../components/ValueSlider";
import { VarispeedPanel } from "./VarispeedPanel";

const DEFAULT_PITCH: PitchConfig = { enabled: false, cents: 0, quality: "high" };

const TUNINGS: [number, number, string][] = [
  [440, 432, "440 → 432 Hz"],
  [440, 442, "440 → 442 Hz"],
  [440, 415, "440 → 415 Hz (Baroque)"],
];

export function PitchView() {
  const st = store();
  useApp((s) => s.config?.chain.pitch);
  useApp((s) => s.stageDrafts.pitch);
  useApp((s) => s.config?.varispeed);
  useApp((s) => s.varispeedDraft);
  const rate = useApp((s) => s.status?.graph_rate ?? s.config?.rate.default_rate ?? 48000);
  const missing = useStageMissing("pitch");
  const p = st.stage("pitch") ?? DEFAULT_PITCH;
  // One switch for the page: pitch shift and varispeed together. The engine
  // runs varispeed even at 1×, so on is never bit-perfect (it reports that).
  const on = st.pitchPageOn();
  const cents = pitchCents(p);
  const tuning = pitchUsesTuning(p);
  const eff = effectivePitchQuality(p.quality, rate);
  const edit = (fn: (c: PitchConfig) => PitchConfig, mode: "drag" | "commit" = "commit") => {
    // Edits never switch the page on or off: only the power button does.
    st.editStage("pitch", (c) => fn(c ?? DEFAULT_PITCH), mode);
  };
  const setCents = (v: number, mode: "drag" | "commit" = "commit") => edit((c) => pitchWithCents(c, v), mode);
  const powerTitle = on ? "Turn pitch and varispeed off (settings are kept)" : "Turn pitch and varispeed on";
  return (
    <section className="view" aria-labelledby="pitch-title">
      <StageHead
        id="pitch-title"
        title="Pitch"
        part={missing ? undefined : "output"}
        on={on}
        powerTitle={powerTitle}
        onPower={missing ? undefined : () => st.setStageEnabled("pitch", !on)}
      >
        {!missing && <CompareButton stage="pitch" />}
      </StageHead>
      <p className="lede">Shifts the pitch and keeps the tempo.</p>
      {missing && <MissingNotice id="pitch" />}
      <div className="panel">
        <div className="row">
          <span className="k" title="In cents (100 = one semitone) or as a tuning change">
            Shift by
          </span>
          <Seg<"cents" | "tuning">
            label="How to set the shift"
            value={tuning ? "tuning" : "cents"}
            disabled={missing}
            onChange={(m) => edit(m === "cents" ? pitchAsCents : pitchAsTuning)}
            options={[
              ["cents", "Cents", "−1200 to +1200 cents (an octave each way)"],
              ["tuning", "Tuning (Hz)", "From one reference A to another, such as 440 → 432 Hz"],
            ]}
          />
          <span className="hint mono" aria-live="polite">
            Now {fmtCents(cents)}
          </span>
        </div>
        {!tuning ? (
          <>
            <div className="row">
              <label className="k" htmlFor="pitch-cents" title="Arrow keys: 1 cent. Page Up/Down: 100. Double-click the value to type it.">
                Shift
              </label>
              <ValueSlider
                id="pitch-cents"
                label="Pitch shift in cents"
                wide
                value={cents}
                min={PITCH_CENTS.min}
                max={PITCH_CENTS.max}
                step={1}
                disabled={missing}
                format={(v) => fmtCents(v)}
                title="−1200 to +1200 cents"
                onChange={(v, mode) => setCents(v, mode)}
                onEnd={() => st.endStageDrag("pitch")}
              />
            </div>
            <div className="row">
              <span className="k">Steps</span>
              <div className="btn-row" role="group" aria-label="Step the shift">
                <button className="btn small" disabled={missing || cents <= PITCH_CENTS.min} title="Down one semitone (100 cents)" onClick={() => setCents(cents - 100)}>
                  −1 semitone
                </button>
                <button className="btn small" disabled={missing || cents <= PITCH_CENTS.min} title="Down one cent" onClick={() => setCents(cents - 1)}>
                  −1 cent
                </button>
                <button className="btn small" disabled={missing || cents >= PITCH_CENTS.max} title="Up one cent" onClick={() => setCents(cents + 1)}>
                  +1 cent
                </button>
                <button className="btn small" disabled={missing || cents >= PITCH_CENTS.max} title="Up one semitone (100 cents)" onClick={() => setCents(cents + 100)}>
                  +1 semitone
                </button>
                <button className="btn small" disabled={missing || cents === 0} title="Back to the original pitch" onClick={() => setCents(0)}>
                  Reset to 0
                </button>
              </div>
            </div>
          </>
        ) : (
          <div className="row">
            <span className="k" title="The recording's A and the A you want to hear">
              Tuning
            </span>
            <NumField label="Reference A in Hz" value={p.reference_hz ?? 440} unit="Hz" disabled={missing} onCommit={(v) => v !== null && v > 0 && edit((c) => pitchWithTuning(c, v, c.target_hz ?? 432))} />
            <span aria-hidden="true">→</span>
            <NumField label="Target A in Hz" value={p.target_hz ?? 432} unit="Hz" disabled={missing} onCommit={(v) => v !== null && v > 0 && edit((c) => pitchWithTuning(c, c.reference_hz ?? 440, v))} />
            <div className="btn-row">
              {TUNINGS.map(([r, t, label]) => (
                <button key={label} className="btn small" disabled={missing} aria-pressed={p.reference_hz === r && p.target_hz === t} onClick={() => edit((c) => pitchWithTuning(c, r, t))}>
                  {label}
                </button>
              ))}
              <button className="btn small" disabled={missing || p.reference_hz === p.target_hz} title="Back to the original pitch" onClick={() => edit((c) => pitchWithTuning(c, c.reference_hz ?? 440, c.reference_hz ?? 440))}>
                Reset to 0
              </button>
            </div>
          </div>
        )}
        <div className="row">
          <span className="k" title="Higher quality: smoother tone, more delay">
            Quality
          </span>
          <Seg<PitchQuality>
            label="Pitch quality"
            value={p.quality}
            disabled={missing}
            onChange={(quality) => edit((c) => ({ ...c, quality }))}
            options={(["low", "high", "max"] as const).map((q) => [q, `${PITCH_QUALITY[q].label} · ${PITCH_QUALITY[q].latencyMs} ms`, PITCH_QUALITY[q].title] as [PitchQuality, string, string])}
          />
          {eff !== p.quality && (
            <span className="hint">
              Runs as {PITCH_QUALITY[eff].label} at {fmtRate(rate)} to keep up ({PITCH_QUALITY[eff].latencyMs} ms).
            </span>
          )}
        </div>
        <InlineError slot="pitch" />
      </div>
      <VarispeedPanel />
    </section>
  );
}
