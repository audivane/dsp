import { useEffect, useRef, useState } from "react";
import type { LimiterConfig, LimiterMode } from "../api/types";
import { LIMITER_CEILING, LIMITER_LOOKAHEAD, LIMITER_MODES, LIMITER_RELEASE, defaultLimiter, limiterDelay, limiterEventLabel, limiterMeterState, RollingMax } from "../lib/effects";
import { clamp, fmtDb, fmtInt, fmtRate } from "../lib/format";
import { meterStore } from "../store/meters";
import { store, useApp } from "../store/store";
import { Seg } from "../components/Seg";
import { InlineError, MissingNotice, NumField, StageHead, useStageMissing } from "../components/StageParts";
import { ValueSlider } from "../components/ValueSlider";
import { ClipWarning } from "../components/ClipWarning";
import { StepSlider } from "../components/StepSlider";

const GR_RANGE_DB = 12;
const PEAK_HOLD_MS = 3000;
const HEADROOM_RANGE = { min: 0, max: 12, step: 0.5 } as const;

/** The fixed output headroom (`output.headroom_db`): a cut before the volume
 *  that works with the limiter off too, so it is never dimmed by its power. */
function HeadroomRow() {
  const st = store();
  const value = useApp((s) => s.config?.chain.output.headroom_db);
  const [draft, setDraft] = useState<number | null>(null);
  if (value === undefined) return null;
  const set = (v: number) => st.setOutput({ headroom_db: Math.round(clamp(v, 0, 12) * 10) / 10 });
  return (
    <div className="row">
      <label className="k" htmlFor="out-headroom" title="A fixed level cut before the volume and the limiter, 0 to 12 dB. It leaves room for peaks rebuilt between samples (loud masters reach about +3 dB). Works with the limiter off too.">
        Headroom
      </label>
      <StepSlider
        id="out-headroom"
        className="headroom-slider"
        label="Headroom slider"
        value={value}
        range={HEADROOM_RANGE}
        format={(v) => `${v.toFixed(1)} dB`}
        onDraft={setDraft}
        onCommit={set}
      />
      <NumField
        label="Headroom"
        unit="dB"
        value={draft ?? value}
        title="Turns the processed signal down by this much, 0 to 12 dB. 0 dB changes nothing; above 0 turns processing on."
        check={(v) => (v === null ? "Enter a number" : v < 0 || v > 12 ? "0 to 12 dB" : null)}
        onCommit={(v) => v !== null && set(v)}
      />
      <span className="hint">Always on, even with the limiter off.</span>
    </div>
  );
}

/**
 * Gain reduction from `Meters.limiter`: the bar is the window's reduction
 * now, the marker the largest over the last 3 s; next to it the applied
 * automatic headroom and the event count. Painted per animation frame from
 * the meter buffer, outside React.
 */
function ReductionMeter({ mode }: { mode: LimiterMode }) {
  const bar = useRef<HTMLSpanElement>(null);
  const peak = useRef<HTMLSpanElement>(null);
  const now = useRef<HTMLDivElement>(null);
  const held = useRef<HTMLDivElement>(null);
  const headroom = useRef<HTMLDivElement>(null);
  const events = useRef<HTMLDivElement>(null);
  const state = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    let raf = 0;
    // A rolling maximum: a newer, smaller peak still shows once the larger one ages out.
    const peaks = new RollingMax(PEAK_HOLD_MS);
    let lastAt = -1;
    const text = (el: HTMLElement | null, t: string) => {
      if (el && el.textContent !== t) el.textContent = t;
    };
    const frame = (t: number) => {
      raf = requestAnimationFrame(frame);
      const m = meterStore.latest;
      const lim = m && t - m.at < 500 ? m.meters.limiter : undefined;
      const meter = limiterMeterState(lim, store().getState().status?.state);
      const gr = meter.inPath ? Math.max(0, lim!.gain_reduction_db) : 0;
      // Push each reading once, at the time it arrived, so a stale one ages out on time.
      if (m && lim && m.at !== lastAt) {
        lastAt = m.at;
        peaks.push(m.at, gr);
      }
      const hold = peaks.max(t);
      const pct = (db: number) => `${Math.min(100, (db / GR_RANGE_DB) * 100)}%`;
      if (bar.current) bar.current.style.width = pct(gr);
      if (peak.current) {
        peak.current.style.left = pct(hold);
        peak.current.style.opacity = hold > 0.05 ? "1" : "0";
      }
      text(now.current, meter.inPath ? `${fmtDb(-gr, 1)} dB` : "—");
      text(held.current, meter.inPath ? `${fmtDb(-hold, 1)} dB` : "—");
      text(headroom.current, meter.inPath ? `${fmtDb(-(lim!.headroom_db ?? 0), 1)} dB` : "—");
      text(events.current, lim ? fmtInt(lim.events) : "—");
      text(state.current, meter.text);
    };
    raf = requestAnimationFrame(frame);
    return () => cancelAnimationFrame(raf);
  }, []);
  return (
    <div className="gr">
      <div className="gr-head">
        <span className="k" title={`Gain reduction: how far the limiter is pulling the level down now (bar) and the most in the last ${PEAK_HOLD_MS / 1000} s (marker). Scale 0 to ${GR_RANGE_DB} dB.`}>
          Gain reduction
        </span>
        <span className="hint" ref={state} aria-live="polite" />
      </div>
      <div className="gr-track" role="img" aria-label="Gain reduction meter">
        <span className="gr-fill" ref={bar} />
        <span className="gr-peak" ref={peak} />
      </div>
      <div className="gr-scale mono" aria-hidden="true">
        {[0, 3, 6, 9, 12].map((d) => (
          <span key={d}>{d === 0 ? "0" : `−${d}`}</span>
        ))}
      </div>
      <div className="facts">
        <div className="fact" title="The largest reduction in the last meter window (1/30 s)">
          <div className="k">Now</div>
          <div className="v" ref={now}>—</div>
        </div>
        <div className="fact" title={`The largest reduction in the last ${PEAK_HOLD_MS / 1000} s`}>
          <div className="k">Peak</div>
          <div className="v" ref={held}>—</div>
        </div>
        <div className="fact" title="Auto headroom: the level drop applied now (what the music needs minus what the volume already takes off). Limit mode keeps it at 0.">
          <div className="k">Applied headroom</div>
          <div className="v" ref={headroom}>—</div>
        </div>
        <div
          className="fact"
          title={
            mode === "auto_headroom"
              ? "Headroom steps since the engine started: each time the level had to come down another 0.5 dB."
              : "Onsets since the engine started: each time limiting began after the gain had recovered."
          }
        >
          <div className="k">{limiterEventLabel(mode)}</div>
          <div className="v" ref={events}>—</div>
        </div>
      </div>
    </div>
  );
}

export function LimiterView() {
  const st = store();
  useApp((s) => s.config?.chain.limiter);
  useApp((s) => s.stageDrafts.limiter);
  const outputRate = useApp((s) => s.status?.output_rate || s.status?.graph_rate || 0);
  const missing = useStageMissing("limiter");
  const lim: LimiterConfig = st.stage("limiter") ?? defaultLimiter();
  const on = lim.mode !== "off";
  const delay = limiterDelay(lim.lookahead_ms, outputRate);
  const edit = (fn: (c: LimiterConfig) => LimiterConfig, mode: "drag" | "commit" = "commit") => st.editStage("limiter", (c) => fn(c ?? defaultLimiter()), mode);
  const offMode = useApp((s) => s.limiterOffMode);
  return (
    <section className="view" aria-labelledby="lim-title">
      <StageHead id="lim-title" title="Limiter" on={on} onPower={missing ? undefined : () => st.setStageEnabled("limiter", !on)} />
      {missing && <MissingNotice id="limiter" />}
      {!missing && <ClipWarning />}
      <div className="panel">
        <HeadroomRow />
      </div>
      <div className="panel">
        <div className="row">
          <span className="k" title="What the stage does with peaks above the ceiling.">
            Mode
          </span>
          <Seg<LimiterMode>
            label="Limiter mode"
            value={lim.mode === "off" ? (offMode ?? "auto_headroom") : lim.mode}
            disabled={missing}
            onChange={(mode) => st.setLimiterMode(mode)}
            options={LIMITER_MODES.filter((m) => m.mode !== "off").map((m) => [m.mode, m.label, m.title] as [LimiterMode, string, string])}
          />
        </div>
        <div className="row">
          <label className="k" htmlFor="lim-ceiling" title="The highest level allowed into the DAC, −6 to 0 dBTP (true peak: between samples too). −0.5 leaves room for the DAC's own filter.">
            Ceiling
          </label>
          <ValueSlider
            id="lim-ceiling"
            label="Limiter ceiling"
            value={lim.ceiling_dbfs}
            min={LIMITER_CEILING.min}
            max={LIMITER_CEILING.max}
            step={0.1}
            disabled={missing}
            format={(v) => `${fmtDb(v, 1)} dBTP`}
            title="−6 to 0 dBTP (default −0.5)"
            onChange={(v, mode) => edit((c) => ({ ...c, ceiling_dbfs: v }), mode)}
            onEnd={() => st.endStageDrag("limiter")}
          />
        </div>
        <div className="row">
          <label className="k" htmlFor="lim-release" title="How fast the gain recovers after a peak, 10 to 2000 ms. Short is transparent on single peaks but can pump on dense music; long is smoother.">
            Release
          </label>
          <ValueSlider
            id="lim-release"
            label="Limiter release"
            value={lim.release_ms}
            min={LIMITER_RELEASE.min}
            max={LIMITER_RELEASE.max}
            step={1}
            log
            disabled={missing}
            format={(v) => `${Math.round(v)} ms`}
            title="10 to 2000 ms (default 100)"
            onChange={(v, mode) => edit((c) => ({ ...c, release_ms: v }), mode)}
            onEnd={() => st.endStageDrag("limiter")}
          />
        </div>
        <div className="row">
          <label className="k" htmlFor="lim-look" title="How far ahead the limiter looks to start lowering the gain before a peak, 5 to 20 ms. This is the delay the stage adds.">
            Look-ahead
          </label>
          <ValueSlider
            id="lim-look"
            label="Limiter look-ahead"
            value={lim.lookahead_ms}
            min={LIMITER_LOOKAHEAD.min}
            max={LIMITER_LOOKAHEAD.max}
            step={0.5}
            disabled={missing}
            format={(v) => `${v.toFixed(1)} ms`}
            title="5 to 20 ms (default 5)"
            onChange={(v, mode) => edit((c) => ({ ...c, lookahead_ms: v }), mode)}
            onEnd={() => st.endStageDrag("limiter")}
          />
          <span className="hint" title="ceil(output rate × look-ahead): whole frames at the rate the DAC runs at">
            {on ? "Adds" : "Would add"} {delay.ms.toFixed(2)} ms
            {outputRate ? ` (${fmtInt(delay.frames)} frames at ${fmtRate(outputRate)})` : ""}
          </span>
        </div>
        <InlineError slot="limiter" />
        <ReductionMeter mode={lim.mode} />
      </div>
    </section>
  );
}
