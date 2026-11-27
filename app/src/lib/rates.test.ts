import { describe, expect, it } from "vitest";
import { toggleRate } from "./rates";

describe("toggleRate", () => {

  it("never blocks the last rate", () => {
    expect(toggleRate({ allowed_rates: [48000], default_rate: 48000 }, 48000)).toBeNull();
  });

});

