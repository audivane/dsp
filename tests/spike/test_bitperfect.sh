#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

TMPDIR="$(mktemp -d)"
PIDS=()

cleanup() {
  local rc=$?
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  rm -rf "$TMPDIR"
  exit "$rc"
}
trap cleanup EXIT INT TERM

echo "Building driver_spike (fastrel)..."
CARGO_BUILD_JOBS=12 cargo build --manifest-path "$ROOT_DIR/Cargo.toml" \
  --profile fastrel -p audivane-io-pw --example driver_spike

BIN="$ROOT_DIR/target/fastrel/examples/driver_spike"

echo "Generating 3s 48kHz 24-bit white-noise test WAV..."
python3 "$SCRIPT_DIR/analyze_bitperfect.py" gen "$TMPDIR/noise24.wav"

echo "Starting driver_spike (48k x1 -> 48k, S32_LE, period=1024)..."
"$BIN" --device hw:Loopback,0,0 --rate 48000 --ratio 1 --format s32 --period 1024 --seconds 7 \
  > "$TMPDIR/spike.log" 2>&1 &
SPIKE_PID=$!
PIDS+=("$SPIKE_PID")

SINK_ID=""
for _ in $(seq 1 50); do
  SINK_ID="$(pw-dump | python3 -c '
import json, sys
for obj in json.load(sys.stdin):
    p = obj.get("info", {}).get("props", {})
    if p.get("node.name") == "audivane_dsp":
        print(obj["id"])
        break
' 2>/dev/null || true)"
  if [[ -n "$SINK_ID" ]]; then
    break
  fi
  sleep 0.1
done
if [[ -z "$SINK_ID" ]]; then
  echo "ERROR: audivane_dsp node did not appear in PipeWire" >&2
  cat "$TMPDIR/spike.log" >&2 || true
  exit 1
fi
wpctl set-volume "$SINK_ID" 1.0
wpctl set-mute "$SINK_ID" 0

echo "Starting 48kHz S32_LE loopback capture..."
arecord -D hw:Loopback,1,0 -f S32_LE -r 48000 -c 2 -t wav -d 5 "$TMPDIR/capture.wav" \
  > "$TMPDIR/arecord.log" 2>&1 &
REC_PID=$!
PIDS+=("$REC_PID")

sleep 0.5
echo "Playing 24-bit white noise via pw-play (--volume=1.0)..."
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/noise24.wav" \
  > "$TMPDIR/pwplay.log" 2>&1 &
PLAY_PID=$!
PIDS+=("$PLAY_PID")

wait "$PLAY_PID"
wait "$REC_PID"
wait "$SPIKE_PID"

python3 "$SCRIPT_DIR/analyze_bitperfect.py" verify "$TMPDIR/noise24.wav" "$TMPDIR/capture.wav"
