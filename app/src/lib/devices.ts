/**
 * Device names as the pickers show them, and the "which DAC is which"
 * choice when identical DACs moved USB ports (`status.unresolved_devices`).
 */
import type { DeviceAssignment, DeviceInfo, UnresolvedDevice } from "../api/types";

type Named = Pick<DeviceInfo, "name" | "id" | "suffix">;

/** "USB Audio (USB port 3)": the name, with what tells it apart from another card of its model. */
export function deviceName(d: Named): string {
  return d.suffix ? `${d.name} (${d.suffix})` : d.name;
}

/** "USB Audio (USB port 3) · hw:USB,0". */
export function deviceLabel(d: Named): string {
  return `${deviceName(d)} · ${d.id.replace(/^hw:CARD=([^,]+),DEV=(\d+)$/, "hw:$1,$2")}`;
}

/** A device id's name from the list, else the id. */
export function nameOf(devices: readonly DeviceInfo[], id: string): string {
  const d = devices.find((x) => x.id === id);
  return d ? deviceName(d) : id;
}

/** "Main output" or the follower's name. */
export function heldOutputName(u: UnresolvedDevice, devices: readonly DeviceInfo[]): string {
  return u.output === "master" ? "Main output" : `“Also play on” ${nameOf(devices, u.device)}`;
}

/** Where it was: "USB port 1.4.3". */
export function heldWas(u: UnresolvedDevice): string {
  const port = u.identity.port;
  if (!port) return u.identity.card;
  const chain = port.includes("-usb-0:") ? port.slice(port.lastIndexOf("-usb-0:") + 7) : port;
  return `USB port ${chain}`;
}

/** A first pick per held output: its own device when it is a candidate, else the first candidate nobody else picked. */
export function initialPicks(held: readonly UnresolvedDevice[]): string[] {
  const picks: string[] = [];
  for (const u of held) {
    const own = u.candidates.includes(u.device) && !picks.includes(u.device) ? u.device : undefined;
    picks.push(own ?? u.candidates.find((c) => !picks.includes(c)) ?? u.candidates[0] ?? "");
  }
  return picks;
}

/** The `assign_devices` request, or why it cannot be sent yet. */
export function assignments(held: readonly UnresolvedDevice[], picks: readonly string[]): DeviceAssignment[] | string {
  if (picks.some((p) => !p)) return "Pick a device for each output.";
  if (new Set(picks).size !== picks.length) return "Each output needs its own DAC.";
  return held.map((u, i) => ({ output: u.output, device: picks[i] }));
}
