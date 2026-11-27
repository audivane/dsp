// Multi-device output ("Also play on"): pure helpers for the follower list
// on the Device page. The engine owns the list (`config.outputs.followers`)
// and reports each follower's state (`status.outputs`); these only decide
// what to offer and how to say it.

import type { DeviceInfo, FollowerConfig, FollowerStatus } from "../api/types";

/** At most this many followers (`MAX_FOLLOWERS`). */
export const MAX_FOLLOWERS = 3;
/** Trim range, ms (`FOLLOWER_MAX_TRIM_MS`). */
export const FOLLOWER_TRIM_MS = 50;
/** Follower volume range, dB. */
export const FOLLOWER_MIN_DB = -120;
/** An enabled varispeed's top speed while any follower is configured. */
export const FOLLOWED_MAX_SPEED = 1.996;

/** One editable follower field set (`set_follower`'s optional fields). */
export type FollowerPatch = Partial<Pick<FollowerConfig, "enabled" | "trim_ms" | "volume_db" | "output_channels" | "follow_volume">>;

/**
 * Whether the follower follows the main volume (main volume changes move it
 * too, on top of its own level); null when the engine predates
 * `follow_volume` (its followers keep their own level, and the toggle is not
 * offered).
 */
export function followsVolume(config: FollowerConfig): boolean | null {
  return typeof config.follow_volume === "boolean" ? config.follow_volume : null;
}

/** Whether a configured device string names this list entry (the engine compares the strings). */
function isDevice(d: DeviceInfo, id: string): boolean {
  return d.id === id;
}

/**
 * Devices that can be added as a follower: every listed device except the
 * main one (by id, or marked `active`) and the ones already following.
 * Empty when the list is full.
 */
export function followerCandidates(devices: readonly DeviceInfo[], mainDevice: string, followers: readonly FollowerConfig[]): DeviceInfo[] {
  if (followers.length >= MAX_FOLLOWERS) return [];
  return devices.filter((d) => !d.active && !isDevice(d, mainDevice) && !followers.some((f) => f.device === d.id));
}

/** Why nothing can be added, or null when something can. */
export function addBlockedReason(candidates: readonly DeviceInfo[], followers: readonly FollowerConfig[]): string | null {
  if (followers.length >= MAX_FOLLOWERS) return `At most ${MAX_FOLLOWERS} other devices.`;
  if (candidates.length === 0) return "No other device to add. Plug one in and refresh the list.";
  return null;
}

/** The follower whose device the main output would move to, or null. */
export function followerOn(device: string, followers: readonly FollowerConfig[]): FollowerConfig | null {
  return followers.find((f) => f.device === device) ?? null;
}

/** A row of the list: the config entry (with any draft) and its status, matched by device. */
export interface FollowerRow {
  config: FollowerConfig;
  status: FollowerStatus | null;
  /**
   * The level its volume slider shows: a draft while one is pending, else
   * the level it plays at (`level_db`: following, the main volume's changes
   * move it), else its `volume_db`.
   */
  level: number;
}

export function followerRows(
  followers: readonly FollowerConfig[],
  status: readonly FollowerStatus[] | undefined,
  drafts: Readonly<Record<string, FollowerPatch>> = {},
): FollowerRow[] {
  return followers.map((f) => {
    const draft = drafts[f.device] ?? {};
    const st = status?.find((s) => s.device === f.device) ?? null;
    const config = { ...f, ...draft };
    const playing = config.follow_volume && typeof st?.level_db === "number" && Number.isFinite(st.level_db) ? st.level_db : config.volume_db;
    return { config, status: st, level: draft.volume_db ?? playing };
  });
}

export type ChipTone = "ok" | "warn" | "bad" | "idle";

export interface FollowerChip {
  text: string;
  tone: ChipTone;
  title: string;
}

/** The status chip of one follower. */
export function followerChip(s: FollowerStatus | null): FollowerChip {
  if (!s) return { text: "Starting", tone: "idle", title: "Waiting for the engine to report this device." };
  switch (s.state) {
    case "off":
      return s.error
        ? { text: "Off", tone: "bad", title: `It could not start: ${s.error}` }
        : { text: "Off", tone: "idle", title: "Not running: the engine is stopped or has released the DAC." };
    case "opening":
      return { text: "Opening", tone: "idle", title: "Opening the device." };
    case "locking":
      return { text: "Locking", tone: "warn", title: "Playing, muted, while its clock is brought in sync with the main device. It is heard once in sync." };
    case "locked":
      return { text: "In sync", tone: "ok", title: "In sync with the main device and heard." };
    case "muted":
      return {
        text: "Muted",
        tone: "idle",
        title: s.enabled
          ? "In sync but not heard: the main device plays the Direct path or with DSP off, which followers do not play."
          : "In sync but not heard: it is turned off here. It keeps its device and stays in sync, so turning it on is instant.",
      };
    case "unaligned":
      return {
        text: `${fmtMs(s.offset_ms, 0)} ms late`,
        tone: "warn",
        title: `In sync and heard, but ${fmtMs(s.offset_ms, 1)} ms later than the main device: its device needs more lead than the main output may wait for (at most 60 ms). A smaller period on this device brings it back in line.`,
      };
    case "lost":
      return { text: "Lost", tone: "bad", title: `The device is gone or failed${s.error ? ` (${s.error})` : ""}. It reopens when it is back.` };
  }
}

/**
 * A follower's time from the player to its DAC, ms, on the same basis as the
 * main device's end-to-end latency (`status.latency_ms`): in sync it plays
 * that much later than the main device as its delay plus `offset_ms`. Null
 * while it is not in sync or the main latency is unknown.
 */
export function followerEndToEndMs(s: FollowerStatus | null, mainLatencyMs: number | null | undefined): number | null {
  if (!s || !mainLatencyMs || mainLatencyMs <= 0) return null;
  if (s.state !== "locked" && s.state !== "muted" && s.state !== "unaligned") return null;
  return mainLatencyMs + s.trim_ms + s.offset_ms;
}

/** The tooltip of a follower's detail line: what its latency figure means, and its own path. */
export function followerLatencyTitle(s: FollowerStatus | null, mainLatencyMs: number | null | undefined): string | undefined {
  if (!s || followerEndToEndMs(s, mainLatencyMs) === null) return undefined;
  const path = s.latency_ms > 0 ? ` Its own path, from where it picks the source up to its DAC: ${fmtMs(s.latency_ms, 1)} ms.` : "";
  return `From the player to this DAC, the same measure as the main device's latency in the status bar (that plus this device's delay${s.offset_ms > 0 ? " and how late it is" : ""}).${path}`;
}

/** The short detail line under a follower: clock, offset, latency, counters. Null before it plays. */
export function followerDetail(s: FollowerStatus | null, mainLatencyMs?: number | null): string | null {
  if (!s || s.state === "off" || s.state === "opening" || s.state === "lost") return s?.state === "lost" && s.error ? s.error : null;
  const parts = [`clock ${fmtPpm(s.ppm)} ppm`];
  if (s.offset_ms > 0) parts.push(`${fmtMs(s.offset_ms, 1)} ms late`);
  const e2e = followerEndToEndMs(s, mainLatencyMs);
  if (e2e !== null) parts.push(`${fmtMs(e2e, 1)} ms latency`);
  if (s.device_rate > 0) parts.push(`${(s.device_rate / 1000).toLocaleString("en-US", { maximumFractionDigits: 1 })} kHz`);
  parts.push(`${s.xruns} ${s.xruns === 1 ? "xrun" : "xruns"}`);
  if (s.skips > 0) parts.push(`${s.skips} ${s.skips === 1 ? "skip" : "skips"}`);
  return parts.join(" · ");
}

/** The chain fields a follower's own chain changes, in words. */
const CHAIN_FIELD: Record<string, string> = {
  convolution: "convolution",
  eq: "EQ",
  upsampler: "upsampler",
  limiter: "limiter",
  output: "output",
  crossfeed: "crossfeed",
  harmonics: "harmonics",
  dc_block: "DC blocker",
  pitch: "pitch",
};

/**
 * "Own chain: convolution differs" when the follower plays its own chain
 * (a `chain_patch` over chain A), null when it plays chain A. The status
 * names what differs; before it reports, the patch's own fields stand in.
 */
export function ownChainLabel(config: FollowerConfig, s: FollowerStatus | null): string | null {
  if (!config.chain_patch) return null;
  const fields = s?.own_chain ?? Object.keys(config.chain_patch);
  if (fields.length === 0) return "Own chain: same as the main chain";
  const names = fields.map((f) => CHAIN_FIELD[f] ?? f.replace(/_/g, " "));
  return `Own chain: ${names.join(", ")} ${names.length === 1 ? "differs" : "differ"}`;
}

function fmtMs(v: number, digits: number): string {
  return v.toFixed(digits);
}

/** +12.3 / −4.0 */
export function fmtPpm(ppm: number): string {
  const v = Math.round(ppm * 10) / 10;
  return (v > 0 ? "+" : v < 0 ? "−" : "±") + Math.abs(v).toFixed(1);
}

/** 1.5 → "+1.5 ms", −2 → "−2.0 ms". */
export function fmtTrim(ms: number): string {
  return `${ms > 0 ? "+" : ms < 0 ? "−" : ""}${Math.abs(ms).toFixed(1)} ms`;
}

/** The speed-cap note, when followers are configured and varispeed is on. */
export function speedCapNote(followers: number, varispeedOn: boolean): string | null {
  if (followers === 0) return null;
  return `Varispeed tops out at ${FOLLOWED_MAX_SPEED}× while other devices play${varispeedOn ? " (it is on now)" : ""}: each one corrects its clock on top of the speed.`;
}
