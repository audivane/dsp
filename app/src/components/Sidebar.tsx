import { useState } from "react";
import { convList, irName } from "../lib/chain";
import { abStoppedByOverload, describeB } from "../lib/abCompare";
import { fmtTaps } from "../lib/format";
import { convSummary, irTaps, outputSummary, upsamplingSummary } from "../lib/summary";
import { crossfeedSummary, dcBlockSummary, harmonicsSummary, limiterSummary, pitchPageOn, pitchPageSummary, stagesOn } from "../lib/effects";
import { tiltStageSummary } from "../lib/tilt";
import { STAGE_NAME, store, useApp, type StageId, type ViewId } from "../store/store";
import { Chevron, Power } from "./Icons";

/** One of the effect stages: its power button turns it on/off and keeps its settings. */
function StageItem({ id, sub, on }: { id: StageId; sub: string; on: boolean }) {
  const st = store();
  const missing = useApp((s) => s.stageCaps[id] === false);
  const name = STAGE_NAME[id];
  if (missing) return <Item id={id} stage name={name} sub="Needs an engine update" power={false} />;
  return <Item id={id} stage name={name} sub={sub} power={on} onPower={() => st.setStageEnabled(id, !on)} />;
}

function Item({ id, name, sub, power, onPower, extra, stage, powerTitle }: { id: ViewId; name: string; sub: string; power?: boolean; onPower?: () => void; extra?: React.ReactNode; stage?: boolean; powerTitle?: string }) {
  const view = useApp((s) => s.view);
  const st = store();
  return (
    <div className={"nav-item" + (stage ? " stage" : "") + (view === id ? " active" : "") + (power === false ? " off" : "")}>
      {onPower ? (
        <button className="pwr" aria-pressed={!!power} aria-label={`${name} on`} title={powerTitle ?? (power ? `Turn ${name} off` : `Turn ${name} on`)} onClick={onPower}>
          <Power />
        </button>
      ) : (
        <span className="pwr-gap" />
      )}
      <button className="nav-main" aria-current={view === id ? "page" : undefined} onClick={() => st.setView(id)}>
        <span className="nav-name">{name}</span>
        <span className="nav-sub" title={sub}>
          {sub}
        </span>
      </button>
      {extra}
    </div>
  );
}

export function Sidebar() {
  const st = store();
  const cfg = useApp((s) => s.config);
  const status = useApp((s) => s.status);
  const clipNoLimiter = useApp((s) => s.clipNoLimiter);
  const irInfo = useApp((s) => s.irInfo);
  const devices = useApp((s) => s.devices);
  const presetsCap = useApp((s) => s.caps.presets);
  const [convOpen, setConvOpen] = useState(true);
  if (!cfg) return <nav className="side" aria-label="Signal path" />;
  const bypass = !cfg.dsp_enabled;
  const eq = cfg.chain.eq;
  const irs = convList(cfg.chain);
  const convOn = irs.some((c) => c.enabled);
  const ups = cfg.chain.upsampler;
  // What the engine runs now, when the DSP path is live: the summary shows
  // that (a fallback or a fractional landing) rather than the target.
  const upsLive =
    status && cfg.dsp_enabled && status.active_sink !== "direct" && (status.state === "playing" || status.state === "ready") && status.rate_ratio
      ? { graphRate: status.graph_rate, outputRate: status.output_rate, ratio: status.rate_ratio }
      : null;
  const dev = devices.length ? st.currentDevice() : undefined;
  const eqSub = (() => {
    if (!eq) return "Off";
    if (!eq.left && !eq.right) {
      if (!eq.linked_enabled) return "L+R off";
      return `${eq.bands.length} ${eq.bands.length === 1 ? "band" : "bands"} · L+R`;
    }
    const parts = [eq.linked_enabled ? `L+R ${eq.bands.length}` : "L+R off"];
    if (eq.left) parts.push(eq.left.enabled ? `L ${eq.left.bands.length}` : "L off");
    if (eq.right) parts.push(eq.right.enabled ? `R ${eq.right.bands.length}` : "R off");
    return parts.join(" · ");
  })();
  const tilt = tiltStageSummary(eq);
  const eqSubFull = eq && tilt ? `${eqSub} · ${tilt}` : eqSub;
  const on = stagesOn(cfg.chain);
  const ab = cfg.ab;
  const abSub = !ab
    ? "Off"
    : abStoppedByOverload(ab, status?.overload)
      ? `B: ${describeB(ab).title} · stopped (overload)`
      : `B: ${describeB(ab).title} · ${ab.level.mode === "manual" ? "manual level" : ab.level.mode === "measured" ? "measured match" : ab.level.mode === "off" ? "no level match" : "analytic match"}`;
  const followers = cfg.outputs?.followers?.length ?? 0;
  const devSub = `${dev?.name ?? cfg.device}${status?.format ? " · " + status.format.toUpperCase().replace("_3", " 3-byte") : ""}${followers ? ` · +${followers} ${followers === 1 ? "device" : "devices"}` : ""}`;

  return (
    <nav className={"side" + (bypass ? " bypassed" : "")} aria-label="Signal path">
      <div className="side-label">Signal path</div>
      <StageItem id="dc_block" sub={dcBlockSummary(cfg.chain.dc_block)} on={on.dc_block} />
      <StageItem id="pitch" sub={pitchPageSummary(cfg.chain.pitch, cfg.varispeed)} on={pitchPageOn(cfg.chain.pitch, cfg.varispeed)} />
      <StageItem id="crossfeed" sub={crossfeedSummary(cfg.chain.crossfeed)} on={on.crossfeed} />
      <Item id="eq" stage name="Parametric EQ" sub={eqSubFull} power={!!eq} onPower={() => st.setEqEnabled(!eq)} />
      <Item
        id="conv"
        stage
        name="Convolution"
        sub={convSummary(cfg, irInfo)}
        power={convOn}
        onPower={irs.length ? () => st.setConvEnabled(!convOn) : undefined}
        extra={
          <button className="icon-btn" aria-expanded={convOpen} aria-label="Show impulse responses" onClick={() => setConvOpen(!convOpen)}>
            <Chevron className={"chev" + (convOpen ? " open" : "")} />
          </button>
        }
      />
      {convOpen && (
        <div className="slots">
          {irs.map((c, i) => {
            const taps = irTaps(c, irInfo);
            return (
              <div key={c.ir_path + i} className={"slot" + (c.enabled ? "" : " off")}>
                <button className="pwr" aria-pressed={c.enabled} aria-label={`${irName(c)} on`} onClick={() => st.setIr(i, { enabled: !c.enabled })}>
                  <Power />
                </button>
                <span className="slot-name" title={c.ir_path}>
                  {irName(c)}
                </span>
                {taps !== null && <span className="slot-len mono">{fmtTaps(taps)}</span>}
              </div>
            );
          })}
          <button className="link-btn" onClick={() => void st.addImpulseResponse()}>
            + Add impulse response
          </button>
        </div>
      )}
      <Item id="ups" stage name="Upsampling" sub={upsamplingSummary(ups, upsLive)} power={ups.target !== "off"} onPower={() => st.setUpsamplingEnabled(ups.target === "off")} />
      <StageItem id="harmonics" sub={harmonicsSummary(cfg.chain.harmonics)} on={on.harmonics} />
      <StageItem id="limiter" sub={clipNoLimiter ? "Off · the output clipped" : limiterSummary(cfg.chain.limiter, cfg.chain.output.headroom_db)} on={on.limiter} />
      <Item id="out" name="Output" sub={outputSummary(cfg, status?.format)} />
      {bypass && <p className="bypass-note">Bypass is on: all stages are skipped.</p>}
      <div className="side-label">Setup</div>
      <Item id="routing" name="Routing" sub={cfg.routing.mode === "system" ? "System-wide" : `Selected apps · ${cfg.routing.rules.length} ${cfg.routing.rules.length === 1 ? "rule" : "rules"}`} />
      <Item id="ab" name="A/B compare" sub={presetsCap === false && !ab ? "Off" : abSub} />
      <Item id="device" name="Device" sub={devSub} />
    </nav>
  );
}
