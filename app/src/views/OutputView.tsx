import { useState } from "react";
import type { DitherKind, SampleFormat } from "../api/types";
import { comparing, outputLabel, outputPatch } from "../lib/abCompare";
import { NOT_SUPPORTED_BY_DEVICE, effectiveDitherBits, formatBits, ditherAdvice, formatUnavailableReason, supportsFormat, usedBits } from "../lib/deviceCaps";
import { fmtSampleFormat } from "../lib/format";
import { store, useApp } from "../store/store";
import { Seg } from "../components/Seg";
import { ClipWarning } from "../components/ClipWarning";
import { DITHERS, ditherInfo, ditherRateWarning } from "../lib/effects";
import { PresetControl } from "../components/PresetMenu";

const ORIGINAL_DITHERS: readonly DitherKind[] = ["none", "tpdf", "shaped"];

export function OutputView() {
  const st = store();
  const cfg = useApp((s) => s.config);
  const negotiated = useApp((s) => s.status?.format ?? null);
  const caps = useApp((s) => s.status?.device_caps ?? null);
  const outputRate = useApp((s) => s.status?.output_rate ?? null);
  const statusDitherBits = useApp((s) => s.status?.dither_bits ?? null);
  const ab = useApp((s) => s.config?.ab ?? null);
  const [cmpBits, setCmpBits] = useState<number | null>(null);
  const [cmpDither, setCmpDither] = useState<DitherKind | null>(null);
  if (!cfg) return null;
  const out = cfg.chain.output;
  const runningOut = ab && "patch" in ab.b ? ((ab.b.patch as { output?: { bits?: number; dither?: DitherKind } } | null)?.output ?? null) : null;
  const isComparingOut =
    runningOut !== null &&
    runningOut.bits !== undefined &&
    runningOut.dither !== undefined &&
    comparing(ab, outputPatch(runningOut.bits, runningOut.dither).patch);
  const bBits = cmpBits ?? runningOut?.bits ?? out.bits;
  const bDither = cmpDither ?? runningOut?.dither ?? out.dither;
  const bEffectiveBits = effectiveDitherBits(bBits, negotiated);
  const bSameAsA = bBits === out.bits && bDither === out.dither;
  const bMatchesRunning = isComparingOut && runningOut!.bits === bBits && runningOut!.dither === bDither;
  const dither = ditherInfo(out.dither);
  // An engine without the limiter predates the newer dither kinds: only none, TPDF and shaped exist there.
  const oldDither = useApp((s) => s.stageCaps.ditherKinds === false);
  const ditherOptions = (group: "dither" | "shaping") =>
    DITHERS.filter((d) => d.group === group).map((d): [DitherKind, string, string, boolean] => {
      const unavailable = oldDither && !ORIGINAL_DITHERS.includes(d.kind);
      return [d.kind, d.label, unavailable ? "This engine version does not have this dither. Update the engine." : d.text + (d.rateNote ? ` ${d.rateNote}` : ""), unavailable];
    });
  const rateWarning = ditherRateWarning(out.dither, outputRate);
  // The newer modes run the quantizer on their own (control-plane.md, "Output format and dither").
  const explicit = !ORIGINAL_DITHERS.includes(out.dither);
  const preferred = cfg.formats[0] ?? null;
  // Formats the device takes at no rate at all are greyed out.
  const fmtOption = (f: SampleFormat, text: string, title: string): [SampleFormat, string, string, boolean] =>
    caps && !supportsFormat(caps, f) ? [f, text, `${NOT_SUPPORTED_BY_DEVICE}. ${title}`, true] : [f, text, title, false];
  const whyNot = formatUnavailableReason(caps, preferred, negotiated, outputRate);
  // Dither never targets more bits than the format in use carries.
  const maxBits = negotiated ? formatBits(negotiated) : null;
  const ditherBits = statusDitherBits ?? effectiveDitherBits(out.bits, negotiated);
  const capped = negotiated !== null && ditherBits < out.bits;
  // What the DAC really converts ("S32" may carry only 24 bits), and the
  // dither target that matches it.
  const used = usedBits(caps, outputRate, negotiated);
  const advice = ditherAdvice(out.bits, negotiated, used);
  const bitsOption = (bits: number, text: string, title: string): [number, string, string, boolean] =>
    maxBits !== null && bits > maxBits
      ? [bits, text, `${title} Not available: the DAC format in use (${fmtSampleFormat(negotiated)}) carries ${maxBits} bits.`, true]
      : [bits, text, title, false];
  return (
    <section className="view" aria-labelledby="out-title">
      <div className="view-head">
        <h1 id="out-title">Output</h1>
        {isComparingOut && runningOut?.bits !== undefined && runningOut?.dither !== undefined && (
          <button
            type="button"
            className="btn small compare-btn on"
            aria-pressed
            title={`Stop comparing with ${ab?.label || outputLabel(runningOut.bits, runningOut.dither)}: turns A/B off and plays A.`}
            onClick={() => void st.compareOutput(runningOut.bits!, runningOut.dither!)}
          >
            Stop compare
          </button>
        )}
        <div className="spacer" />
        <PresetControl part="output" />
      </div>
      <ClipWarning />
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
            {used !== null && negotiated && used < formatBits(negotiated) ? `, the DAC converts ${used} of its ${formatBits(negotiated)} bits` : ""}
            {used !== null && negotiated && used >= formatBits(negotiated) ? `, all ${used} bits used` : ""}
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
          {advice && (
            <span className="hint" title={advice.title}>
              {advice.text}
              {advice.offer !== null && (
                <>
                  {" "}
                  <button className="link-btn" onClick={() => advice.offer !== null && st.setOutput({ bits: advice.offer })}>
                    Use {advice.offer} bits
                  </button>
                </>
              )}
            </span>
          )}
        </div>
        <div className="row">
          <span className="k" title="Dither adds a tiny amount of noise before rounding so the rounding error becomes plain, steady noise instead of distortion that follows the music.">
            Dither
          </span>
          <Seg<DitherKind>
            label="Dither"
            value={dither?.group === "dither" ? out.dither : null}
            onChange={(d) => st.setOutput({ dither: d })}
            options={ditherOptions("dither")}
          />
        </div>
        <div className="row">
          <span className="k" title="Noise shaping feeds the rounding error back so its noise moves out of the audio band, to where hearing is least sensitive or above 20 kHz. Most of these need a high output rate (upsampling).">
            Noise shaping
          </span>
          <Seg<DitherKind>
            label="Noise shaping"
            value={dither?.group === "shaping" ? out.dither : null}
            onChange={(d) => st.setOutput({ dither: d })}
            options={ditherOptions("shaping")}
          />
        </div>
        <div className="row">
          <span className="k" />
          <div className="dither-notes">
            <span className="dither-desc">
              {dither ? `${dither.label}: ${dither.text}` : out.dither}
              {dither?.rateNote && !rateWarning ? ` ${dither.rateNote}` : ""}
              {explicit ? " Turns the quantizer on even when no other stage runs, so the output is not bit-perfect." : ""}
            </span>
            {rateWarning && (
              <span className="dither-desc warn" role="status">
                {rateWarning}
              </span>
            )}
          </div>
        </div>
        <div className="row">
          <span className="k" title="Compare A with side B using another bit depth or dither mode. Both quantizers run at once.">
            Compare B
          </span>
          <label className="chip compare-chip">
            <span>Bits</span>
            <select
              aria-label="Compare side B bit depth"
              value={bBits}
              onChange={(e) => setCmpBits(Number(e.target.value))}
            >
              {[16, 20, 24, 32].map((b) => (
                <option key={b} value={b} disabled={maxBits !== null && b > maxBits}>
                  {b}-bit
                </option>
              ))}
            </select>
          </label>
          <label className="chip compare-chip">
            <span>Dither</span>
            <select
              aria-label="Compare side B dither"
              value={bDither}
              onChange={(e) => setCmpDither(e.target.value as DitherKind)}
            >
              {DITHERS.map((d) => {
                const unavailable = oldDither && !ORIGINAL_DITHERS.includes(d.kind);
                return (
                  <option key={d.kind} value={d.kind} disabled={unavailable}>
                    {d.label}
                  </option>
                );
              })}
            </select>
          </label>
          <button
            type="button"
            className={`btn small compare-btn${bMatchesRunning ? " on" : ""}`}
            aria-pressed={bMatchesRunning || undefined}
            disabled={!bMatchesRunning && bSameAsA}
            title={
              bMatchesRunning
                ? "Stop output compare: turns A/B off and plays A."
                : bSameAsA
                  ? "Pick a different bit depth or dither from A to compare."
                  : `Compare A with B = ${outputLabel(bBits, bDither)}.`
            }
            onClick={() => void st.compareOutput(bBits, bDither)}
          >
            {bMatchesRunning ? "Stop compare" : isComparingOut ? "Update B" : "Compare"}
          </button>
          {negotiated !== null && bEffectiveBits < bBits && (
            <span className="hint">
              B dithers to {bEffectiveBits} bits ({fmtSampleFormat(negotiated)})
            </span>
          )}
        </div>
      </div>
      <p className="lede">Dither is applied once, at the final conversion to the DAC format, and only when processing is on. Bypass and Direct playback are never dithered. Many 32-bit DACs resolve about 24 bits, so 24 is a good target.</p>
    </section>
  );
}

