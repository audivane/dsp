import type { CrossfeedConfig, CrossfeedMode, StereoWidthShape } from "../api/types";
import {
  CROSSFEED_CUTOFF,
  CROSSFEED_FEED,
  CROSSFEED_MODES,
  crossfeedActive,
  crossfeedLevelDb,
  crossfeedLoudnessDb,
  crossfeedOn,
  crossfeedParams,
  crossfeedPeakDb,
  crossfeedResponse,
  defaultCrossfeed,
  STEREO_WIDTH,
  STEREO_WIDTH_DETENTS,
  STEREO_WIDTH_SHAPES,
  stereoWidth,
  stereoWidthLabel,
  stereoWidthSideDb,
} from "../lib/effects";
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
  const on = crossfeedOn(cfg);
  const shown: CrossfeedConfig = cfg ?? defaultCrossfeed();
  const { cutoffHz, feedDb } = crossfeedParams(shown);
  const levelDb = crossfeedLevelDb(shown, rate);
  const peakDb = crossfeedPeakDb(shown, rate);
  const resp = crossfeedResponse(shown, rate, FREQS);
  const custom = shown.mode === "bs2b_custom";
  const hasXf = shown.mode !== "none";
  const width = stereoWidth(shown);
  const widthOn = width !== 1;
  const shape: StereoWidthShape = shown.width_shape ?? "flat";
  const cornerHz = shown.width_corner_hz ?? 700;
  const loudDb = crossfeedLoudnessDb(shown, rate);
  const active = crossfeedActive(cfg);
  // Edits never switch the stage on or off: only the power button does.
  const edit = (fn: (c: CrossfeedConfig) => CrossfeedConfig, mode: "drag" | "commit" = "commit") =>
    st.editStage("crossfeed", (c) => fn(c ?? { ...defaultCrossfeed(), enabled: false }), mode);
  const notStereo = streams.some((s) => s.sink === "dsp" && s.channels !== null && s.channels !== 2);
  return (
    <section className="view" aria-labelledby="xf-title">
      <StageHead id="xf-title" title="Stereo image" part={missing ? undefined : "stereo"} on={on} onPower={missing ? undefined : () => st.setStageEnabled("crossfeed", !on)}>
        {!missing && <CompareButton stage="crossfeed" />}
      </StageHead>
      {missing && <MissingNotice id="crossfeed" />}
      {notStereo && active && <div className="notice warn">A stream playing now is not stereo. The stereo image stage needs two channels.</div>}
      <div className="panel">
        <div className="row">
          <span className="k" title="Feeds some of each channel into the other ear, as speakers do: one of the published presets, your own cutoff and feed level, or none.">
            Crossfeed
          </span>
          <Seg<CrossfeedMode>
            label="Crossfeed mode"
            value={shown.mode}
            disabled={missing}
            onChange={(m) => edit((c) => ({ ...c, mode: m }))}
            options={CROSSFEED_MODES.map((m) => [m.mode, m.label, m.title] as [CrossfeedMode, string, string])}
          />
        </div>
        {hasXf && (
          <>
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
          </>
        )}
        <div className="row">
          <label
            className="k"
            htmlFor="xf-width"
            title="The side (left minus right) level; the centre never changes. Mono at the left end, 0 leaves the image as it is, +1 and +2 are one and two steps wider."
          >
            Width
          </label>
          <ValueSlider
            id="xf-width"
            label="Stereo width"
            value={width}
            min={STEREO_WIDTH.min}
            max={STEREO_WIDTH.max}
            step={0.01}
            wide
            detents={STEREO_WIDTH_DETENTS}
            snap={0.04}
            disabled={missing}
            format={(v) => `${stereoWidthLabel(v)} · side ${stereoWidthSideDb(v)}`}
            title="Side gain 0 to 2 (1 is unchanged)"
            onChange={(v, mode) => edit((c) => ({ ...c, width: v }), mode)}
            onEnd={() => st.endStageDrag("crossfeed")}
          />
        </div>
        <div className="row">
          <span className="k" title="Where the width acts.">
            Width shape
          </span>
          <Seg<StereoWidthShape>
            label="Width shape"
            value={shape}
            disabled={missing}
            onChange={(v) => edit((c) => ({ ...c, width_shape: v }))}
            options={STEREO_WIDTH_SHAPES.map((o) => [o.shape, o.label, o.title] as [StereoWidthShape, string, string])}
          />
        </div>
        <p className="hint">Widening raises the side and the room sound, and is a little louder: +0.6 dB at +1, +1.2 dB at +2 on typical music.</p>
        <InlineError slot="crossfeed" />
        <div className="facts">
          <div
            className="fact"
            title="The gain the stage applies so music is as loud with crossfeed on as off (A-weighted, for typical stereo). The width is not compensated. The stage is never turned down for safety, and the EQ preamp does not change for it."
          >
            <div className="k">Level</div>
            <div className="v">{fmtDb(levelDb, 1)} dB</div>
          </div>
          <div
            className="fact"
            title="How far over full scale the rarest worst-case signal could go (full-scale, opposite-polarity channels). Real music almost never gets there; the limiter and the output headroom catch it if it does."
          >
            <div className="k">Worst-case peak</div>
            <div className="v">{fmtDb(peakDb, 1)} dB</div>
          </div>
          {widthOn && (
            <div className="fact" title="How much louder or quieter typical stereo music plays through the stage (A-weighted, level included). Mono music does not change with the width.">
              <div className="k">Loudness</div>
              <div className="v">{fmtDb(loudDb, 1)} dB</div>
            </div>
          )}
          {hasXf && (
            <div className="fact" title="Direct minus fed-in level at low frequencies">
              <div className="k">Separation (bass)</div>
              <div className="v">{feedDb.toFixed(1)} dB</div>
            </div>
          )}
          <div className="fact" title="The sample rate this response is computed at">
            <div className="k">At</div>
            <div className="v">{fmtRate(rate)}</div>
          </div>
        </div>
        <MiniPlot
          label="Stereo image response: own-ear and other-ear paths"
          fMin={20}
          freqs={FREQS}
          yMin={-30}
          yMax={3}
          yStep={6}
          marks={[
            ...(hasXf ? [{ hz: cutoffHz, label: `${Math.round(cutoffHz)} Hz` }] : []),
            ...(widthOn && shape === "above_bass" ? [{ hz: cornerHz, label: `width ${Math.round(cornerHz)} Hz` }] : []),
          ]}
          curves={[
            { label: "Own ear (direct)", db: resp.direct, colour: active ? "var(--accent)" : "var(--faint)" },
            { label: "Other ear (fed in)", db: resp.cross, colour: active ? "var(--ch-r)" : "var(--faint)", dash: true },
          ]}
        />
      </div>
    </section>
  );
}
