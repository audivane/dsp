import { useEffect, useRef, useState } from "react";
import { fmtDb } from "../lib/format";
import { meterChainGainDb, meterStore } from "../store/meters";
import { cssVar } from "../theme";

const FLOOR = -60;
const HOLD_MS = 1500;
const FALL_DB_PER_S = 24;
const CLIP_YELLOW_DB = -6;
const CLIP_RED_DB = -0.1;
/** Readouts are fixed-width (see `.meters .readout` in app.css) so a changing
 *  number never shifts the bars; values are clamped to fit, e.g.
 *  "chain −99.9 dB" / "peak +99.9 dBFS". */
const READOUT_MAX_DB = 99.9;

const pct = (db: number) => Math.max(0, Math.min(1, (db - FLOOR) / -FLOOR));
const YELLOW_AT = pct(CLIP_YELLOW_DB);
const RED_AT = pct(CLIP_RED_DB);

interface Chan {
  peak: number;
  rms: number;
  hold: number;
  holdAt: number;
}

/** L/R peak meters with RMS marker, peak hold, headroom readout, and per-channel clip LEDs. */
export function Meters() {
  const canvas = useRef<HTMLCanvasElement>(null);
  const readout = useRef<HTMLSpanElement>(null);
  const headroom = useRef<HTMLSpanElement>(null);
  const [clips, setClips] = useState<[number, number]>([0, 0]);

  useEffect(() => {
    const cv = canvas.current!;
    const ctx = cv.getContext("2d")!;
    const ch: Chan[] = [0, 1].map(() => ({ peak: FLOOR, rms: FLOOR, hold: FLOOR, holdAt: 0 }));
    let raf = 0;
    let last = performance.now();
    let w = 0;
    let h = 0;
    let hrSmooth: number | null = null;
    let colors = { ok: "", warn: "", bad: "", track: "", line: "", fg: "", muted: "", mono: "" };
    const readColors = () => {
      colors = {
        ok: cssVar("--ok"),
        warn: cssVar("--warn"),
        bad: cssVar("--bad"),
        track: cssVar("--panel-2"),
        line: cssVar("--line"),
        fg: cssVar("--fg"),
        muted: cssVar("--muted"),
        mono: cssVar("--mono"),
      };
    };
    const resize = () => {
      const r = cv.getBoundingClientRect();
      const d = window.devicePixelRatio || 1;
      w = r.width;
      h = r.height;
      cv.width = Math.round(w * d);
      cv.height = Math.round(h * d);
      ctx.setTransform(d, 0, 0, d, 0, 0);
      readColors();
    };
    const ro = new ResizeObserver(resize);
    ro.observe(cv);
    const mo = new MutationObserver(readColors);
    mo.observe(document.documentElement, { attributes: true, attributeFilter: ["data-skin"] });
    resize();
    let lastText = "";
    let lastHrText = "";
    let lastHrWarn = false;
    let lastClipKey = "0:0";
    const frame = (now: number) => {
      raf = requestAnimationFrame(frame);
      const dt = Math.min(0.1, (now - last) / 1000);
      last = now;
      const m = meterStore.latest;
      const fresh = m && now - m.at < 500;
      for (let i = 0; i < 2; i++) {
        const c = ch[i];
        const p = fresh ? (m.meters.peak_db[i] ?? m.meters.peak_db[0] ?? FLOOR) : FLOOR;
        const r = fresh ? (m.meters.rms_db[i] ?? m.meters.rms_db[0] ?? FLOOR) : FLOOR;
        c.peak = Math.max(p, c.peak - FALL_DB_PER_S * dt);
        c.rms = c.rms + (Math.max(FLOOR, r) - c.rms) * Math.min(1, dt * 12);
        if (c.peak >= c.hold || now - c.holdAt > HOLD_MS) {
          c.hold = c.peak;
          c.holdAt = now;
        }
      }
      paint();
      const pk = Math.max(ch[0].hold, ch[1].hold);
      // Clamped so the text never outgrows its fixed-width box (15ch).
      const text = pk <= FLOOR + 0.5 ? "peak −∞" : `peak ${fmtDb(Math.min(pk, READOUT_MAX_DB), 1, pk > 0)} dBFS`;
      if (text !== lastText && readout.current) {
        readout.current.textContent = text;
        lastText = text;
      }

      const rawHr = fresh ? meterChainGainDb(m.meters) : null;
      if (rawHr !== null) {
        hrSmooth = hrSmooth === null ? rawHr : hrSmooth + (rawHr - hrSmooth) * Math.min(1, dt * 8);
      } else {
        // Silent, stale, or the chain is not in the path (Direct / Bypass).
        hrSmooth = null;
      }
      const hrText =
        hrSmooth === null ? "chain —" : `chain ${fmtDb(Math.max(-READOUT_MAX_DB, Math.min(READOUT_MAX_DB, hrSmooth)), 1)} dB`;
      const hrWarn = hrSmooth !== null && hrSmooth > 0.05;
      if ((hrText !== lastHrText || hrWarn !== lastHrWarn) && headroom.current) {
        headroom.current.textContent = hrText;
        headroom.current.style.color = hrWarn ? colors.warn : "";
        lastHrText = hrText;
        lastHrWarn = hrWarn;
      }

      const [cL, cR] = meterStore.clipsPerChannel(2);
      const clipKey = `${cL}:${cR}`;
      if (clipKey !== lastClipKey) {
        lastClipKey = clipKey;
        setClips([cL, cR]);
      }
    };
    const paint = () => {
      ctx.clearRect(0, 0, w, h);
      const labelW = 12;
      const trackX = labelW + 4;
      const trackW = w - trackX - 1;
      const innerW = Math.max(0, trackW - 2);
      const rowH = 7;
      const gap = (h - rowH * 2) / 3;
      ctx.font = "11px " + colors.mono;
      ctx.textBaseline = "middle";
      for (let i = 0; i < 2; i++) {
        const y = gap + i * (rowH + gap);
        ctx.fillStyle = colors.muted;
        ctx.textAlign = "left";
        ctx.fillText(i === 0 ? "L" : "R", 0, y + rowH / 2 + 0.5);
        ctx.fillStyle = colors.track;
        ctx.strokeStyle = colors.line;
        ctx.lineWidth = 1;
        ctx.beginPath();
        ctx.roundRect(trackX + 0.5, y + 0.5, trackW - 1, rowH - 1, 3);
        ctx.fill();
        ctx.stroke();
        const c = ch[i];
        const fw = pct(c.peak) * innerW;
        if (fw > 0) {
          ctx.save();
          ctx.beginPath();
          ctx.roundRect(trackX + 1, y + 1, innerW, rowH - 2, 2);
          ctx.clip();
          const segs: [number, number, string][] = [
            [0, YELLOW_AT, colors.ok],
            [YELLOW_AT, RED_AT, colors.warn],
            [RED_AT, 1, colors.bad],
          ];
          for (const [a, b, col] of segs) {
            const x0 = trackX + 1 + a * innerW;
            const x1 = Math.min(trackX + 1 + fw, trackX + 1 + b * innerW);
            if (x1 > x0) {
              ctx.fillStyle = col;
              ctx.fillRect(x0, y + 1, x1 - x0, rowH - 2);
            }
          }
          if (c.peak >= CLIP_RED_DB) {
            ctx.fillStyle = colors.bad;
            ctx.fillRect(trackX + 1 + Math.max(0, innerW - 3), y + 1, Math.min(3, innerW), rowH - 2);
          }
          ctx.restore();
        }
        ctx.fillStyle = colors.fg;
        ctx.globalAlpha = 0.55;
        ctx.fillRect(trackX + 1 + pct(c.rms) * innerW - 1, y, 2, rowH);
        ctx.globalAlpha = 1;
        if (c.hold > FLOOR + 0.5) {
          ctx.fillStyle = c.hold >= CLIP_RED_DB ? colors.bad : c.hold >= CLIP_YELLOW_DB ? colors.warn : colors.fg;
          ctx.fillRect(trackX + 1 + pct(c.hold) * innerW - 2, y - 1, 2, rowH + 2);
        }
      }
    };
    raf = requestAnimationFrame(frame);
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
      mo.disconnect();
    };
  }, []);

  const chanNames = ["Left", "Right"] as const;
  return (
    <div className="meters">
      <canvas ref={canvas} role="img" aria-label="Output level, left and right" />
      <span
        className="readout mono"
        ref={readout}
        title="Output peak (held 1.5 s), the same level as the bars: the signal sent to the DAC, after the volume, just before conversion to integers. 0 dBFS is full scale; at or above it the signal clips and the clip light turns on."
      >
        {"peak −∞"}
      </span>
      <span
        className="headroom mono"
        ref={headroom}
        title="Chain gain at the peaks: what EQ, convolution and the upsampler add (output peak before the volume minus input peak). A relative gain, not a level: +2 dB only clips if the music already peaks within 2 dB of full scale. The volume is not included."
      >
        {"chain —"}
      </span>
      <div className="clip-leds" role="group" aria-label="Channel clip indicators">
        {([0, 1] as const).map((i) => {
          const count = clips[i];
          const on = count > 0;
          const lbl = i === 0 ? "L" : "R";
          return (
            <button
              key={i}
              className={"clip" + (on ? " on" : "")}
              title={on ? `${chanNames[i]} channel reached or exceeded 0 dBFS (${count}). Click to reset.` : `${chanNames[i]} channel: no clipping since the last reset`}
              aria-label={on ? `${chanNames[i]} clip count ${count}. Reset` : `${chanNames[i]} channel no clipping`}
              onClick={() => {
                meterStore.resetClip(i);
                const [l, r] = meterStore.clipsPerChannel(2);
                setClips([l, r]);
              }}
            >
              <span className="light" />
              <span>{on ? `${lbl} ${count}` : lbl}</span>
            </button>
          );
        })}
      </div>
    </div>
  );
}
