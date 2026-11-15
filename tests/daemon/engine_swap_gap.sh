#!/usr/bin/env bash
set -euo pipefail

# Regression test for Finding #6:
# 65,536-tap 4x linear-phase upsampler + EQ, swap EQ preamp gain 5 times mid-playback.
# Capture must show ZERO gaps (no block dropping below expected level envelope) and
# every gain step must land cleanly on target level with 0 xruns.

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

make_cfg() {
  local preamp="$1"
  printf '{"device":"hw:Loopback,0,0","dsp_enabled":true,"chain":{"eq":{"name":"swap","preamp_db":%s,"bands":[{"kind":"peaking","freq_hz":1000.0,"gain_db":0.0,"q":1.0,"enabled":true}]},"upsampler":{"target":{"ratio":4},"taps":65536,"phase":"linear","kaiser_beta":14.0,"cutoff":1.0}},"rate":{"follow_source":false,"allowed_rates":[48000],"default_rate":48000},"quantum":1024}' "$preamp"
}

make_cfg "-6.0" > "$TMPDIR/cfg.json"
sox -n -r 48000 -b 24 -c 2 "$TMPDIR/tone15s.wav" synth 15 sine 1000 gain -6.0

mkfifo "$TMPDIR/rig.in"
exec 3<> "$TMPDIR/rig.in"
"$BIN" "$TMPDIR/cfg.json" --interval-ms 250 < "$TMPDIR/rig.in" > "$TMPDIR/rig.out" 2> "$TMPDIR/rig.log" &
RIG_PID=$!; PIDS+=("$RIG_PID")

SINK_ID=""
for _ in $(seq 1 50); do
  SINK_ID="$(pw-dump | python3 -c '
import json, sys
for obj in json.load(sys.stdin):
    if obj.get("info", {}).get("props", {}).get("node.name") == "audivane_dsp":
        print(obj["id"]); break
' 2>/dev/null || true)"
  if [[ -n "$SINK_ID" ]]; then
    break
  fi
  sleep 0.1
done
wpctl set-volume "$SINK_ID" 1.0
wpctl set-mute "$SINK_ID" 0

echo "Capturing 16s 192kHz while applying 5 EQ gain swaps mid-playback on 65536-tap 4x chain..."
arecord -D hw:Loopback,1,0 -f S32_LE -r 192000 -c 2 -t wav -d 16 "$TMPDIR/cap_swap.wav" \
  > "$TMPDIR/arecord.log" 2>&1 &
REC_PID=$!; PIDS+=("$REC_PID")

sleep 0.4
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/tone15s.wav" > "$TMPDIR/play.log" 2>&1 &
PLAY_PID=$!; PIDS+=("$PLAY_PID")

# Input = -6 dBFS; steps: -6 -> -3 -> -8 -> -2 -> -7 -> -4 dB preamp
# Expected output levels: -12, -9, -14, -8, -13, -10 dBFS.
sleep 2.2
echo "apply $(make_cfg -3.0)" >&3
sleep 2.2
echo "apply $(make_cfg -8.0)" >&3
sleep 2.2
echo "apply $(make_cfg -2.0)" >&3
sleep 2.2
echo "apply $(make_cfg -7.0)" >&3
sleep 2.2
echo "apply $(make_cfg -4.0)" >&3

wait "$PLAY_PID"
wait "$REC_PID"
echo "quit" >&3
wait "$RIG_PID" || true
exec 3>&-

python3 "$SCRIPT_DIR/analyze_rig.py" check_swap_steps \
  "$TMPDIR/rig.out" "$TMPDIR/cap_swap.wav" 192000 "-12,-9,-14,-8,-13,-10"
