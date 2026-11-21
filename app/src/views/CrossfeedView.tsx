import type { CrossfeedConfig, CrossfeedLevel, CrossfeedMode } from "../api/types";
import {
  CROSSFEED_CUTOFF,
  CROSSFEED_FEED,
  CROSSFEED_LEVELS,
  CROSSFEED_MODES,
  crossfeedLevel,
  crossfeedLevelDb,
  crossfeedOn,
  crossfeedParams,
  crossfeedPeakDb,
  crossfeedResponse,
  defaultCrossfeed,
  withCrossfeedLevel,
  crossfeedAsRun,
} from "../lib/effects";
import { ENGINE_UPDATE_TIP } from "../lib/upsamplerLimits";
import { fmtDb, fmtRate } from "../lib/format";
import { store, useApp } from "../store/store";
import { Seg } from "../components/Seg";
import { MiniPlot, plotFreqs } from "../components/MiniPlot";
import { CompareButton, InlineError, MissingNotice, StageHead, useStageMissing } from "../components/StageParts";
import { ValueSlider } from "../components/ValueSlider";

const FREQS = plotFreqs(20, 20000, 160);

export function CrossfeedView() {
  const st = store();
  useApp((s) => s.config?.chain.crossfeed);
  useApp((s) => s.stageDrafts.crossfeed);
  const rate = useApp((s) => s.status?.graph_rate ?? s.config?.rate.default_rate ?? 48000);
  const streams = useApp((s) => s.streams);
  const missing = useStageMissing("crossfeed");
  const cfg = st.stage("crossfeed");
  // Known only from the daemon's own config: an engine without the level
  // policy echoes a crossfeed config without the field.
  const levelCap = useApp((s) => s.stageCaps.crossfeedLevel);
  const levelSupported = levelCap !== false;
  const on = crossfeedOn(cfg);
  const shown: CrossfeedConfig = cfg ?? defaultCrossfeed();
  const { cutoffHz, feedDb } = crossfeedParams(shown);
  const levelShown = crossfeedAsRun(shown, levelCap);
  const level = crossfeedLevel(levelShown);
  const levelDb = crossfeedLevelDb(levelShown, rate);
  const peakDb = crossfeedPeakDb(levelShown, rate);
  const resp = crossfeedResponse(levelShown, rate, FREQS);
  const custom = shown.mode === "bs2b_custom";
  // Edits never switch the stage on or off: only the power button does.
  const edit = (fn: (c: CrossfeedConfig) => CrossfeedConfig, mode: "drag" | "commit" = "commit") =>
    st.editStage("crossfeed", (c) => fn(c ?? { ...defaultCrossfeed(), enabled: false }), mode);
  const notStereo = streams.some((s) => s.sink === "dsp" && s.channels !== null && s.channels !== 2);
  return (
    <section className="view" aria-labelledby="xf-title">
      <StageHead id="xf-title" title="Crossfeed" on={on} onPower={missing ? undefined : () => st.setStageEnabled("crossfeed", !on)}>
        {!missing && <CompareButton stage="crossfeed" />}
      </StageHead>
      {missing && <MissingNotice id="crossfeed" />}
      {notStereo && on && <div className="notice warn">A stream playing now is not stereo. Crossfeed needs two channels.</div>}
      <div className="panel">
        <div className="row">
          <span className="k" title="One of the published presets, or your own cutoff and feed level.">
            Mode
          </span>
          <Seg<CrossfeedMode>
            label="Crossfeed mode"
            value={shown.mode}
            disabled={missing}
            onChange={(m) => edit((c) => ({ ...c, mode: m }))}
            options={CROSSFEED_MODES.map((m) => [m.mode, m.label, m.title] as [CrossfeedMode, string, string])}
          />
        </div>
        <div className="row">
          <label className="k" htmlFor="xf-cutoff" title="Below this frequency the other channel is fed in; above it, less and less. 300 to 2000 Hz (custom only).">
            Cutoff
          </label>
          <ValueSlider
            id="xf-cutoff"
            label="Crossfeed cutoff"
            value={cutoffHz}
            min={CROSSFEED_CUTOFF.min}
            max={CROSSFEED_CUTOFF.max}
            step={5}
            log
            disabled={missing}
            format={(v) => `${Math.round(v)} Hz`}
            title={custom ? "300 to 2000 Hz" : "300 to 2000 Hz. Moving it switches to Custom."}
            onChange={(v, mode) => edit((c) => ({ ...c, mode: "bs2b_custom", cutoff_hz: v, feed_db: custom ? c.feed_db : feedDb }), mode)}
            onEnd={() => st.endStageDrag("crossfeed")}
          />
        </div>
        <div className="row">
          <label className="k" htmlFor="xf-feed" title="How much quieter the fed-in signal is than the direct one at low frequencies: 1 dB is a strong blend, 15 dB a faint one (custom only).">
            Feed level
          </label>
          <ValueSlider
            id="xf-feed"
            label="Crossfeed feed level"
            value={feedDb}
            min={CROSSFEED_FEED.min}
            max={CROSSFEED_FEED.max}
            step={0.5}
            disabled={missing}
            format={(v) => `${v.toFixed(1)} dB`}
            title={custom ? "1 to 15 dB below the direct signal" : "1 to 15 dB. Moving it switches to Custom."}
            onChange={(v, mode) => edit((c) => ({ ...c, mode: "bs2b_custom", feed_db: v, cutoff_hz: custom ? c.cutoff_hz : cutoffHz }), mode)}
            onEnd={() => st.endStageDrag("crossfeed")}
          />
          {!custom && <span className="hint">Move a slider to switch to Custom.</span>}
        </div>
        <div className="row">
          <span
            className="k"
            title="How the stage sets its volume. Matched loudness: as loud on as off. Peak-safe: never over full scale, but quieter. Either way the EQ preamp does not change."
          >
            Level
          </span>
          <Seg<CrossfeedLevel>
            label="Crossfeed level"
            value={level}
            disabled={missing || !levelSupported}
            onChange={(l) => st.editStage("crossfeed", (c) => withCrossfeedLevel(c, l))}
            options={CROSSFEED_LEVELS.map((l) => [l.level, l.label, levelSupported ? l.title : ENGINE_UPDATE_TIP] as [CrossfeedLevel, string, string])}
          />
        </div>
        <InlineError slot="crossfeed" />
        <div className="facts">
          <div
            className="fact"
            title={
              level === "matched"
                ? "The gain the stage applies so music is as loud with crossfeed on as off (A-weighted, for typical stereo). The EQ preamp does not change for it."
                : "The stage lowers the level by its exact worst-case peak gain, so it can never go over full scale. The EQ preamp does not change for it."
            }
          >
            <div className="k">Level</div>
            <div className="v">{fmtDb(levelDb, 1)} dB</div>
          </div>
          {level === "matched" && (
            <div
              className="fact"
              title="How far over full scale the rarest worst-case signal could go (full-scale, opposite-polarity channels). Real music almost never gets there, but if it does it clips, unless the limiter is on or the digital volume is at least this far down. Hardware volume does not help; choose Peak-safe to rule it out."
            >
              <div className="k">Worst-case peak</div>
              <div className="v">{fmtDb(peakDb, 1)} dB</div>
            </div>
          )}
          <div className="fact" title="Direct minus fed-in level at low frequencies">
            <div className="k">Separation (bass)</div>
            <div className="v">{feedDb.toFixed(1)} dB</div>
          </div>
          <div className="fact" title="The sample rate this response is computed at">
            <div className="k">At</div>
            <div className="v">{fmtRate(rate)}</div>
          </div>
        </div>
        <MiniPlot
          label="Crossfeed response: direct and fed-in paths"
          fMin={20}
          freqs={FREQS}
          yMin={-30}
          yMax={3}
          yStep={6}
          marks={[{ hz: cutoffHz, label: `${Math.round(cutoffHz)} Hz` }]}
          curves={[
            { label: "Own ear (direct)", db: resp.direct, colour: on ? "var(--accent)" : "var(--faint)" },
            { label: "Other ear (fed in)", db: resp.cross, colour: on ? "var(--ch-r)" : "var(--faint)", dash: true },
          ]}
        />
      </div>
    </section>
  );
}
