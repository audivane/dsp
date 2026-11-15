import type { DitherKind, SampleFormat } from "../api/types";
import { fmtSampleFormat } from "../lib/format";
import { store, useApp } from "../store/store";
import { Seg } from "../components/Seg";

export function OutputView() {
  const st = store();
  const cfg = useApp((s) => s.config);
  const negotiated = useApp((s) => s.status?.format ?? null);
  if (!cfg) return null;
  const out = cfg.chain.output;
  return (
    <section className="view" aria-labelledby="out-title">
      <div className="view-head">
        <h1 id="out-title">Output</h1>
      </div>
      <div className="panel">
        <div className="row">
          <span className="k" title="The sample format the engine asks the DAC for. The first one the DAC accepts is used.">
            DAC format
          </span>
          <Seg<SampleFormat>
            label="Preferred DAC format"
            value={cfg.formats[0] ?? null}
            onChange={(f) => st.setPreferredFormat(f)}
            options={[
              ["s32", "S32", "32-bit samples in 4 bytes. Most USB DACs take this; many convert only the top 24 bits."],
              ["s24", "S24", "24-bit samples padded into 4 bytes (24 bits of audio, 8 unused)."],
              ["s24_3", "S24 3-byte", "24-bit samples packed into 3 bytes. Some older USB DACs only accept this."],
              ["s16", "S16", "16-bit samples (CD resolution). Use only for DACs that offer nothing better."],
            ]}
          />
          <span className="hint">In use: {fmtSampleFormat(negotiated)}</span>
        </div>
        <div className="row">
          <span className="k" title="The resolution the 64-bit processed signal is rounded to. Dither noise sits at this bit depth, below the DAC's own noise when it matches what the DAC really resolves.">
            Dither to
          </span>
          <Seg<number>
            label="Dither target"
            value={out.bits}
            onChange={(bits) => st.setOutput({ bits })}
            options={[
              [16, "16 bits", "Round to 16 bits: dither noise near −96 dBFS. For 16-bit DACs."],
              [20, "20 bits", "Round to 20 bits: dither noise near −120 dBFS."],
              [24, "24 bits", "Round to 24 bits: dither noise near −144 dBFS, below any real DAC's noise. A good default."],
              [32, "32 bits", "Round to 32 bits: dither far below anything a DAC can resolve; effectively plain rounding."],
            ]}
          />
        </div>
        <div className="row">
          <span className="k" title="Dither adds a tiny amount of noise before rounding so the rounding error becomes plain, steady noise instead of distortion that follows the music.">
            Dither
          </span>
          <Seg<DitherKind>
            label="Dither"
            value={out.dither}
            onChange={(dither) => st.setOutput({ dither })}
            options={[
              ["none", "Off", "Plain rounding: the rounding error stays correlated with the signal (low-level distortion)."],
              ["tpdf", "TPDF", "Triangular (TPDF) dither: flat, signal-independent noise at the target bit depth. The safe choice."],
              ["shaped", "Noise-shaped", "TPDF dither with the noise pushed towards high frequencies where hearing is least sensitive: lower audible noise, more total noise. Most useful at 16 bits."],
            ]}
          />
        </div>
      </div>
      <p className="lede">Dither is applied once, at the final conversion to the DAC format, and only when processing is on. Bypass and Direct playback are never dithered. Many 32-bit DACs resolve about 24 bits, so 24 is a good target.</p>
    </section>
  );
}
