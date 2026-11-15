import type { DitherKind, SampleFormat } from "../api/types";
import { NOT_SUPPORTED_BY_DEVICE, effectiveDitherBits, formatBits, formatUnavailableReason, supportsFormat } from "../lib/deviceCaps";
import { fmtSampleFormat } from "../lib/format";
import { store, useApp } from "../store/store";
import { Seg } from "../components/Seg";

export function OutputView() {
  const st = store();
  const cfg = useApp((s) => s.config);
  const negotiated = useApp((s) => s.status?.format ?? null);
  const caps = useApp((s) => s.status?.device_caps ?? null);
  const outputRate = useApp((s) => s.status?.output_rate ?? null);
  const statusDitherBits = useApp((s) => s.status?.dither_bits ?? null);
  if (!cfg) return null;
  const out = cfg.chain.output;
  const preferred = cfg.formats[0] ?? null;
  // Formats the device takes at no rate at all are greyed out.
  const fmtOption = (f: SampleFormat, text: string, title: string): [SampleFormat, string, string, boolean] =>
    caps && !supportsFormat(caps, f) ? [f, text, `${NOT_SUPPORTED_BY_DEVICE}. ${title}`, true] : [f, text, title, false];
  const whyNot = formatUnavailableReason(caps, preferred, negotiated, outputRate);
  // Dither never targets more bits than the format in use carries.
  const maxBits = negotiated ? formatBits(negotiated) : null;
  const ditherBits = statusDitherBits ?? effectiveDitherBits(out.bits, negotiated);
  const capped = negotiated !== null && ditherBits < out.bits;
  const bitsOption = (bits: number, text: string, title: string): [number, string, string, boolean] =>
    maxBits !== null && bits > maxBits
      ? [bits, text, `${title} Not available: the DAC format in use (${fmtSampleFormat(negotiated)}) carries ${maxBits} bits.`, true]
      : [bits, text, title, false];
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
              fmtOption("s32", "S32", "32-bit samples in 4 bytes. Most USB DACs take this; many convert only the top 24 bits."),
              fmtOption("s24", "S24", "24-bit samples padded into 4 bytes (24 bits of audio, 8 unused)."),
              fmtOption("s24_3", "S24 3-byte", "24-bit samples packed into 3 bytes. Some older USB DACs only accept this."),
              fmtOption("s16", "S16", "16-bit samples (CD resolution). Use only for DACs that offer nothing better."),
            ]}
          />
          <span className="hint">
            In use: {fmtSampleFormat(negotiated)}
            {whyNot ? ` (${whyNot})` : ""}
          </span>
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
              bitsOption(16, "16 bits", "Round to 16 bits: dither noise near −96 dBFS. For 16-bit DACs."),
              bitsOption(20, "20 bits", "Round to 20 bits: dither noise near −120 dBFS."),
              bitsOption(24, "24 bits", "Round to 24 bits: dither noise near −144 dBFS, below any real DAC's noise. A good default."),
              bitsOption(32, "32 bits", "Round to 32 bits: dither far below anything a DAC can resolve; effectively plain rounding."),
            ]}
          />
          {capped && (
            <span className="hint">
              Dithering to {ditherBits} bits (DAC format is {fmtSampleFormat(negotiated)})
            </span>
          )}
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
