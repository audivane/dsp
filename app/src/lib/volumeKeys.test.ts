import { describe, expect, it } from "vitest";
import { keyPatch } from "./volumeKeys";

const L = (volume_db: number, muted = false) => ({ volume_db, balance_db: 0, muted });
const VS = { min_db: -80, max_db: 0 };

describe("volume keys", () => {

  it("stops at the ceiling and at the bottom, and writes nothing there", () => {
    const capped = { min_db: -80, max_db: -6 };
    expect(keyPatch(L(-6.5), capped, "up", 1)).toEqual({ volume_db: -6 });
    expect(keyPatch(L(-6), capped, "up", 1)).toBeNull();
    expect(keyPatch(L(-80), VS, "down", 1)).toBeNull();
    expect(keyPatch(L(-79.6), VS, "down", 1)).toEqual({ volume_db: -80 });
  });

  it("never steps against the key from a level outside the range", () => {
    // An engine that reports a narrower range than the level: up does not pull it down.
    expect(keyPatch(L(-3), { min_db: -80, max_db: -6 }, "up", 1)).toBeNull();
    expect(keyPatch(L(-3), { min_db: -80, max_db: -6 }, "down", 1)).toEqual({ volume_db: -6 });
  });

});
