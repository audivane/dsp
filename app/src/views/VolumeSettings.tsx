// Volume settings (not the level): where the volume is applied, the ceiling,
// what the desktop slider does, and the guard against apps getting louder.
// Written with get_config + set_config, like `avdsp ctl volume`, with the level,
// balance and mute left out: those stay with the top bar's guarded writes.
// Each box checks the engine's bounds before anything is sent.

import type { BlockedVolumeChange, GuardMode, SinkVolumePolicy, VolumeConfig, VolumeMode } from "../api/types";
import { fmtDb, fmtInt } from "../lib/format";
import { volumeFieldProblem } from "../lib/volumeSettings";
import { store, useApp } from "../store/store";
import { Seg } from "../components/Seg";
import { InlineError, NumField } from "../components/StageParts";

const REASON: Record<string, string> = {
  locked: "held by the lock",
  capped: "capped",
  slewed: "slowed down",
  direct_bit_perfect: "kept at 100% on Direct",
};

function blockedText(b: BlockedVolumeChange): string {
  const src = b.source.kind === "stream" ? (b.source.app ?? `stream ${b.source.node}`) : b.source.kind === "sink" ? `the ${b.source.sink === "direct" ? "Direct" : "DSP"} sink` : "a control client";
  return `Last: ${src} asked for ${fmtDb(b.requested_db, 1)} dB, ${REASON[b.reason] ?? b.reason} at ${fmtDb(b.applied_db, 1)} dB.`;
}

export function VolumeSettings() {
  const st = store();
  const vol = useApp((s) => s.config?.volume ?? null);
  const status = useApp((s) => s.status?.volume ?? null);
  const supported = useApp((s) => s.stageCaps.volumeSettings !== false);
  // An engine older than the volume guard sends no `guard`: nothing here can be set.
  if (!vol || !supported || !vol.guard) {
    return (
      <div className="panel vol-panel">
        <h2>Volume</h2>
        <p className="lede">This engine version cannot change the volume settings. Update the engine to use them.</p>
      </div>
    );
  }
  const set = (fn: (v: VolumeConfig) => VolumeConfig) => void st.setVolumeSettings(fn);
  const g = vol.guard;
  const noRiseLimit = g.max_rise_db_per_s === 0;
  return (
    <div className="panel vol-panel" aria-labelledby="vol-title">
      <h2 id="vol-title">Volume</h2>
      <div className="row">
        <span className="k" title="Where the master volume is applied: in the DAC's own volume control (keeps the full digital resolution and bit-perfect Direct), in software (64-bit float before dither), or both.">
          Volume control
        </span>
        <Seg<VolumeMode>
          label="Volume control"
          value={vol.mode}
          onChange={(mode) => set((v) => ({ ...v, mode, hardware_db: mode === "hybrid" && v.hardware_db === null ? Math.min(0, status?.hardware_db ?? -10) : v.hardware_db }))}
          options={[
            ["auto", "Auto", "Use the DAC's hardware volume when it has one, otherwise software."],
            ["software", "Software", "Always digital, in the engine. The DAC's own volume stays where it is, or at the fixed level below."],
            ["hardware", "Hardware", "Always the DAC's volume control. If it has none, the engine falls back to software and says so."],
            ["hybrid", "Hybrid", "The DAC sits at a fixed level and software lowers from there; only louder than that raises the DAC. Needs a DAC level."],
          ]}
        />
        {status?.hardware_unavailable && (
          <span className="notice warn" role="status">
            This DAC has no hardware volume control: the volume is digital.
          </span>
        )}
      </div>
      {(vol.mode === "software" || vol.mode === "hybrid") && (
        <div className="row">
          <label className="k" htmlFor="vol-hw" title={vol.mode === "hybrid" ? "The level the DAC stays at for every volume at or below it, −120 to 0 dB. Required in hybrid." : "A fixed level the engine sets the DAC to when it opens it, −120 to 0 dB. Leave empty to leave the DAC alone."}>
            DAC level
          </label>
          <NumField
            id="vol-hw"
            label="DAC hardware level in dB"
            value={vol.hardware_db}
            unit="dB"
            placeholder={vol.mode === "hybrid" ? "required" : "leave alone"}
            check={(v) => volumeFieldProblem("hardware", v, vol.mode)}
            onCommit={(v) => set((x) => ({ ...x, hardware_db: v }))}
          />
          {status?.hardware_unavailable && <span className="hint">No hardware control on this DAC: this level has no effect.</span>}
        </div>
      )}
      <div className="row">
        <label className="k" htmlFor="vol-ceiling" title="The loudest the master volume can ever go, −120 to 0 dB, from any source (this app, the desktop, the command line). Empty: no ceiling.">
          Ceiling
        </label>
        <NumField id="vol-ceiling" label="Volume ceiling in dB" value={vol.max_volume_db} unit="dB" placeholder="none" check={(v) => volumeFieldProblem("ceiling", v)} onCommit={(v) => set((x) => ({ ...x, max_volume_db: v }))} />
        <span className="hint">{vol.max_volume_db === null ? "No ceiling" : `The volume never goes above ${fmtDb(vol.max_volume_db, 1)} dB`}</span>
      </div>
      <div className="row">
        <span className="k" title="What the desktop volume slider (and media keys, pavucontrol) does on the Audivane outputs.">
          Desktop slider
        </span>
        <Seg<SinkVolumePolicy>
          label="Desktop volume slider"
          value={vol.sink_volume}
          onChange={(sink_volume) => set((v) => ({ ...v, sink_volume }))}
          options={[
            ["follow", "Controls Audivane", "The desktop slider moves Audivane's volume (with the ceiling and guard), and PipeWire passes the audio untouched."],
            ["ignore", "Left to PipeWire", "The desktop slider scales the audio inside PipeWire, before Audivane sees it. Below 100% the output is not bit-perfect."],
          ]}
        />
      </div>
      <div className="row">
        <span className="k" title="Protection against an app (or the desktop) suddenly getting louder while Audivane drives the DAC hot.">
          Guard
        </span>
        <Seg<GuardMode>
          label="Volume guard"
          value={g.mode}
          onChange={(mode) => set((v) => ({ ...v, guard: { ...v.guard, mode } }))}
          options={[
            ["off", "Off", "No protection."],
            ["lock", "Lock", "Apps cannot raise their own volume, and the desktop slider cannot raise Audivane's. Turning down and mute still work."],
            ["limit", "Limit", "Rises are capped, and slowed to the rate below unless there is no rise limit; turning down passes at once."],
          ]}
        />
        <span className="blocked" title="Volume changes the guard or the ceiling blocked or reduced since the engine started">
          {fmtInt(status?.blocked ?? 0)} blocked
        </span>
      </div>
      {g.mode === "limit" && (
        <div className="row">
          <label className="k" htmlFor="vol-rise" title="The fastest a volume may rise, 0 to 1000 dB per second. 0 lets a rise through at once (only the caps apply).">
            Max rise
          </label>
          <NumField
            id="vol-rise"
            label="Maximum rise in dB per second"
            value={g.max_rise_db_per_s}
            unit="dB/s"
            disabled={noRiseLimit}
            title={noRiseLimit ? "No rise limit: rises are not slowed, only capped. Untick the box to set a rate." : undefined}
            check={(v) => volumeFieldProblem("rise", v)}
            onCommit={(v) => v !== null && void st.setGuardRiseRate(v)}
          />
          <label
            className="ups-check"
            title="Rises go straight to the requested level, still capped by the ceiling and the app stream cap (max rise 0 dB/s: only the cap). This also stops the desktop slider stepping while a rise is slowed down. Turning it off restores the last rate (6 dB/s if none)."
          >
            <input type="checkbox" checked={noRiseLimit} onChange={(e) => void st.setGuardRiseLimit(!e.target.checked)} />
            <span>No rise limit (only the cap)</span>
          </label>
          <label className="k" htmlFor="vol-stream" title="The loudest an app's own stream volume may be set to, −120 to 0 dB (0 dB = 100%). Empty: 100%.">
            App stream cap
          </label>
          <NumField
            id="vol-stream"
            label="App stream volume cap in dB"
            value={g.stream_max_db}
            unit="dB"
            placeholder="100%"
            check={(v) => volumeFieldProblem("stream", v)}
            onCommit={(v) => set((x) => ({ ...x, guard: { ...x.guard, stream_max_db: v } }))}
          />
        </div>
      )}
      {status?.last_blocked && <p className="blocked">{blockedText(status.last_blocked)}</p>}
      <InlineError slot="volume" />
    </div>
  );
}
