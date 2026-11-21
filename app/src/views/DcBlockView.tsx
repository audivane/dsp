import { DC_CUTOFF, dcBlockDb, dcBlockOn } from "../lib/effects";
import { fmtDb } from "../lib/format";
import { store, useApp } from "../store/store";
import { MiniPlot, plotFreqs } from "../components/MiniPlot";
import { CompareButton, InlineError, MissingNotice, StageHead, useStageMissing } from "../components/StageParts";
import { ValueSlider } from "../components/ValueSlider";

const FREQS = plotFreqs(1, 20000, 160);

export function DcBlockView() {
  const st = store();
  useApp((s) => s.config?.chain.dc_block);
  useApp((s) => s.stageDrafts.dc_block);
  const missing = useStageMissing("dc_block");
  const cfg = st.stage("dc_block");
  const on = dcBlockOn(cfg);
  const cutoff = cfg?.cutoff_hz ?? DC_CUTOFF.def;
  return (
    <section className="view" aria-labelledby="dc-title">
      <StageHead id="dc-title" title="DC block" on={on} onPower={missing ? undefined : () => st.setStageEnabled("dc_block", !on)}>
        {!missing && <CompareButton stage="dc" />}
      </StageHead>
      {missing && <MissingNotice id="dc_block" />}
      <div className="panel">
        <div className="row">
          <label className="k" htmlFor="dc-cutoff" title="The high-pass corner (2nd-order Butterworth, −3 dB point), 1 to 20 Hz. Lower keeps more deep bass; higher removes more rumble.">
            Cutoff
          </label>
          <ValueSlider
            id="dc-cutoff"
            label="DC block cutoff"
            value={cutoff}
            min={DC_CUTOFF.min}
            max={DC_CUTOFF.max}
            step={0.5}
            disabled={missing}
            format={(v) => `${v.toFixed(1)} Hz`}
            title="High-pass corner, 1 to 20 Hz (default 5 Hz)"
            onChange={(v, mode) => st.editStage("dc_block", (c) => ({ enabled: c?.enabled ?? true, cutoff_hz: v }), mode)}
            onEnd={() => st.endStageDrag("dc_block")}
          />
          <button className="btn small" disabled={missing || cutoff === DC_CUTOFF.def} onClick={() => st.editStage("dc_block", (c) => ({ enabled: c?.enabled ?? true, cutoff_hz: DC_CUTOFF.def }))}>
            Reset to 5 Hz
          </button>
          <span className="hint">At 20 Hz: {fmtDb(dcBlockDb(cutoff, 20), 2)} dB</span>
        </div>
        <InlineError slot="dc_block" />
        <MiniPlot
          label={`DC block response, ${cutoff} Hz high-pass`}
          fMin={1}
          freqs={FREQS}
          yMin={-24}
          yMax={3}
          yStep={6}
          marks={[{ hz: cutoff, label: `${cutoff.toFixed(1)} Hz` }]}
          curves={[{ label: "Response", db: FREQS.map((f) => dcBlockDb(cutoff, f)), colour: on ? "var(--accent)" : "var(--faint)" }]}
        />
      </div>
    </section>
  );
}
