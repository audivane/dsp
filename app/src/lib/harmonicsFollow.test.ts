import { describe, expect, it } from "vitest";
import { FOLLOW_MIN_INTERVAL_MS, MusicSmoother, followStatus, levelGate, snapLevel, snapTone, toneGate, type FollowFrame } from "./harmonicsFollow";
import { parseHarmGraph } from "./graphPrefs";

const frame = (seq: number, level: number, centre: number, state = "playing"): FollowFrame => ({ seq, state, music_level_dbfs: level, centroid_hz: centre });

describe("snapping", () => {
  it("puts levels on the 1 dB grid within the slider", () => {
    expect(snapLevel(-14.4, -60, 0)).toBe(-14);
    expect(snapLevel(-14.6, -60, 0)).toBe(-15);
    expect(snapLevel(-70, -40, 0)).toBe(-40);
    expect(snapLevel(2, -40, 0)).toBe(0);
  });
  it("puts tones on a sixth-octave grid through 1 kHz, so nearby centres share one", () => {
    expect(snapTone(1000)).toBe(1000);
    expect(snapTone(1050)).toBe(1000);
    expect(snapTone(1100)).toBe(1122);
    expect(snapTone(306)).toBe(315);
    expect(snapTone(300)).toBe(snapTone(310));
    expect(snapTone(5)).toBe(20);
    expect(snapTone(30000)).toBe(20000);
  });
});

describe("smoothing", () => {
  it("starts at the first frame and moves toward the music over about a second", () => {
    const s = new MusicSmoother();
    expect(s.push(frame(1, -20, 1000), 0)).toBe(true);
    expect(s.level).toBe(-20);
    expect(s.tone).toBeCloseTo(1000, 6);
    // A step to −10 dBFS: after 250 ms about 22 % of the way, after a second about 63 %.
    s.push(frame(2, -10, 4000), 250);
    expect(s.level!).toBeGreaterThan(-18.5);
    expect(s.level!).toBeLessThan(-17.5);
    for (let i = 3; i <= 5; i++) s.push(frame(i, -10, 4000), 250 * (i - 1));
    expect(s.level!).toBeCloseTo(-10 - 10 * Math.exp(-1), 1);
    // The centre is smoothed in octaves: halfway in log is 2 kHz, not 2.5 kHz.
    expect(Math.log2(s.tone! / 1000)).toBeCloseTo(2 * (1 - Math.exp(-1)), 1);
  });
  it("ignores repeated frames and frames without a centre leave the tone", () => {
    const s = new MusicSmoother();
    s.push(frame(1, -20, 500), 0);
    expect(s.push(frame(1, -5, 500), 250)).toBe(false);
    expect(s.level).toBe(-20);
    s.push(frame(2, -20, 0), 500);
    expect(s.tone).toBeCloseTo(500, 6);
  });
  it("holds the values while nothing plays, and does not jump when music resumes", () => {
    const s = new MusicSmoother();
    s.push(frame(1, -20, 1000), 0);
    s.push(frame(2, -107, 0, "silent"), 250);
    expect(s.playing).toBe(false);
    expect(s.level).toBe(-20);
    s.push(frame(3, 0, 1000), 60_000);
    // At most half a second's worth of movement on the first frame back.
    expect(s.level!).toBeLessThan(-20 * Math.exp(-0.5) + 0.01);
    expect(s.playing).toBe(true);
  });
});

describe("gates", () => {
  it("sends the first value at once, then only after a step and a second", () => {
    const g = levelGate(-60, 0);
    expect(g.next(-14.4, 0)).toBe(-14);
    expect(g.next(-14.9, 2000)).toBeNull(); // under a dB from −14
    expect(g.next(-15.2, 500)).toBeNull(); // a dB, but within the second
    expect(g.next(-15.2, FOLLOW_MIN_INTERVAL_MS)).toBe(-15);
    expect(g.next(null, 5000)).toBeNull();
  });
  it("does not flicker between two grid points", () => {
    const g = levelGate(-60, 0);
    g.next(-14.5, 0);
    let sent = 0;
    for (let t = 1000; t < 20_000; t += 250) if (g.next(-14.5 + (t % 500 ? 0.4 : -0.4), t) !== null) sent++;
    expect(sent).toBe(0);
  });
  it("moves the tone on sixth-octave steps", () => {
    const g = toneGate();
    expect(g.next(306, 0)).toBe(315);
    expect(g.next(330, 2000)).toBeNull();
    expect(g.next(360, 3000)).toBe(354);
  });
  it("starts over after a reset", () => {
    const g = levelGate(-40, 0);
    g.next(-10, 0);
    g.reset();
    expect(g.next(-10.2, 10)).toBe(-10);
  });
});

describe("status", () => {
  it("waits, follows, pauses on silence and freezes in a blind test", () => {
    const s = new MusicSmoother();
    expect(followStatus(s, false)).toBe("waiting");
    s.push(frame(1, -20, 1000), 0);
    expect(followStatus(s, false)).toBe("following");
    s.push(frame(2, -107, 0, "idle"), 250);
    expect(followStatus(s, false)).toBe("paused");
    s.push(frame(3, -20, 1000), 500);
    expect(followStatus(s, true)).toBe("frozen");
    expect(followStatus(s, false, true)).toBe("stage-off");
  });
  it("a blind test feeds nothing: the values stay where they were", () => {
    // The page passes no frame and skips the gates while a blind test runs.
    const s = new MusicSmoother();
    s.push(frame(1, -20, 1000), 0);
    const before = { level: s.level, tone: s.tone };
    s.push(null, 250);
    expect({ level: s.level, tone: s.tone }).toEqual(before);
    expect(followStatus(s, true)).toBe("frozen");
  });
});

describe("prefs", () => {
  it("remembers what follows", () => {
    expect(parseHarmGraph(null).followSpec).toBe("off");
    const p = parseHarmGraph(JSON.stringify({ followSpec: "both", followLevel: true, followFreq: "yes" }));
    expect(p.followSpec).toBe("both");
    expect(p.followLevel).toBe(true);
    expect(p.followFreq).toBe(false);
    expect(parseHarmGraph(JSON.stringify({ followSpec: "tone" })).followSpec).toBe("off");
  });
});
