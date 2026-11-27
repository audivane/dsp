// "Follow the music" for the Harmonics page's test-sine graphs: the music's
// level (`music_level_dbfs`, its sine-equivalent level) and spectral centre
// (`centroid_hz`) from the live frames, smoothed over about a second, turned
// into slider values that change only when the music has moved enough, on a
// grid (so the engine's readout cache hits), at most about once a second.
// Nothing playing: the last values hold ("paused"). A blind test: nothing
// moves at all ("frozen"), since following would give the side away.

/** Smoothing time constant of the followed values, ms. */
export const FOLLOW_TAU_MS = 1000;
/** A gap longer than this between frames counts as at most this much time (no jump after a pause). */
const MAX_DT_MS = 500;
/** Level grid and the move that sends a new level, dB. */
export const LEVEL_STEP_DB = 1;
/** Tone grid and the move that sends a new tone, octaves. */
export const TONE_STEP_OCT = 1 / 6;
/** At most one new value per graph and slider this often, ms. */
export const FOLLOW_MIN_INTERVAL_MS = 1000;
/** Centres outside the sliders' range are clamped to it. */
export const TONE_MIN_HZ = 20;
export const TONE_MAX_HZ = 20000;

/** What a live frame tells the follower (a subset of `HarmonicsLive`). */
export interface FollowFrame {
  seq: number;
  state: string;
  music_level_dbfs: number;
  centroid_hz: number;
}

/** Why the followed values are where they are. */
export type FollowStatus = "following" | "paused" | "frozen" | "waiting" | "stage-off";

const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, v));

/** A level on the 1 dB grid within a slider's range. */
export const snapLevel = (db: number, min: number, max: number) => clamp(Math.round(db / LEVEL_STEP_DB) * LEVEL_STEP_DB, min, max);

/** A tone on the sixth-octave grid through 1 kHz (whole hertz), within 20 Hz–20 kHz. */
export function snapTone(hz: number): number {
  const n = Math.round(Math.log2(clamp(hz, TONE_MIN_HZ, TONE_MAX_HZ) / 1000) / TONE_STEP_OCT);
  return clamp(Math.round(1000 * Math.pow(2, n * TONE_STEP_OCT)), TONE_MIN_HZ, TONE_MAX_HZ);
}

/**
 * The music's level and centre, smoothed: an exponential average over
 * about a second of new frames (by `seq`), the centre in octaves. Frames
 * that are not music leave the values where they were.
 */
export class MusicSmoother {
  /** Smoothed level, dBFS; null until music has played. */
  level: number | null = null;
  /** Smoothed centre, log2 Hz; null until music with a centre has played. */
  private logTone: number | null = null;
  private lastT: number | null = null;
  private lastSeq = -1;
  /** The last frame seen was music. */
  playing = false;

  get tone(): number | null {
    return this.logTone === null ? null : Math.pow(2, this.logTone);
  }

  /** Feed a frame seen at `t` (ms); false when it changed nothing. */
  push(f: FollowFrame | null, t: number): boolean {
    if (!f) {
      const was = this.playing;
      this.playing = false;
      return was;
    }
    if (f.seq === this.lastSeq) return false;
    this.lastSeq = f.seq;
    if (f.state !== "playing" || !Number.isFinite(f.music_level_dbfs)) {
      const was = this.playing;
      this.playing = false;
      this.lastT = null;
      return was;
    }
    this.playing = true;
    const dt = this.lastT === null ? MAX_DT_MS : Math.min(MAX_DT_MS, Math.max(0, t - this.lastT));
    this.lastT = t;
    const a = 1 - Math.exp(-dt / FOLLOW_TAU_MS);
    this.level = this.level === null ? f.music_level_dbfs : this.level + a * (f.music_level_dbfs - this.level);
    if (f.centroid_hz > 0 && Number.isFinite(f.centroid_hz)) {
      const lt = Math.log2(clamp(f.centroid_hz, TONE_MIN_HZ, TONE_MAX_HZ));
      this.logTone = this.logTone === null ? lt : this.logTone + a * (lt - this.logTone);
    }
    return true;
  }
}

/**
 * One followed slider: sends a new grid value when the smoothed one has
 * moved at least a step from the value sent last, and not within a second
 * of the last one.
 */
export class FollowGate {
  sent: number | null = null;
  private at = -Infinity;
  constructor(
    /** The distance in steps between two values. */
    private readonly steps: (a: number, b: number) => number,
    private readonly snap: (v: number) => number,
  ) {}

  /** The value to send now for `target` at `t` (ms), or null to keep the current one. */
  next(target: number | null, t: number): number | null {
    if (target === null) return null;
    if (this.sent !== null && (this.steps(target, this.sent) < 1 || t - this.at < FOLLOW_MIN_INTERVAL_MS)) return null;
    const v = this.snap(target);
    if (v === this.sent) return null;
    this.sent = v;
    this.at = t;
    return v;
  }

  /** Forget the value sent (following turned off, or on again): the next target goes out at once. */
  reset(): void {
    this.sent = null;
    this.at = -Infinity;
  }
}

export const levelGate = (min: number, max: number) => new FollowGate((a, b) => Math.abs(a - b) / LEVEL_STEP_DB, (v) => snapLevel(v, min, max));
export const toneGate = () => new FollowGate((a, b) => Math.abs(Math.log2(a / b)) / TONE_STEP_OCT, snapTone);

/** The status a graph shows while it follows. */
export function followStatus(s: { playing: boolean; level: number | null }, blind: boolean, stageOff = false): FollowStatus {
  if (blind) return "frozen";
  // The live feed carries no music while the stage is off.
  if (stageOff) return "stage-off";
  if (s.level === null) return "waiting";
  return s.playing ? "following" : "paused";
}

export const FOLLOW_STATUS_TEXT: Record<FollowStatus, string> = {
  following: "following",
  paused: "following: paused",
  frozen: "following: frozen (blind test)",
  waiting: "following: waiting for music",
  "stage-off": "following: paused (the stage is off)",
};

export const FOLLOW_TITLE =
  "The nearest sine to what is playing: its level (the music's RMS + 3 dB, a sine of the same power) and its spectral centre. Music is not a sine, so the graph is a guide, not a measurement of the music. Smoothed over about a second; a new request at most once a second, after a 1 dB or a sixth-octave move.";
