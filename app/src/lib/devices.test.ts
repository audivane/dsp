import { describe, expect, it } from "vitest";
import type { DeviceInfo, UnresolvedDevice } from "../api/types";
import { assignments, deviceLabel, deviceName, heldOutputName, heldWas, initialPicks, nameOf } from "./devices";

const dev = (card: string, suffix?: string): DeviceInfo => ({ id: `hw:CARD=${card},DEV=0`, card, name: "USB Audio", hdmi: false, loopback: false, active: false, channels: [], suffix });
const held = (output: string, device: string, chain: string): UnresolvedDevice => ({
  output,
  device,
  identity: { card: "USB", usb_id: "2522:0007", port: `pci-0000:00:14.0-usb-0:${chain}` },
  candidates: ["hw:CARD=USB,DEV=0", "hw:CARD=USB_1,DEV=0"],
});

describe("device names", () => {
  it("adds the suffix that tells identical cards apart", () => {
    expect(deviceName(dev("USB", "USB port 3"))).toBe("USB Audio (USB port 3)");
    expect(deviceName(dev("PCH"))).toBe("USB Audio");
    expect(deviceLabel(dev("USB_1", "USB port 1.4"))).toBe("USB Audio (USB port 1.4) · hw:USB_1,0");
    expect(nameOf([dev("USB", "serial A1")], "hw:CARD=USB,DEV=0")).toBe("USB Audio (serial A1)");
    expect(nameOf([], "hw:CARD=X,DEV=0")).toBe("hw:CARD=X,DEV=0");
  });
});

describe("which DAC is which", () => {
  const list = [held("master", "hw:CARD=USB,DEV=0", "1.4.3"), held("hw:CARD=USB_1,DEV=0", "hw:CARD=USB_1,DEV=0", "1.4.1.4")];

  it("names the outputs and where they were", () => {
    expect(heldOutputName(list[0], [])).toBe("Main output");
    expect(heldOutputName(list[1], [dev("USB_1", "USB port 1.4")])).toBe("“Also play on” USB Audio (USB port 1.4)");
    expect(heldWas(list[0])).toBe("USB port 1.4.3");
  });

  it("starts from each output's own name and sends every output at once", () => {
    const picks = initialPicks(list);
    expect(picks).toEqual(["hw:CARD=USB,DEV=0", "hw:CARD=USB_1,DEV=0"]);
    expect(assignments(list, ["hw:CARD=USB_1,DEV=0", "hw:CARD=USB,DEV=0"])).toEqual([
      { output: "master", device: "hw:CARD=USB_1,DEV=0" },
      { output: "hw:CARD=USB_1,DEV=0", device: "hw:CARD=USB,DEV=0" },
    ]);
    expect(assignments(list, ["hw:CARD=USB,DEV=0", "hw:CARD=USB,DEV=0"])).toBe("Each output needs its own DAC.");
    expect(assignments(list, ["", "hw:CARD=USB,DEV=0"])).toBe("Pick a device for each output.");
  });
});
