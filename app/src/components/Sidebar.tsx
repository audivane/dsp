import { useState } from "react";
import { convList, irName } from "../lib/chain";
import { fmtTaps } from "../lib/format";
import { convSummary, irTaps, outputSummary, upsamplingSummary } from "../lib/summary";
import { store, useApp, type ViewId } from "../store/store";
import { Chevron, Power } from "./Icons";

function Item({ id, name, sub, power, onPower, extra, stage }: { id: ViewId; name: string; sub: string; power?: boolean; onPower?: () => void; extra?: React.ReactNode; stage?: boolean }) {
  const view = useApp((s) => s.view);
  const st = store();
  return (
    <div className={"nav-item" + (stage ? " stage" : "") + (view === id ? " active" : "") + (power === false ? " off" : "")}>
      {onPower ? (
        <button className="pwr" aria-pressed={!!power} aria-label={`${name} on`} title={power ? `Turn ${name} off` : `Turn ${name} on`} onClick={onPower}>
          <Power />
        </button>
      ) : (
        <span className="pwr-gap" />
      )}
      <button className="nav-main" aria-current={view === id ? "page" : undefined} onClick={() => st.setView(id)}>
        <span className="nav-name">{name}</span>
        <span className="nav-sub">{sub}</span>
      </button>
      {extra}
    </div>
  );
}

export function Sidebar() {
  const st = store();
  const cfg = useApp((s) => s.config);
  const status = useApp((s) => s.status);
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
  const ab = cfg.ab;
  const abSub = !ab ? "Off" : `B: ${ab.chain_b.eq?.name || "Flat"} · ${ab.level.mode === "manual" ? "manual level" : ab.level.mode === "off" ? "no level match" : "analytic match"}`;

  return (
    <nav className={"side" + (bypass ? " bypassed" : "")} aria-label="Signal path">
      <div className="side-label">Signal path</div>
      <Item id="eq" stage name="Parametric EQ" sub={eqSub} power={!!eq} onPower={() => st.setEqEnabled(!eq)} />
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
      <Item id="ups" stage name="Upsampling" sub={upsamplingSummary(ups)} power={ups.target !== "off"} onPower={() => st.setUpsamplingEnabled(ups.target === "off")} />
      <Item id="out" name="Output" sub={outputSummary(cfg, status?.format)} />
      {bypass && <p className="bypass-note">Bypass is on: all stages are skipped.</p>}
      <div className="side-label">Setup</div>
      <Item id="routing" name="Routing" sub={cfg.routing.mode === "system" ? "System-wide" : `Selected apps · ${cfg.routing.rules.length} ${cfg.routing.rules.length === 1 ? "rule" : "rules"}`} />
      <Item id="ab" name="A/B compare" sub={presetsCap === false && !ab ? "Off" : abSub} />
      <Item id="device" name="Device" sub={`${dev?.name ?? cfg.device}${status?.format ? " · " + status.format.toUpperCase().replace("_3", " 3-byte") : ""}`} />
    </nav>
  );
}
