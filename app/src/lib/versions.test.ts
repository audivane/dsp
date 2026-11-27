import { describe, expect, it } from "vitest";
import { compareVersions } from "./versions";

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

});

