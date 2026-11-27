import type { OutputChannels } from "../api/types";
import { pairKey, pairLabel, pairOptions, parsePairKey, showPairSelector } from "../lib/outputChannels";

const TITLE =
  "The device channels the left and right play on. This device has more than two outputs; the others play silence. Changing it reopens the device.";

/**
 * Which pair of a multichannel device's outputs plays the left and right.
 * Shown only for a device without a stereo mode (it opened at more than two
 * channels) or when a pair other than the first two is set.
 */
export function OutputPairPicker({
  id,
  label,
  opened,
  channels,
  pair,
  onChange,
}: {
  id: string;
  label: string;
  /** Channels the device opened at (0: not open). */
  opened: number;
  /** The most channels it takes (0: unknown). */
  channels: number;
  pair: OutputChannels | null | undefined;
  onChange: (p: OutputChannels) => void;
}) {
  if (!showPairSelector(opened, pair)) return null;
  const options = pairOptions(Math.max(channels, opened), pair);
  return (
    <div className="row">
      <label className="k" htmlFor={id} title={TITLE}>
        Output channels
      </label>
      <select id={id} className="chip" aria-label={label} title={TITLE} value={pairKey(pair)} onChange={(e) => onChange(parsePairKey(e.target.value))}>
        {options.map((p) => (
          <option key={pairKey(p)} value={pairKey(p)}>
            {pairLabel(p)}
          </option>
        ))}
      </select>
      {opened > 2 && <span className="hint">{opened} channels; the others play silence</span>}
    </div>
  );
}
