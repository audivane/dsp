import { irName } from "../lib/chain";
import { fmtDb, fmtInt, fmtMs, fmtRate } from "../lib/format";
import { combinedTaps } from "../lib/summary";
import { store, useApp } from "../store/store";
import { Plus, Power, Trash } from "../components/Icons";
import { CompareButton } from "../components/StageParts";
import { StepSlider } from "../components/StepSlider";

const GAIN_RANGE = { min: -24, max: 12, step: 0.5 } as const;

function GainSlider({ value, onCommit, label }: { value: number; onCommit: (v: number) => void; label: string }) {
  return <StepSlider className="gain" value={value} label={label} range={GAIN_RANGE} format={(v) => `${fmtDb(v)} dB`} onCommit={onCommit} showValue />;
}

export function ConvolutionView() {
  const st = store();
  useApp((s) => s.config?.chain.convolution);
  const info = useApp((s) => s.irInfo);
  const latency = useApp((s) => s.status?.latency_ms ?? null);
  const single = useApp((s) => s.caps.convChain === false);
  const list = st.convolution();
  const on = list.filter((c) => c.enabled);
  const combined = combinedTaps(list, info);
  return (
    <section className="view" aria-labelledby="conv-title">
      <div className="view-head">
        <h1 id="conv-title">Convolution</h1>
        <div className="spacer" />
        <CompareButton stage="conv" />
        <button className="btn primary" onClick={() => void st.addImpulseResponse()} disabled={single && list.length >= 1}>
          <Plus />
          Add impulse response
        </button>
      </div>
      {single && list.length >= 1 && <div className="notice">This engine version runs one impulse response at a time.</div>}
      <div className="panel">
        {list.length === 0 ? (
          <p className="lede">No impulse responses yet. Add a WAV file to correct a room or a pair of headphones.</p>
        ) : (
          <div className="ir-list">
            {list.map((c, i) => {
              const ir = info[c.ir_path];
              const facts = ir
                ? `${ir.channels === 1 ? "Mono, both channels" : ir.channels === 2 ? "Stereo (L→L, R→R)" : `${ir.channels} channels`} · ${fmtInt(ir.frames)} taps · ${fmtRate(ir.rate)}, resampled per rate`
                : c.ir_path;
              return (
                <div key={c.ir_path + i} className={"ir-card" + (c.enabled ? "" : " off")}>
                  <button className="pwr" aria-pressed={c.enabled} aria-label={`${irName(c)} on`} onClick={() => st.setIr(i, { enabled: !c.enabled })}>
                    <Power />
                  </button>
                  <div style={{ minWidth: 0 }}>
                    <div className="n" title={c.ir_path}>
                      {irName(c)}
                    </div>
                    <div className="m" title={c.ir_path}>
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
      <div className="facts">
        <div className="fact">
          <div className="k">Filters on</div>
          <div className="v">
            {on.length} of {list.length}
          </div>
        </div>
        <div className="fact">
          <div className="k">Combined length</div>
          <div className="v">{combined === null ? "—" : `${fmtInt(combined)} taps`}</div>
        </div>
        <div className="fact">
          <div className="k">Engine latency</div>
          <div className="v" title="End-to-end added latency reported by the engine, all stages included">
            {latency === null ? "—" : fmtMs(latency)}
          </div>
        </div>
      </div>
    </section>
  );
}
