import type { SinkChoice, StreamInfo } from "../api/types";
import { fmtRateShort } from "../lib/format";
import { store, useApp } from "../store/store";
import { Close } from "../components/Icons";
import { Seg } from "../components/Seg";
import { SINK_LABEL, sinkOptions } from "../lib/routing";
import { directRateNote, directResamples } from "../lib/deviceCaps";

function fmtStream(s: StreamInfo) {
  return [s.rate ? fmtRateShort(s.rate) : null, s.format?.replace(/LE$|BE$/, ""), s.channels ? `${s.channels}ch` : null].filter(Boolean).join(" · ") || "—";
}

export function RoutingView() {
  const st = store();
  const routing = useApp((s) => s.config?.routing);
  const streams = useApp((s) => s.streams);
  const normalOutput = useApp((s) => s.status?.normal_output);
  const effectiveRates = useApp((s) => s.status?.effective_rates);
  const directRates = useApp((s) => s.status?.direct_rates);
  if (!routing) return null;
  const apps = routing.mode === "apps";
  const options = sinkOptions(apps, normalOutput);
  const directNote = directRateNote(effectiveRates, directRates);
  return (
    <section className="view" aria-labelledby="routing-title">
      <div className="view-head">
        <h1 id="routing-title">Routing</h1>
        <div className="spacer" />
        <Seg
          label="Routing mode"
          value={routing.mode}
          onChange={(m) => void st.setRoutingMode(m === "system" ? { mode: "system" } : { mode: "apps", rules: [] })}
          options={[
            ["system", "System-wide"],
            ["apps", "Selected apps"],
          ]}
        />
      </div>
      <p className="lede">
        {apps
          ? "Only the apps you choose play through Audivane. Everything else uses your normal output. A choice below is remembered for that app."
          : "All audio plays through Audivane DSP. Move a stream to Direct for bit-perfect playback beside it."}
      </p>
      <div className="panel">
        <h2>Playing now</h2>
        {directNote && <p className="hint">{directNote}</p>}
        {streams.length === 0 ? (
          <p className="lede">No app is playing audio.</p>
        ) : (
          <div className="table-wrap">
            <table className="streams">
              <thead>
                <tr>
                  <th>App</th>
                  <th>Stream</th>
                  <th>Format</th>
                  <th>Volume</th>
                  <th>Sends to</th>
                </tr>
              </thead>
              <tbody>
                {streams.map((s) => {
                  const name = s.app_name || s.binary || `Stream ${s.node}`;
                  // In system-wide routing an unrouted stream plays on the
                  // default, which is Audivane DSP.
                  const value: SinkChoice = s.sink ?? (apps ? "default" : "dsp");
                  return (
                    <tr key={s.node}>
                      <td>{name}</td>
                      <td className="muted">{s.media_name ?? ""}</td>
                      <td className="mono">
                        {fmtStream(s)}
                        {value === "direct" && directResamples(s.rate, directRates) && (
                          <span className="muted" title="Audivane Direct never upsamples and this device does not play this rate natively, so PipeWire resamples the stream: not bit-perfect.">
                            {" "}
                            · resampled
                          </span>
                        )}
                      </td>
                      <td className="mono">{s.muted ? "muted" : s.volume === null || s.volume === undefined ? "—" : `${Math.round(s.volume * 100)}%`}</td>
                      <td>
                        <select aria-label={`Output for ${name}`} value={value} onChange={(e) => void st.routeStream(s, e.target.value as SinkChoice)}>
                          {options.map(([k, label]) => (
                            <option key={k} value={k} disabled={k === "default" && !normalOutput && value !== "default"}>
                              {label}
                            </option>
                          ))}
                        </select>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>
      {apps && (
        <div className="panel">
          <h2>Remembered apps</h2>
          {routing.rules.length === 0 ? (
            <p className="lede">None yet. Choose an output for a playing app above.</p>
          ) : (
            <div className="rules">
              {routing.rules.map((r) => (
                <span key={r.app} className="rule">
                  {r.app} → {SINK_LABEL[r.sink]}
                  <button className="icon-btn" aria-label={`Forget ${r.app}`} onClick={() => void st.removeAppRule(r.app)}>
                    <Close />
                  </button>
                </span>
              ))}
            </div>
          )}
        </div>
      )}
    </section>
  );
}
