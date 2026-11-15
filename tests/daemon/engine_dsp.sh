#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

TMPDIR="$(mktemp -d)"
PIDS=()

ORIG_ALLOWED_RATES="$(pw-metadata -n settings | awk -F"value:'" '/clock\.allowed-rates/{split($2,a,"'\''"); print a[1]}')"
ORIG_FORCE_RATE="$(pw-metadata -n settings | awk -F"value:'" '/clock\.force-rate/{split($2,a,"'\''"); print a[1]}')"

cleanup() {
  local rc=$?
  if [[ $rc -ne 0 && -f "$TMPDIR/rig.out" ]]; then
    echo '--- last engine snapshot (gap counters) ---' >&2
    grep '"type":"snapshot"' "$TMPDIR/rig.out" | tail -n 1 >&2 || true
  fi
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  if [[ -n "$ORIG_ALLOWED_RATES" ]]; then
    pw-metadata -n settings 0 clock.allowed-rates "$ORIG_ALLOWED_RATES" >/dev/null 2>&1 || true
  fi
  if [[ -n "$ORIG_FORCE_RATE" ]]; then
    pw-metadata -n settings 0 clock.force-rate "$ORIG_FORCE_RATE" >/dev/null 2>&1 || true
  fi
  rm -rf "$TMPDIR"
  exit "$rc"
}
trap cleanup EXIT INT TERM

echo "Building engine_rig (fastrel)..."
CARGO_BUILD_JOBS=16 cargo build --manifest-path "$ROOT_DIR/Cargo.toml" \
  --profile fastrel -p audivane-dsp-daemon --example engine_rig
BIN="$ROOT_DIR/target/fastrel/examples/engine_rig"

# EQ: preamp -4.0 dB + +3.0 dB peaking @ 1 kHz (net -1.0 dB at 1 kHz).
# Input tone: -6.0 dBFS 1 kHz -> expected output: -7.00 dBFS.
# Real 4x 65536-tap linear-phase upsampler (48000 -> 192000 Hz).
cat > "$TMPDIR/cfg.json" <<'EOF'
{
  "device": "hw:Loopback,0,0",
  "dsp_enabled": true,
  "chain": {
    "eq": {
      "name": "test_eq",
      "preamp_db": -4.0,
      "bands": [
        { "kind": "peaking", "freq_hz": 1000.0, "gain_db": 3.0, "q": 1.0, "enabled": true }
      ]
    },
    "upsampler": {
      "target": { "ratio": 4 },
      "taps": 65536,
      "phase": "linear",
      "kaiser_beta": 14.0,
      "cutoff": 1.0
    }
  },
  "rate": {
    "follow_source": false,
    "allowed_rates": [48000],
    "default_rate": 48000
  },
  "quantum": 1024
}
EOF

echo "Generating 60s 48kHz 24-bit 1kHz -6.0dBFS sine..."
sox -n -r 48000 -b 24 -c 2 "$TMPDIR/tone60s.wav" synth 60 sine 1000 gain -6.0

echo "Starting engine_rig..."
"$BIN" "$TMPDIR/cfg.json" --interval-ms 1000 < /dev/null > "$TMPDIR/rig.out" 2> "$TMPDIR/rig.log" &
RIG_PID=$!
PIDS+=("$RIG_PID")

SINK_ID=""
for _ in $(seq 1 60); do
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
  echo "ERROR: audivane_dsp did not appear" >&2
  cat "$TMPDIR/rig.log" >&2 || true
  exit 1
fi
wpctl set-volume "$SINK_ID" 1.0
wpctl set-mute "$SINK_ID" 0

echo "Starting 192kHz S32_LE loopback capture (62s)..."
arecord -D hw:Loopback,1,0 -f S32_LE -r 192000 -c 2 -t wav -d 62 "$TMPDIR/cap192k.wav" \
  > "$TMPDIR/arecord.log" 2>&1 &
REC_PID=$!
PIDS+=("$REC_PID")

sleep 0.6
echo "Playing 60s tone into audivane_dsp..."
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/tone60s.wav" > "$TMPDIR/pwplay.log" 2>&1 &
PLAY_PID=$!
PIDS+=("$PLAY_PID")

sleep 15
pw-top -b -n 2 > "$TMPDIR/pwtop.txt" 2>&1 || true

wait "$PLAY_PID"
wait "$REC_PID"
kill "$RIG_PID" 2>/dev/null || true
wait "$RIG_PID" 2>/dev/null || true

python3 "$SCRIPT_DIR/analyze_rig.py" check_dsp_60s \
  "$TMPDIR/rig.out" "$TMPDIR/cap192k.wav" "$TMPDIR/pwtop.txt" -7.0
