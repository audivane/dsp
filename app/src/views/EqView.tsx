import { useEffect, useRef, useState } from "react";
import { hasSection, sectionEnabled, sectionSet, type EqSection } from "../lib/chain";
import { channelsMatch, profilePeakGainDb } from "../lib/eqmath";
import { fmtDb, parseNumber } from "../lib/format";
import { EQ_MIN_HEIGHT, MAX_PLOT_HEIGHT, PLOT_HEIGHT_KEYS, usePersistentHeight } from "../lib/plotHeight";
import { store, useApp } from "../store/store";
import { Download, Power, Redo, Undo } from "../components/Icons";
import { OverlayMenu } from "../components/OverlayMenu";
import { CompareButton } from "../components/StageParts";
import { ResizeHandle } from "../components/ResizeHandle";
import { BandStrip } from "./BandStrip";
import { EqGraph } from "./EqGraph";
import { SpectrumControls } from "./SpectrumLayer";
import { RangePresets } from "../components/RangePresets";
import { TiltPanel } from "./TiltPanel";
import { TonePanel } from "./TonePanel";
import { PresetControl } from "../components/PresetMenu";

const SECTIONS: { id: EqSection; label: string; title: string }[] = [
  { id: "linked", label: "L+R", title: "Shared EQ applied to both channels" },
  { id: "left", label: "L", title: "Extra EQ applied to the left channel" },
  { id: "right", label: "R", title: "Extra EQ applied to the right channel" },
];

export function ImportApoButton({ className = "btn", label = "Import", onText }: { className?: string; label?: string; onText: (text: string, name: string) => void }) {
  const input = useRef<HTMLInputElement>(null);
  return (
    <>
      <button className={className} title="Import an AutoEQ or Equalizer APO ParametricEQ.txt" onClick={() => input.current?.click()}>
        <Download />
        {label}
      </button>
      <input
        ref={input}
        type="file"
        accept=".txt,text/plain"
        hidden
        onChange={(e) => {
          const f = e.target.files?.[0];
          e.target.value = "";
          if (!f) return;
          void f.text().then((t) => onText(t, f.name));
        }}
      />
    </>
  );
}

function Preamp({ value }: { value: number }) {
  const auto = useApp((s) => s.autoPreamp);
  const [text, setText] = useState<string | null>(null);
  const st = store();
  return (
    <div className="preamp">
      <span id="preamp-label">Preamp</span>
      {auto ? (
        <b className="num">{fmtDb(value, 1, false)} dB</b>
      ) : (
        <input
          aria-labelledby="preamp-label"
          value={text ?? fmtDb(value, 1, false)}
          onChange={(e) => setText(e.target.value)}
          onBlur={() => {
            const v = text === null ? null : parseNumber(text);
            setText(null);
            if (v !== null && v !== value) st.setManualPreamp(Math.max(-60, Math.min(12, Math.round(v * 10) / 10)));
          }}
          onKeyDown={(e) => e.key === "Enter" && (e.target as HTMLInputElement).blur()}
        />
      )}
      <button
        className="tag"
        aria-pressed={auto}
        title={auto ? "Preamp follows the largest boost the curve shows (an estimate when a tilt is on, within about 0.2 dB), so the EQ should not clip. Select to set it by hand." : "Set by hand. Select to follow the largest boost again."}
        onClick={() => st.setAutoPreamp(!auto)}
      >
        auto
      </button>
    </div>
  );
}

export function EqView() {
  const st = store();
  const mode = useApp((s) => s.eqMode);
  const sel = useApp((s) => s.selectedBand);
  const canUndo = useApp((s) => s.canUndo);
  const range = useApp((s) => s.eqRange);
  const canRedo = useApp((s) => s.canRedo);
  const caps = useApp((s) => s.caps);
  const rate = useApp((s) => s.status?.graph_rate ?? s.config?.rate.default_rate ?? 48000);
  useApp((s) => s.eqDraft);
  useApp((s) => s.config?.chain.eq);
  useApp((s) => s.eqOff);
  const eqOn = useApp((s) => !!s.config?.chain.eq);
  const eq = st.eq();
  const set = sectionSet(eq, mode);
  const hasCur = hasSection(eq, mode);
  const same = eq ? channelsMatch(eq, rate) : true;
  const curOn = sectionEnabled(eq, mode);
  const showDashed = !!eq && hasCur && (mode !== "linked" || !same || !curOn);
  const curLabel = SECTIONS.find((s) => s.id === mode)?.label ?? "L+R";
  const curDashColor = mode === "left" ? "var(--ch-l)" : mode === "right" ? "var(--ch-r)" : same ? "var(--accent)" : "var(--fg)";
  const peakGainDb = eq ? profilePeakGainDb(eq, rate) : 0;

  const eqStageRef = useRef<HTMLDivElement>(null);
  const [measuredHeight, setMeasuredHeight] = useState(300);
  const eqHeight = usePersistentHeight({
    storageKey: PLOT_HEIGHT_KEYS.eq,
    defaultHeight: null,
    minHeight: EQ_MIN_HEIGHT,
    maxHeight: MAX_PLOT_HEIGHT,
  });

  useEffect(() => {
    const el = eqStageRef.current;
    if (!el) return;
    const update = () => {
      const h = Math.round(el.getBoundingClientRect().height);
      if (h > 0) setMeasuredHeight((prev) => (prev === h ? prev : h));
    };
    const ro = new ResizeObserver(update);
    ro.observe(el);
    update();
    return () => ro.disconnect();
  }, []);

  const showHint = !!eq && set.bands.length > 0;
  const showLegend = !same || showDashed;

  return (
    <section className="view eq-view" aria-labelledby="eq-title">
      <div className="view-head">
        <button className="pwr stage-pwr" aria-pressed={eqOn} aria-label="Parametric EQ on" title={eqOn ? "Turn the EQ off (the bands are kept)" : "Turn the EQ on"} onClick={() => st.setEqEnabled(!eqOn)}>
          <Power />
        </button>
        <h1 id="eq-title">Parametric EQ</h1>
        <span className={"tag" + (eqOn ? " on" : "")}>{eqOn ? "On" : "Off"}</span>
        <div className="sec-tabs" role="group" aria-label="EQ sections">
          {SECTIONS.map((sec) => {
            const present = hasSection(eq, sec.id);
            const on = sectionEnabled(eq, sec.id);
            const noSplit = sec.id !== "linked" && caps.perChannel === false;
            if (sec.id !== "linked" && !present) {
              return (
                <div key={sec.id} className="sec-tab absent">
                  <button
                    className="sec-sel"
                    disabled={!eq || noSplit}
                    title={`Add ${sec.id} channel section`}
                    onClick={() => st.addSection(sec.id as "left" | "right")}
                  >
                    <span>{sec.label}</span>
                    <span className="sec-add">+ add</span>
                  </button>
                </div>
              );
            }
            return (
              <div
                key={sec.id}
                className={"sec-tab" + (mode === sec.id ? " active" : "") + (!on ? " off" : "")}
              >
                <button
                  className="pwr"
                  aria-pressed={on}
                  aria-label={`${sec.label} section on`}
                  title={on ? `Turn ${sec.label} section off` : `Turn ${sec.label} section on`}
                  disabled={!eq}
                  onClick={() => st.toggleSectionEnabled(sec.id)}
                >
                  <Power />
                </button>
                <button
                  className="sec-sel"
                  aria-pressed={mode === sec.id}
                  disabled={!eq || noSplit}
                  title={sec.title}
                  onClick={() => st.setEqSection(sec.id)}
                >
                  <span>{sec.label}</span>
                </button>
              </div>
            );
          })}
        </div>
        {eq && <Preamp value={eq.preamp_db} />}
        {eq && peakGainDb > 0.05 && (
          <div className="clip-warn" role="status">
            <span>Max boost {eq.tilt?.enabled || eq.tilt_high?.enabled ? "about " : ""}{fmtDb(peakGainDb, 1)} dB exceeds preamp</span>
            <button className="tag" onClick={() => st.setAutoPreamp(true)}>
              Auto preamp
            </button>
          </div>
        )}
        {eq && hasCur && (
          <button
            className="btn small"
            disabled={set.bands.length === 0 && (mode === "linked" || set.preamp_db === 0)}
            title={`Clear all bands in ${curLabel}`}
            onClick={() => st.clearSection(mode)}
          >
            Clear
          </button>
        )}
        {eq && mode !== "linked" && hasCur && (
          <button
            className="btn small danger"
            title={`Remove the ${curLabel} section`}
            onClick={() => st.removeSection(mode)}
          >
            Remove
          </button>
        )}
        <div className="spacer" />
        <RangePresets x={range} onX={(r) => st.setEqRange(r)} className="eq-range" />
        <CompareButton stage="eq" />
        <PresetControl part="eq" />
        <SpectrumControls />
        <button className="icon-btn" aria-label="Undo" title="Undo (Ctrl+Z)" disabled={!canUndo} onClick={() => st.undo()}>
          <Undo />
        </button>
        <button className="icon-btn" aria-label="Redo" title="Redo (Ctrl+Shift+Z)" disabled={!canRedo} onClick={() => st.redo()}>
          <Redo />
        </button>
        <ImportApoButton onText={(t, n) => st.importApoToEq(t, n)} />
        <OverlayMenu />
      </div>
      <div className={"graph-wrap" + (eqHeight.height !== null ? " custom-h" : "")}>
        {(showHint || showLegend) && (
          <div className="graph-head">
            {showHint && <div className="graph-hint">Drag handles · scroll to change Q · double-click to add a band</div>}
            {showLegend && (
              <div className="legend">
                {!same ? (
                  <>
                    <span style={{ color: "var(--ch-l)" }}>
                      <i style={{ background: "var(--ch-l)" }} />
                      <span style={{ color: "var(--muted)" }}>Left</span>
                    </span>
                    <span style={{ color: "var(--ch-r)" }}>
                      <i style={{ background: "var(--ch-r)" }} />
                      <span style={{ color: "var(--muted)" }}>Right</span>
                    </span>
                  </>
                ) : (
                  <span style={{ color: "var(--accent)" }}>
                    <i style={{ background: "var(--accent)" }} />
                    <span style={{ color: "var(--muted)" }}>L &amp; R</span>
                  </span>
                )}
                {showDashed && (
                  <span style={{ color: curDashColor }}>
                    <i className="dash" />
                    <span style={{ color: "var(--muted)" }}>{curLabel} section</span>
                  </span>
                )}
              </div>
            )}
          </div>
        )}
        <div
          ref={eqStageRef}
          className="graph-stage"
          style={eqHeight.height !== null ? { height: eqHeight.height } : undefined}
        >
          <EqGraph />
        </div>
        <ResizeHandle
          label="Resize EQ graph height"
          value={eqHeight.height}
          measured={measuredHeight}
          min={eqHeight.minHeight}
          max={eqHeight.maxHeight}
          onChange={eqHeight.setHeight}
          onCommit={eqHeight.commitHeight}
          onStep={eqHeight.stepHeight}
          onReset={eqHeight.resetHeight}
          measure={() => eqStageRef.current?.getBoundingClientRect().height ?? measuredHeight}
        />
      </div>
      {eq && <TonePanel />}
      {eq && <TiltPanel />}
      {eq && <BandStrip bands={set.bands} selected={sel} disabled={false} />}
    </section>
  );
}
