// App vs engine version: after a package upgrade the running app (old code)
// may talk to a restarted, newer engine, or a restarted app to an engine
// that is still the old one.

/** Semver-style compare of "X.Y.Z[-pre]" (missing parts are 0); null when either does not parse. */
export function compareVersions(a: string, b: string): number | null {
  const pa = parse(a);
  const pb = parse(b);
  if (!pa || !pb) return null;
  const len = Math.max(pa.nums.length, pb.nums.length);
  for (let i = 0; i < len; i++) {
    const d = (pa.nums[i] ?? 0) - (pb.nums[i] ?? 0);
    if (d !== 0) return Math.sign(d);
  }
  if (!pa.pre.length || !pb.pre.length) return pa.pre.length === pb.pre.length ? 0 : pa.pre.length ? -1 : 1;
  for (let i = 0; i < Math.min(pa.pre.length, pb.pre.length); i++) {
    const x = pa.pre[i];
    const y = pb.pre[i];
    const nx = /^\d+$/.test(x);
    const ny = /^\d+$/.test(y);
    const d = nx && ny ? Number(x) - Number(y) : nx ? -1 : ny ? 1 : x < y ? -1 : x > y ? 1 : 0;
    if (d !== 0) return Math.sign(d);
  }
  return Math.sign(pa.pre.length - pb.pre.length);
}

function parse(v: string): { nums: number[]; pre: string[] } | null {
  const m = /^v?(\d+(?:\.\d+){0,3})(?:-([0-9A-Za-z.-]+))?(?:\+.*)?$/.exec(v.trim());
  if (!m) return null;
  return { nums: m[1].split(".").map(Number), pre: m[2] ? m[2].split(".") : [] };
}

export type VersionSkew =
  /** The engine is newer: this app is the old code and should restart. */
  | { kind: "app-outdated"; engine: string; app: string }
  /** The app is newer: the engine service still runs the old version. */
  | { kind: "engine-outdated"; engine: string; app: string };

/** How the app and the engine differ, or null when they match (or a version is unknown). */
export function versionSkew(app: string | null, engine: string | null | undefined): VersionSkew | null {
  if (!app || !engine) return null;
  const c = compareVersions(engine, app);
  if (c === null || c === 0) return null;
  return { kind: c > 0 ? "app-outdated" : "engine-outdated", engine, app };
}
