import type { SinkChoice } from "../api/types";

export const SINK_LABEL: Record<SinkChoice, string> = { dsp: "Audivane DSP", direct: "Audivane Direct (bit-perfect)", default: "Not through Audivane" };

/**
 * The choices a stream can be sent to. In system-wide routing the session
 * default *is* Audivane DSP, so "default" would only send the stream back to
 * DSP: offer it only in per-app routing, named after the output it reaches.
 */
export function sinkOptions(apps: boolean, normalOutput: string | null | undefined): [SinkChoice, string][] {
  const options: [SinkChoice, string][] = [
    ["dsp", SINK_LABEL.dsp],
    ["direct", SINK_LABEL.direct],
  ];
  if (apps) options.push(["default", `${SINK_LABEL.default} (${normalOutput ?? "no other output"})`]);
  return options;
}
