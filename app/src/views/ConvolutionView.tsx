import { convList, convSafety, irKey, irName, irPaths, matrixText } from "../lib/chain";
import { fmtDb, fmtInt, fmtMs, fmtRate } from "../lib/format";
import { combinedTaps, irTaps } from "../lib/summary";
import { store, useApp } from "../store/store";
import { Plus, Power, Trash } from "../components/Icons";
import { CompareButton } from "../components/StageParts";
import { StepSlider } from "../components/StepSlider";
import { PresetControl } from "../components/PresetMenu";
import { useState } from "react";
import type { CalibrationConfig } from "../api/types";
import { ConvResponsePanel, useConvResponse, type ConvTarget } from "./ConvResponsePlot";

const GAIN_RANGE = { min: -24, max: 12, step: 0.5 } as const;
const CAP_RANGE = { min: 0, max: 24, step: 0.5 } as const;
const SUBSONIC_RANGE = { min: 5, max: 60, step: 1 } as const;

function GainSlider({ value, onCommit, label }: { value: number; onCommit: (v: number) => void | Promise<unknown>; label: string }) {
  return <StepSlider className="gain" value={value} label={label} range={GAIN_RANGE} format={(v) => `${fmtDb(v)} dB`} onCommit={onCommit} showValue />;
}

export function ConvolutionView() {
  const st = store();
  useApp((s) => s.config?.chain.convolution);
  const info = useApp((s) => s.irInfo);
  // The convolution's own delay (0 when it is not in the path); the engine total is in the status bar.
  const convLatency = useApp((s) => s.status?.conv_latency_ms ?? 0);
  const single = useApp((s) => s.caps.convChain === false);
  const warnings = useApp((s) => s.status?.conv_warnings) ?? [];
  const safety = convSafety(useApp((s) => s.config));
  const list = st.convolution();
  // Every item, the room calibration's included: the facts and the response plot cover it.
  const all = convList(useApp((s) => s.config?.chain));
  const on = all.filter((c) => c.enabled);
  const [target, setTarget] = useState<ConvTarget>({ side: "a", device: null });
  const response = useConvResponse(target);
  // The engine's combined length at the graph rate when the plot shows the
  // main chain A, else the files' own lengths summed.
  const measured = response.data && target.side === "a" && !target.device ? response.data : null;
  const combined = measured ? measured.combined_taps : combinedTaps(all, info);
  return (
    <section className="view" aria-labelledby="conv-title">
      <div className="view-head">
        <h1 id="conv-title">Convolution</h1>
        <div className="spacer" />
        <CompareButton stage="conv" />
        <PresetControl part="convolution" />
        <button className="btn primary" onClick={() => void st.addImpulseResponse()} disabled={single && list.length >= 1}>
          <Plus />
          Add impulse response
        </button>
      </div>
      {single && list.length >= 1 && <div className="notice">This engine version runs one impulse response at a time.</div>}
      {warnings.length > 0 && (
        <div className="notice warn" role="status">
          {warnings.map((w, i) => (
            <div key={i}>{w.message}</div>
          ))}
        </div>
      )}
      <CalibrationLine />
      <div className="panel">
        {list.length === 0 ? (
          <p className="lede">No impulse responses yet. Add a WAV file to correct a room or a pair of headphones.</p>
        ) : (
          <div className="ir-list">
            {list.map((c, i) => {
              const ir = info[c.ir_path];
              const matrix = matrixText(c);
              const taps = irTaps(c, info);
              const files = irPaths(c).join("\n");
              // A matrix is shown read-only (no editor yet): its routes and longest entry.
              const facts = matrix
                ? `${matrix}${taps === null ? "" : ` · up to ${fmtInt(taps)} taps`}`
                : ir
                  ? `${ir.channels === 1 ? "Mono, both channels" : ir.channels === 2 ? "Stereo (L→L, R→R)" : `${ir.channels} channels`} · ${fmtInt(ir.frames)} taps · ${fmtRate(ir.rate)}, resampled per rate`
                  : c.ir_path;
              return (
                <div key={irKey(c) + i} className={"ir-card" + (c.enabled ? "" : " off")}>
                  <button className="pwr" aria-pressed={c.enabled} aria-label={`${irName(c)} on`} onClick={() => st.setIr(i, { enabled: !c.enabled })}>
                    <Power />
                  </button>
                  <div style={{ minWidth: 0 }}>
                    <div className="n" title={files}>
                      {irName(c)}
                    </div>
                    <div className="m" title={files}>
                      {facts}
                    </div>
                  </div>
                  <GainSlider value={c.gain_db} label={`${irName(c)} gain`} onCommit={(v) => st.setIr(i, { gain_db: v })} />
                  <button className="icon-btn" aria-label={`Remove ${irName(c)}`} title="Remove" onClick={() => st.removeIr(i)}>
                    <Trash />
                  </button>
                </div>
              );
            })}
          </div>
        )}
      </div>
      {all.length > 0 && <ConvResponsePanel target={target} setTarget={setTarget} data={response.data} error={response.error} />}
      <div className="facts">
        <div className="fact">
          <div className="k">Filters on</div>
          <div className="v">
            {on.length} of {all.length}
          </div>
        </div>
        <div className="fact">
          <div className="k">Combined length</div>
          <div className="v" title={measured ? `What the engine runs at ${fmtRate(measured.rate_hz)}, after resampling` : "The files' lengths at their own rates"}>{combined === null ? "-" : `${fmtInt(combined)} taps`}</div>
        </div>
        <div className="fact">
          <div className="k">Convolution latency</div>
          <div className="v" title="The delay the convolution adds as it plays now: the filters' bulk delay plus the engine's processing block, at the source rate. The engine's total latency is in the status bar.">
            {on.length === 0 || convLatency <= 0 ? "-" : fmtMs(convLatency)}
          </div>
        </div>
      </div>
      <div className="facts">
        <div className="fact" title="A chain whose convolution boosts more than this on any output is refused">
          <div className="k">Boost cap</div>
          <StepSlider className="gain" value={safety.max_boost_db} label="Boost cap" range={CAP_RANGE} format={(v) => `+${v} dB`} onCommit={(v) => st.setConvolutionSafety({ max_boost_db: v })} showValue />
        </div>
        <div className="fact" title="Warn when an output has gain below this frequency (a sub's driver has limited excursion there)">
          <div className="k">Subsonic limit</div>
          <StepSlider className="gain" value={safety.subsonic_hz} label="Subsonic limit" range={SUBSONIC_RANGE} format={(v) => `${v} Hz`} onCommit={(v) => st.setConvolutionSafety({ subsonic_hz: v })} showValue />
        </div>
      </div>
    </section>
  );
}

/** The loaded room calibration (a bundle from a measurement tool): its label, its devices, Load and Unload. */
function CalibrationLine() {
  const st = store();
  const cal = useApp((s) => s.status?.calibration ?? null);
  const stored = useApp((s) => s.config?.calibration);
  const parts = calibrationParts(stored);
  // The status is the daemon's word; the config's flag is the fallback (on unless switched off).
  const enabled = cal?.enabled ?? stored?.enabled ?? true;
  const devices = cal?.devices ?? [];
  const facts = devices
    .map((d) => `${d.role === "master" ? "Main" : d.device}: ${d.entries} filter${d.entries === 1 ? "" : "s"}${d.role === "follower" && d.trim_ms !== 0 ? `, ${fmtMs(d.trim_ms)}` : ""}${d.installed ? "" : " (removed)"}`)
    .join(" · ");
  return (
    <div className="panel">
      <div className="ir-card">
        {cal ? (
          <button className="pwr" aria-pressed={enabled} aria-label="Room calibration on" title={enabled ? "Turn the room calibration off" : "Turn the room calibration on"} onClick={() => void st.setCalibrationEnabled(!enabled)}>
            <Power />
          </button>
        ) : (
          <div />
        )}
        <div style={{ minWidth: 0 }}>
          <div className="n">Room calibration{cal ? `: ${cal.label}` : ""}</div>
          <div className="m" title={cal?.path}>
            {cal
              ? `${facts} · designed at ${fmtRate(cal.design_rate_hz)}${enabled ? "" : " · off: the main speakers play unfiltered and the calibrated followers are muted"}`
              : "None loaded. A bundle sets every device's filters and trims in one go."}
          </div>
          {cal?.warnings.map((w, i) => (
            <div key={i} className="m" style={{ color: "var(--warn)" }}>
              {w}
            </div>
          ))}
        </div>
        <button className="btn" onClick={() => void st.loadCalibration()}>
          {cal ? "Replace…" : "Load…"}
        </button>
        {cal && (
          <button className="btn" onClick={() => void st.unloadCalibration()}>
            Unload
          </button>
        )}
      </div>
      {parts.length > 0 && (
        <div className="ir-list" style={{ marginTop: 8 }}>
          {parts.map((p) => (
            <div key={p.device ?? "master"} className="ir-card">
              <div />
              <div style={{ minWidth: 0 }}>
                <div className="n" title={p.device ?? undefined}>
                  {p.name}
                </div>
                <div className="m">Level · bundle {fmtDb(p.manifest)} dB</div>
              </div>
              <GainSlider value={p.gain} label={`${p.name} calibration level`} onCommit={(v) => st.setCalibrationGain(p.device, v)} />
              <button className="btn" disabled={p.gain === p.manifest} title="Back to the bundle's level" onClick={() => void st.setCalibrationGain(p.device, null)}>
                Reset
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/** The loaded calibration's parts with their levels, from the config (what an edit's echo updates): the main one first, then the followers. */
function calibrationParts(cal: CalibrationConfig | null | undefined): { device: string | null; name: string; gain: number; manifest: number }[] {
  if (!cal) return [];
  const main = cal.master ? [{ device: null, name: "Main", gain: cal.master.gain_db, manifest: cal.master_manifest_gain_db ?? cal.master.gain_db }] : [];
  return [...main, ...cal.followers.map((f) => ({ device: f.device, name: f.device, gain: f.item.gain_db, manifest: f.manifest_gain_db ?? f.item.gain_db }))];
}
