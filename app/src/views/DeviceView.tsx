import type { RateConflict } from "../api/types";
import { fmtRate, fmtSampleFormat } from "../lib/format";
import { store, useApp } from "../store/store";
import { DevicePicker } from "../components/TopBar";
import { Seg } from "../components/Seg";

export function DeviceView() {
  const st = store();
  const cfg = useApp((s) => s.config);
  const status = useApp((s) => s.status);
  const devices = useApp((s) => s.devices);
  const test = useApp((s) => s.selfTest);
  if (!cfg) return null;
  const dev = devices.length ? st.currentDevice() : undefined;
  const released = status?.state === "idle";
  const hw = status?.volume ? (status.volume.hardware ? "Yes" : "No (digital)") : "—";
  return (
    <section className="view" aria-labelledby="dev-title">
      <div className="view-head">
        <h1 id="dev-title">Device</h1>
      </div>
      <div className="panel">
        <div className="row">
          <span className="k">Output device</span>
          <DevicePicker className="chip" id="device-page" />
        </div>
        <div className="facts">
          <div className="fact">
            <div className="k">Card</div>
            <div className="v" title={cfg.device}>{dev?.card ?? cfg.device}</div>
          </div>
          <div className="fact">
            <div className="k">Format in use</div>
            <div className="v">{fmtSampleFormat(status?.format)}</div>
          </div>
          <div className="fact">
            <div className="k">Rate now</div>
            <div className="v">{status?.output_rate ? fmtRate(status.output_rate) : "—"}</div>
          </div>
          <div className="fact">
            <div className="k">Hardware volume</div>
            <div className="v">{hw}</div>
          </div>
          <div className="fact">
            <div className="k">Quantum</div>
            <div className="v">{cfg.quantum}</div>
          </div>
          <div className="fact">
            <div className="k">Engine</div>
            <div className="v">{status?.version ?? "—"}</div>
          </div>
        </div>
        <div className="row">
          <span className="k" title="When apps play into Audivane DSP at different sample rates, the DAC can run at only one of them; PipeWire resamples the others. Apps on the Direct (bit-perfect) path always set the rate themselves.">
            Mixed rates
          </span>
          <Seg<RateConflict>
            label="When apps play at different rates"
            value={cfg.rate.conflict}
            onChange={(conflict) => st.setRateConflict(conflict)}
            disabled={!cfg.rate.follow_source}
            options={[
              ["keep_current", "Keep current", "Keep the current rate while any app still plays at it; otherwise follow the app that started first."],
              ["highest", "Use highest", "Run at the highest rate any of the apps plays at."],
              ["default_rate", "Use default", `Run at the default rate (${fmtRate(cfg.rate.default_rate)}) while the apps disagree. A single app is still followed.`],
            ]}
          />
          {!cfg.rate.follow_source && <span className="hint">Rate following is off: always {fmtRate(cfg.rate.default_rate)}</span>}
        </div>
        <div className="row">
          {released ? (
            <button className="btn" onClick={() => void st.acquireDevice()}>
              Take the DAC back
            </button>
          ) : (
            <button className="btn" onClick={() => void st.releaseDevice()} title="Hand the DAC to other apps; the engine idles">
              Release DAC to the system
            </button>
          )}
          <button className="btn primary" disabled={test.state === "running" || released} onClick={() => void st.runSelfTest()}>
            Run bit-perfect self-test
          </button>
        </div>
        {test.state === "running" && <div className="result pending" role="status">Playing a test signal through the Direct path…</div>}
        {test.state === "done" && test.result && (
          <div className={"result" + (test.result.passed ? "" : " bad")} role="status">
            {test.result.passed ? "✓ Passed. " : "✗ Failed. "}
            {test.result.frames.toLocaleString("en-US")} frames at {fmtRate(test.result.rate)}
            {test.result.format ? ` (${fmtSampleFormat(test.result.format)})` : ""} {test.result.passed ? "reached the DAC unchanged." : "did not reach the DAC unchanged."}
            {!test.result.passed && test.result.detail ? ` ${test.result.detail}` : ""}
          </div>
        )}
        {test.state === "error" && (
          <div className="result bad" role="alert">
            The self-test could not run. {test.error}
          </div>
        )}
      </div>
    </section>
  );
}
