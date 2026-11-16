import { describe, expect, it } from "vitest";
import { compareVersions, versionSkew } from "./versions";

describe("compareVersions", () => {
  it("orders like semver", () => {
    expect(compareVersions("0.9.2", "0.9.1")).toBe(1);
    expect(compareVersions("0.9.1", "0.10.0")).toBe(-1);
    expect(compareVersions("0.9.1", "v0.9.1")).toBe(0);
    expect(compareVersions("0.9", "0.9.0")).toBe(0);
    expect(compareVersions("0.10.0-rc.1", "0.10.0")).toBe(-1);
    expect(compareVersions("0.10.0-rc.10", "0.10.0-rc.9")).toBe(1);
    expect(compareVersions("0.10.0-beta", "0.10.0-rc")).toBe(-1);
    expect(compareVersions("0.9.1+abc", "0.9.1")).toBe(0);
  });

  it("is null for something that is not a version", () => {
    expect(compareVersions("0.1.0 (mock)", "0.1.0")).toBeNull();
    expect(compareVersions("", "0.1.0")).toBeNull();
  });
});

describe("versionSkew", () => {
  it("tells which side is old", () => {
    expect(versionSkew("0.9.1", "0.9.1")).toBeNull();
    expect(versionSkew("0.9.1", "0.9.2")).toEqual({ kind: "app-outdated", engine: "0.9.2", app: "0.9.1" });
    expect(versionSkew("0.9.2", "0.9.1")).toEqual({ kind: "engine-outdated", engine: "0.9.1", app: "0.9.2" });
  });

  it("says nothing while a version is unknown", () => {
    expect(versionSkew(null, "0.9.2")).toBeNull();
    expect(versionSkew("0.9.1", undefined)).toBeNull();
    expect(versionSkew("0.9.1", "dev build")).toBeNull();
  });
});
