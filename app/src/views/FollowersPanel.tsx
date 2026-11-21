import { useState } from "react";
import type { DeviceInfo } from "../api/types";
import { store, useApp } from "../store/store";
import { deviceLabel } from "../components/TopBar";
import { ValueSlider } from "../components/ValueSlider";
import { Close, Plus, Power, Trash } from "../components/Icons";
import {
  FOLLOWER_MIN_DB,
  FOLLOWER_TRIM_MS,
  MAX_FOLLOWERS,
  addBlockedReason,
  followerCandidates,
  followerChip,
  followerDetail,
  followerRows,
  fmtTrim,
  speedCapNote,
  type FollowerRow,
} from "../lib/followers";

const TRIM_TITLE =
  "Its delay behind the main device, −50 to +50 ms (negative plays it earlier). Use it for the DACs' own latencies or speaker distance: 1 ms is about 34 cm. A change makes it re-lock for a moment.";
const VOLUME_TITLE = "Its own digital volume, −120 to 0 dB, after the main volume. Below 0 dB it is not bit-perfect (followers never are: each one corrects its clock).";

function nameOf(devices: readonly DeviceInfo[], id: string): string {
  return devices.find((d) => d.id === id)?.name ?? id;
}

function ErrorLine({ text, onClose }: { text: string; onClose: () => void }) {
  return (
    <div className="inline-err" role="alert">
      <span>{text}</span>
      <button className="icon-btn" aria-label="Dismiss" onClick={onClose}>
        <Close />
      </button>
    </div>
  );
}

function FollowerCard({ row, devices, epoch }: { row: FollowerRow; devices: readonly DeviceInfo[]; epoch: number }) {
  const st = store();
  const f = row.config;
  const error = useApp((s) => s.followerErrors[f.device]);
  const chip = followerChip(row.status);
  const detail = followerDetail(row.status);
  const name = nameOf(devices, f.device);
  return (
    <li className={"follower-card" + (f.enabled ? "" : " off")} aria-label={name}>
      <div className="follower-head">
        <button
          className="pwr"
          aria-pressed={f.enabled}
          aria-label={`Play on ${name}`}
          title={f.enabled ? "Mute this device. It keeps its device and stays in sync, so turning it back on is instant." : "Play on this device again"}
          onClick={() => st.editFollower(f.device, { enabled: !f.enabled })}
        >
          <Power />
        </button>
        <div className="follower-name">
          <div className="n" title={f.device}>
            {name}
          </div>
          <div className="m mono" aria-live="polite">
            {detail ?? f.device}
          </div>
        </div>
        <span className={`follower-chip ${chip.tone}`} title={chip.title} role="status">
          {chip.text}
        </span>
        <button className="icon-btn" aria-label={`Stop playing on ${name}`} title="Remove this device from the list (frees the DAC)" onClick={() => st.removeFollower(f.device)}>
          <Trash />
        </button>
      </div>
      <div className="follower-ctl" key={epoch}>
        <label className="k" htmlFor={`trim-${f.device}`} title={TRIM_TITLE}>
          Delay
        </label>
        <ValueSlider
          id={`trim-${f.device}`}
          label={`Delay of ${name}`}
          value={f.trim_ms}
          min={-FOLLOWER_TRIM_MS}
          max={FOLLOWER_TRIM_MS}
          step={0.1}
          format={fmtTrim}
          title={TRIM_TITLE}
          onChange={(v, mode) => st.editFollower(f.device, { trim_ms: v }, mode)}
          onEnd={() => st.endFollowerDrag(f.device)}
        />
        <label className="k" htmlFor={`vol-${f.device}`} title={VOLUME_TITLE}>
          Volume
        </label>
        <ValueSlider
          id={`vol-${f.device}`}
          label={`Volume of ${name}`}
          value={f.volume_db}
          min={FOLLOWER_MIN_DB}
          max={0}
          step={0.5}
          format={(v) => `${v === 0 ? "0" : v.toFixed(1).replace("-", "−")} dB`}
          title={VOLUME_TITLE}
          onChange={(v, mode) => st.editFollower(f.device, { volume_db: v }, mode)}
          onEnd={() => st.endFollowerDrag(f.device)}
        />
      </div>
      {error && <ErrorLine text={error} onClose={() => st.clearFollowerError(f.device)} />}
    </li>
  );
}

/** "Also play on": the follower devices (multi-device output). */
export function FollowersPanel() {
  const st = store();
  const cfg = useApp((s) => s.config);
  const outputs = useApp((s) => s.status?.outputs);
  const drafts = useApp((s) => s.followerDrafts);
  const devices = useApp((s) => s.devices);
  const addError = useApp((s) => s.followerErrors[""]);
  const epoch = useApp((s) => s.connEpoch);
  const [pick, setPick] = useState("");
  if (!cfg) return null;
  const known = cfg.outputs !== undefined;
  const followers = cfg.outputs?.followers ?? [];
  const rows = followerRows(followers, outputs?.followers, drafts);
  const candidates = followerCandidates(devices, cfg.device, followers);
  const blocked = addBlockedReason(candidates, followers);
  const chosen = candidates.some((d) => d.id === pick) ? pick : (candidates[0]?.id ?? "");
  const capNote = speedCapNote(followers.length, cfg.varispeed.enabled);
  const alignment = outputs?.alignment_ms ?? 0;
  return (
    <div className="panel followers" aria-labelledby="followers-title">
      <div className="followers-head">
        <h2
          id="followers-title"
          title="Play the same music on other DACs at the same time, in sync with the main device. Each one runs its own copy of the processing, so it costs CPU per device."
        >
          Also play on
        </h2>
        <span className="tag">
          {followers.length} of {MAX_FOLLOWERS}
        </span>
        {alignment > 0 && (
          <span className="hint" title="The main device waits this long so the other devices have their audio in time. It grows when one needs more and stays until they are removed.">
            Main device delayed {alignment.toFixed(0)} ms to line them up
          </span>
        )}
      </div>
      {!known ? (
        <p className="followers-note">This engine plays on one device only. Update the engine to play on more.</p>
      ) : (
        <>
          {rows.length > 0 && (
            <ul className="follower-list">
              {rows.map((r) => (
                <FollowerCard key={r.config.device} row={r} devices={devices} epoch={epoch} />
              ))}
            </ul>
          )}
          <div className="row">
            <label className="chip" htmlFor="follower-add" title={blocked ?? "A device to play on as well as the main one"}>
              <select id="follower-add" aria-label="Device to add" value={chosen} disabled={!!blocked} onChange={(e) => setPick(e.target.value)}>
                {candidates.length === 0 && <option value="">No device to add</option>}
                {candidates.map((d) => (
                  <option key={d.id} value={d.id}>
                    {deviceLabel(d.name, d.id)}
                    {d.hdmi ? " (HDMI)" : d.loopback ? " (loopback)" : ""}
                  </option>
                ))}
              </select>
            </label>
            <button className="btn small" disabled={!!blocked || !chosen} onClick={() => st.addFollower(chosen)} title={blocked ?? "Start playing on this device too"}>
              <Plus />
              Add device
            </button>
            {blocked && <span className="hint">{blocked}</span>}
          </div>
          {addError && <ErrorLine text={addError} onClose={() => st.clearFollowerError("")} />}
          {rows.length > 0 && (
            <p className="followers-note">
              Every device plays the same processing; the Direct path and DSP off play on the main device only. A/B compare stays on the main device.
            </p>
          )}
          {capNote && <p className="followers-note">{capNote}</p>}
        </>
      )}
    </div>
  );
}
