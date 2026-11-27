import { VOLUME_MAX_DB, VOLUME_MIN_DB, type SinkChoice, type StreamInfo } from "../api/types";
import { fmtRateShort } from "../lib/format";
import { store, useApp } from "../store/store";
import { Close, Speaker } from "../components/Icons";
import { FineSlider } from "../components/FineSlider";
import { WHEEL_HINT } from "../components/sliderWheel";
import { volumeTaper } from "../lib/taper";
import { streamDbText, streamVolumeBlock } from "../lib/streamVolume";
import { Seg } from "../components/Seg";
import { SINK_LABEL, sinkOptions } from "../lib/routing";
import { directRateNote, directResamples } from "../lib/deviceCaps";

function fmtStream(s: StreamInfo) {
  return [s.rate ? fmtRateShort(s.rate) : null, s.format?.replace(/LE$|BE$/, ""), s.channels ? `${s.channels}ch` : null].filter(Boolean).join(" · ") || "-";
}

const STREAM_TAPER = volumeTaper(VOLUME_MIN_DB, VOLUME_MAX_DB);
const STREAM_TIP =
  `This app's own volume in Audivane, 0 dB = 100%, down to −120 dB. It sets the app's stream in PipeWire, so the app's own slider still works, and it never goes above the app stream cap. Below 0 dB the stream is not bit-perfect. Arrow keys: 0.5 dB, Shift+arrows: 0.1 dB, Page Up/Down: 6 dB. Double-click the value to type it. ${WHEEL_HINT}`;

/** One stream's volume: a dB slider, a mute toggle, and the engine's note
 *  or refusal beside them. */
function StreamVolumeCell({ s, name }: { s: StreamInfo; name: string }) {
  const st = store();
  const cap = useApp((x) => x.caps.streamVolume);
  const epoch = useApp((x) => x.connEpoch);
  // Re-render on this stream's draft and message only.
  useApp((x) => x.streamDrafts[s.node]);
  const message = useApp((x) => x.streamMessages[s.node]);
  const { db, muted } = st.streamLevel(s);
  const block = streamVolumeBlock(s, cap);
  const off = block !== null;
  return (
    <div className="stream-vol-cell">
      <div className="stream-vol" title={block ?? STREAM_TIP}>
        <FineSlider
          key={`stream-${s.node}-${epoch}`}
          id={`stream-vol-${s.node}`}
          label={`Volume of ${name}`}
          value={db}
          taper={STREAM_TAPER}
          disabled={off}
          text={muted ? "muted" : streamDbText(db)}
          title={block ?? STREAM_TIP}
          onChange={(v, mode) => st.setStreamLevel(s.node, { volume_db: v }, mode)}
          commitOnUnmount
        />
        <button
          className="icon-btn mute"
          aria-pressed={muted}
          aria-label={`Mute ${name}`}
          title={block ?? (muted ? `Unmute ${name}` : `Mute ${name} (its level is kept)`)}
          disabled={off}
          onClick={() => st.setStreamLevel(s.node, { muted: !muted })}
        >
          <Speaker muted={muted} />
        </button>
      </div>
      {message &&
        (message.error ? (
          <div className="inline-err" role="alert">
            <span>{message.text}</span>
            <button className="icon-btn" aria-label="Dismiss" onClick={() => st.clearStreamMessage(s.node)}>
              <Close />
            </button>
          </div>
        ) : (
          <p className="stream-vol-note" role="status">
            {message.text}
          </p>
        ))}
    </div>
  );
}

export function RoutingView() {
  const st = store();
  const routing = useApp((s) => s.config?.routing);
  const streams = useApp((s) => s.streams);
  const normalOutput = useApp((s) => s.status?.normal_output);
  const effectiveRates = useApp((s) => s.status?.effective_rates);
  const directRates = useApp((s) => s.status?.direct_rates);
  const deviceCaps = useApp((s) => s.status?.device_caps);
  if (!routing) return null;
  const apps = routing.mode === "apps";
  const options = sinkOptions(apps, normalOutput);
  const directNote = directRateNote(effectiveRates, directRates, deviceCaps);
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
                  <th title="Each app's own stream volume in dB (0 dB = 100%). Apps playing through Audivane can be set here; the desktop mixer shows the same volume on its own scale. Below 0 dB the stream is not bit-perfect.">Volume</th>
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
                          <span className="muted" title="This device does not play this rate natively and the engine does not convert it for Direct, so PipeWire resamples the stream: not bit-perfect.">
                            {" "}
                            · resampled
                          </span>
                        )}
                      </td>
                      <td>
                        <StreamVolumeCell s={s} name={name} />
                      </td>
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
