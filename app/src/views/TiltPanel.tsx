// The EQ's tilt (`EqProfile.tilt`, and `tilt_high` for Both): a global slope across the band list,
// sent with the rest of the EQ (`set_eq`), so it is in the curve, in the
// auto preamp and in undo. The engine reports how it reads the tilt at the
// current rate (`status.eq_tilt`): effective band, clamp note, error.

import { useRef } from "react";
import type { TiltConfig } from "../api/types";
import { EQ_PROFILE_VERSION } from "../lib/chain";
import { fmtFreq } from "../lib/format";
import { TILT_HI_HZ, TILT_LO_HZ, TILT_MAX_SLOPE, TILT_MAX_TOTAL, type TiltPair, type TiltShape, clampCorner, cornerText, defaultTilt, pairWithShape, tiltAs, tiltPairProblem, tiltProblem, tiltRanges, tiltShape, tiltSlope, tiltTotal, withBoth, withShape } from "../lib/tilt";
import { store, useApp } from "../store/store";
import { Power } from "../components/Icons";
import { Seg } from "../components/Seg";
import { CompareButton, InlineError, NumField } from "../components/StageParts";
import { ValueSlider } from "../components/ValueSlider";

type Mode = "drag" | "commit";

export function TiltPanel() {
  const st = store();
  useApp((s) => s.eqDraft);
  useApp((s) => s.config?.chain.eq);
  useApp((s) => s.eqOff);
  const status = useApp((s) => s.status?.eq_tilt ?? null);
  const supported = useApp((s) => s.stageCaps.tilt !== false);
  const rate = useApp((s) => s.status?.graph_rate ?? s.config?.rate.default_rate ?? 48000);
  const dragging = useRef(false);
  const eq = st.eq();
  const tilt = eq?.tilt ?? null;
  const high = eq?.tilt_high ?? null;
  const on = !!tilt?.enabled || !!high?.enabled;
  const t: TiltConfig = tilt ?? defaultTilt();
  const disabled = !eq || !supported;
  const byTotal = t.total_db != null;
  const problem = !on
    ? null
    : ((high && tilt ? tiltPairProblem(tilt, high) : null) ?? [t, high].map((s) => (s?.enabled ? tiltProblem(s, rate) : null)).find((e) => e) ?? null);

  // Every edit goes through the pair: the plain (or bass) section and, for Both, the treble one.
  const editPair = (fn: (cur: TiltPair) => TiltPair, mode: Mode = "commit") => {
    const eqFn = (cur: typeof eq) => {
      if (!cur) return cur;
      const p = fn({ tilt: cur.tilt ?? defaultTilt(), tilt_high: cur.tilt_high ?? null });
      const next = { ...cur, version: EQ_PROFILE_VERSION, tilt: p.tilt };
      if (p.tilt_high) next.tilt_high = p.tilt_high;
      else delete next.tilt_high;
      return next;
    };
    const edit = (e: Parameters<typeof st.editEq>[0], m: Mode, record?: boolean) => st.editEq(e, m, record);
    if (mode === "drag") {
      if (!dragging.current) {
        dragging.current = true;
        st.beginEqDrag();
      }
      edit(eqFn, "drag");
      return;
    }
    if (dragging.current) {
      dragging.current = false;
      edit(eqFn, "commit", false);
      return;
    }
    edit(eqFn, "commit");
  };
  const edit = (fn: (cur: TiltConfig) => TiltConfig, mode: Mode = "commit") => editPair((p) => ({ ...p, tilt: fn(p.tilt) }), mode);
  const endDrag = () => {
    if (!dragging.current) return;
    dragging.current = false;
    st.endEqDrag();
  };

  // Shown as the closest shape; the band is only rewritten on an edit here.
  const shape: TiltShape = high ? "both" : tiltShape(t).shape;
  const corner = tiltShape(t).hz;
  const pivotKind: "center" | "hz" | null = t.pivot === "center" ? "center" : typeof t.pivot === "object" ? "hz" : null;
  // An amount edit keeps the tilt's on/off as it is (only its power button changes that).
  const amountEdit = (which: "low" | "high") => (fn: (cur: TiltConfig) => TiltConfig, mode: Mode) =>
    editPair(
      (p) => ({
        tilt: which === "low" ? fn(p.tilt) : p.tilt,
        tilt_high: p.tilt_high && (which === "high" ? fn(p.tilt_high) : p.tilt_high),
      }),
      mode,
    );

  const engineError = on ? (status?.error ?? null) : null;
  const last = high ?? t;
  const freqSlider = (id: string, label: string, value: number, onChange: (v: number, mode: Mode) => void) => (
    <ValueSlider id={id} label={label} value={value} min={TILT_LO_HZ} max={TILT_HI_HZ} step={1} log disabled={disabled} format={(v) => cornerText(v)} title="20 Hz to 20 kHz" onChange={onChange} onEnd={endDrag} />
  );

  return (
    <div className={"panel tilt-panel" + (on ? "" : " off")} aria-labelledby="tilt-title">
      <div className="tilt-head">
        <button
          className="pwr"
          aria-pressed={on}
          aria-label="Tilt on"
          disabled={disabled}
          title={!supported ? "This engine version has no tilt. Update the engine to use it." : !eq ? "Turn the EQ on to use the tilt" : on ? "Turn the tilt off (its settings are kept)" : "Turn the tilt on"}
          onClick={() => editPair((p) => ({ tilt: { ...p.tilt, enabled: !on }, tilt_high: p.tilt_high && { ...p.tilt_high, enabled: !on } }))}
        >
          <Power />
        </button>
        <h2 id="tilt-title" title="A constant slope in dB per octave, for every channel; counts toward the auto preamp">
          Tilt
        </h2>
        <Seg<TiltShape>
          label="Tilt shape"
          value={shape}
          disabled={disabled}
          onChange={(k) => editPair((p) => pairWithShape(p, k))}
          options={[
            ["whole", "Whole band", "Tilt 20 Hz to 20 kHz"],
            ["above", "Above", "Flat below the frequency, tilted above it"],
            ["below", "Below", "Tilted below the frequency, flat above it"],
            ["both", "Both", "Tilted below one frequency and above another, flat between"],
          ]}
        />
        <Seg<"slope" | "total">
          label="Tilt amount as"
          className="quiet"
          value={byTotal ? "total" : "slope"}
          disabled={disabled}
          onChange={(m) => editPair((p) => ({ tilt: tiltAs(p.tilt, m), tilt_high: p.tilt_high && tiltAs(p.tilt_high, m) }))}
          options={[
            ["slope", "dB/oct", `A slope in dB per octave, ±${TILT_MAX_SLOPE}`],
            ["total", "Total dB", `The change across the tilted part, ±${TILT_MAX_TOTAL} dB`],
          ]}
        />
        <span className="spacer" />
        {supported && <CompareButton stage="tilt" />}
      </div>
      {/* One side per tilted part: its corner (or the pivot) and its amount. */}
      <div className="row tilt-sides">
        {shape === "both" && high ? (
          <>
            <div className="tilt-side stack">
              <span className="k">Bass</span>
              <div className="tilt-line">{freqSlider("tilt-corner", "Bass tilt frequency", t.end_hz, (v, mode) => editPair((p) => (p.tilt_high ? withBoth(p.tilt, p.tilt_high, v, p.tilt_high.start_hz, "bass") : p), mode))}</div>
              <div className="tilt-line">
                <Amount id="tilt" label="Bass tilt" t={t} disabled={disabled} onChange={amountEdit("low")} onEnd={endDrag} />
              </div>
            </div>
            <div className="tilt-side stack">
              <span className="k">Treble</span>
              <div className="tilt-line">{freqSlider("tilt-high-corner", "Treble tilt frequency", high.start_hz, (v, mode) => editPair((p) => (p.tilt_high ? withBoth(p.tilt, p.tilt_high, p.tilt.end_hz, v, "treble") : p), mode))}</div>
              <div className="tilt-line">
                <Amount id="tilt-high" label="Treble tilt" t={high} disabled={disabled} onChange={amountEdit("high")} onEnd={endDrag} />
              </div>
            </div>
          </>
        ) : (
          <div className="tilt-side">
            {shape === "whole" ? (
              <>
                <span className="k" title="The frequency that stays at 0 dB">
                  Pivot
                </span>
                <Seg<"center" | "hz">
                  label="Tilt pivot"
                  value={pivotKind}
                  disabled={disabled}
                  onChange={(k) => edit((c) => ({ ...c, pivot: k === "hz" ? { hz: 1000 } : "center" }))}
                  options={[
                    ["center", "Centre", "0 dB at the middle of the band (630 Hz)"],
                    ["hz", "Hz", "0 dB at a frequency you choose"],
                  ]}
                />
                {typeof t.pivot === "object" && (
                  <NumField
                    label="Tilt pivot in Hz"
                    value={t.pivot.hz}
                    unit="Hz"
                    digits={0}
                    disabled={disabled}
                    onCommit={(v) => v !== null && edit((c) => ({ ...c, pivot: { hz: Math.round(clampCorner("whole", v)) } }))}
                  />
                )}
              </>
            ) : (
              freqSlider("tilt-corner", "Tilt frequency", corner, (v, mode) => edit((c) => withShape(c, shape === "below" ? "below" : "above", v), mode))
            )}
            <Amount id="tilt" label="Tilt" t={t} disabled={disabled} onChange={amountEdit("low")} onEnd={endDrag} />
          </div>
        )}
      </div>
      {!supported && <p className="tilt-note">This engine version has no tilt. Update the engine to use it.</p>}
      {on && status?.clamp_note && Math.log2(last.end_hz / status.effective_end_hz) > 0.1 && <p className="tilt-note" title={status.clamp_note}>The end is capped at 0.45 × the sample rate here ({fmtFreq(status.effective_end_hz)} Hz).</p>}
      {(engineError || problem) && (
        <p className="tilt-note bad" role="alert">
          {engineError ?? problem}
        </p>
      )}
      <InlineError slot="eq" />
    </div>
  );
}

/** One section's amount: a slope or a total slider, and the other figure as a hint. */
function Amount(props: { id: string; label: string; t: TiltConfig; disabled: boolean; onChange: (fn: (cur: TiltConfig) => TiltConfig, mode: Mode) => void; onEnd: () => void }) {
  const { id, label, t, disabled, onChange, onEnd } = props;
  const byTotal = t.total_db != null;
  const slope = tiltSlope(t) ?? 0;
  const total = tiltTotal(t) ?? 0;
  // Both limits hold at once, so the band decides how far each slider goes.
  const range = tiltRanges(t);
  const sign = (v: number) => (v > 0 ? "+" : v < 0 ? "−" : "");
  return (
    <>
      {byTotal ? (
        <ValueSlider
          id={`${id}-total`}
          label={`${label} total`}
          value={total}
          min={-range.total}
          max={range.total}
          step={0.1}
          disabled={disabled}
          format={(v) => `${sign(v)}${Math.abs(v).toFixed(1)} dB`}
          title={`Change across the tilted part, up to ±${range.total.toFixed(1)} dB over this band (slope within ±${TILT_MAX_SLOPE} dB/oct)`}
          onChange={(v, mode) => onChange((c) => ({ ...c, slope_db_per_oct: null, total_db: v }), mode)}
          onEnd={onEnd}
        />
      ) : (
        <ValueSlider
          id={`${id}-slope`}
          label={`${label} slope`}
          value={slope}
          min={-range.slope}
          max={range.slope}
          step={0.05}
          disabled={disabled}
          format={(v) => `${sign(v)}${Math.abs(v).toFixed(2)} dB/oct`}
          title={`Slope, up to ±${range.slope.toFixed(2)} dB per octave here (total within ±${TILT_MAX_TOTAL} dB); negative tilts the treble down`}
          onChange={(v, mode) => onChange((c) => ({ ...c, total_db: null, slope_db_per_oct: v }), mode)}
          onEnd={onEnd}
        />
      )}
      <span className="hint mono">{byTotal ? `${sign(slope)}${Math.abs(slope).toFixed(2)} dB/oct` : `${sign(total)}${Math.abs(total).toFixed(1)} dB total`}</span>
    </>
  );
}
