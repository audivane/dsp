import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { StreamInfo } from "../api/types";
import { fmtMs, fmtRateShort, fmtSampleFormat } from "../lib/format";
import { verdict } from "../lib/passthrough";
import { isResampling, rateRatioText } from "../lib/summary";
import { useApp } from "../store/store";
import { Meters } from "./Meters";

const STATE_TEXT = { idle: "Device released", ready: "Ready", playing: "Playing", switching: "Switching rate", error: "Engine error" } as const;
const LED = { idle: "idle", ready: "ready", playing: "", switching: "warn", error: "bad" } as const;

function bitsOf(format: string | null | undefined): string | null {
  if (!format) return null;
  const m = format.match(/^([SUF])(\d+)/i);
  if (!m) return null;
  return m[1].toUpperCase() === "F" ? `${m[2]}-bit float` : `${m[2]}-bit`;
}

function sourceText(s: StreamInfo | undefined, bits: number | null | undefined, graphRate: number) {
  // The upsampling ratio applies to the live graph rate, after client resampling.
  const rate = fmtRateShort(graphRate);
  const b = bits ? `${bits}-bit` : bitsOf(s?.format);
  return (
    <>
      {["IN " + rate, b].filter(Boolean).join(" · ")}
      {s?.channels ? <span className="ch"> · {s.channels}ch</span> : null}
    </>
  );
}

export function fmtXruns(n: number): string {
  if (n > 99999) return "99999+ xruns";
  return `${n} ${n === 1 ? "xrun" : "xruns"}`;
}

function Badge() {
  const status = useApp((s) => s.status);
  const streams = useApp((s) => s.streams);
  const [open, setOpen] = useState(false);
  const wrap = useRef<HTMLSpanElement>(null);
  const pop = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const off = (e: MouseEvent) => !wrap.current?.contains(e.target as Node) && setOpen(false);
    const esc = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("mousedown", off);
    document.addEventListener("keydown", esc);
    return () => {
      document.removeEventListener("mousedown", off);
      document.removeEventListener("keydown", esc);
    };
  }, [open]);
  useLayoutEffect(() => {
    if (!open) return;
    const place = () => {
      const w = wrap.current;
      const p = pop.current;
      if (!w || !p) return;
      const pad = 8;
      const bx = w.getBoundingClientRect().left;
      const pw = p.offsetWidth;
      const vw = window.innerWidth;
      const x = Math.max(pad, Math.min(bx, vw - pad - pw));
      p.style.left = `${Math.round(x - bx)}px`;
    };
    place();
    window.addEventListener("resize", place);
    return () => window.removeEventListener("resize", place);
  }, [open]);
  const v = verdict(status?.passthrough, streams);
  const icon = v.kind === "ok" ? "✓" : v.kind === "proc" ? "◆" : v.kind === "warn" ? "!" : "–";
  const tip = [v.summary, ...v.reasons].join(" · ");
  return (
    <span className="badge-wrap" ref={wrap}>
      <button className={"badge " + v.kind} title={tip} aria-expanded={open} aria-haspopup="dialog" onClick={() => setOpen(!open)}>
        <span aria-hidden="true">{icon}</span> {v.label}
      </button>
      {open && (
        <div className="popover" ref={pop} role="dialog" aria-label="Output check">
          <p>
            <b>{v.label}.</b> {v.summary}
          </p>
          {v.reasons.length > 0 && (
            <ul>
              {v.reasons.map((r) => (
                <li key={r}>{r}</li>
              ))}
            </ul>
          )}
        </div>
      )}
    </span>
  );
}

function latencyTip(total: number, filter: number): string {
  const buffer = Math.max(0, total - filter);
  const lines = [`Latency added by the engine: ${fmtMs(total)}.`, `Output buffer: ${fmtMs(buffer)}.`];
  lines.push(
    filter > 0.05
      ? `Filter delay (upsampler, convolution and limiter in the active chain): ${fmtMs(filter)}.`
      : "Filter delay: none (minimum-phase or no filters, or the Direct sink / DSP off, where the chain is not in the path).",
  );
  return lines.join("\n");
}

export function StatusBar() {
  const status = useApp((s) => s.status);
  const np = useApp((s) => s.nowPlaying);
  const streams = useApp((s) => s.streams);
  // While the limiter runs, the meter bars yield room to the gain-reduction
  // readout at narrow widths so the now-playing text stays visible.
  const limiting = useApp((s) => !!s.config?.dsp_enabled && (s.config?.chain.limiter?.mode ?? "off") !== "off");
  if (!status) return <footer className="status" aria-label="Engine status" />;
  const feeding = streams.filter((s) => status.active_sink && s.sink === status.active_sink);
  const src = feeding[0];
  const npText = np && (np.title || np.artist) ? [np.player.replace(/^org\.mpris\.MediaPlayer2\./, "").replace(/^\w/, (c) => c.toUpperCase()), [np.artist, np.title].filter(Boolean).join(" · ")].join(" · ") : null;
  const load = Math.round(status.dsp_load * 100);
  return (
    <footer className={"status" + (limiting ? " limiting" : "")} aria-label="Engine status">
      <span className="state">
        <span className={"led " + (status.waiting_for_device ? "warn" : LED[status.state])} />
        <span>{status.waiting_for_device ? "Waiting for device" : STATE_TEXT[status.state]}</span>
      </span>
      {npText && (
        <span className="np" title={npText}>
          {npText}
        </span>
      )}
      <span className="path" aria-label="Rate path">
        <span className="pill">{sourceText(src, np?.native_bits ?? null, status.graph_rate)}</span>
        {isResampling(status.rate_ratio) && (
          <>
            <span aria-hidden="true">→</span>
            <span className="pill ratio">{rateRatioText(status.rate_ratio)}</span>
          </>
        )}
        <span aria-hidden="true">→</span>
        <span className="pill">
          OUT {fmtRateShort(status.output_rate || status.graph_rate)}
          {status.format ? " · " + fmtSampleFormat(status.format) : ""}
        </span>
      </span>
      <Badge />
      <span className="stats">
        <span
          className="stat-load"
          title={`DSP load: the share of each audio period's time budget spent on processing (${load}%). Near 100% the engine cannot keep up and drops out.`}
        >
          DSP {load}%
        </span>
        <span className="stat-sep" aria-hidden="true">·</span>
        <span
          className="stat-xruns"
          title="xruns: buffer underruns since the engine started. Each one is an audible dropout or click."
        >
          {fmtXruns(status.xruns)}
        </span>
        <span className="stat-sep stat-latency-sep" aria-hidden="true">·</span>
        <span className="stat-latency" title={latencyTip(status.latency_ms, status.filter_latency_ms ?? 0)}>
          {fmtMs(status.latency_ms)}
        </span>
      </span>
      {status.error && !status.waiting_for_device && (
        <span className="stats status-err" role="status" title={status.error}>
          {status.error}
        </span>
      )}
      <Meters />
    </footer>
  );
}
