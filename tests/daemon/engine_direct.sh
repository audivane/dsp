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

cat > "$TMPDIR/cfg.json" <<'EOF'
{
  "device": "hw:Loopback,0,0",
  "dsp_enabled": true,
  "chain": {
    "upsampler": { "target": { "ratio": 4 } }
  },
  "rate": {
    "follow_source": true,
    "allowed_rates": [44100, 48000, 88200, 96000, 176400, 192000],
    "default_rate": 48000
  }
}
EOF

mkfifo "$TMPDIR/rig.in"
exec 3<> "$TMPDIR/rig.in"
"$BIN" "$TMPDIR/cfg.json" --interval-ms 250 < "$TMPDIR/rig.in" > "$TMPDIR/rig.out" 2> "$TMPDIR/rig.log" &
RIG_PID=$!
PIDS+=("$RIG_PID")

# Wait for PipeWire sinks audivane_dsp & audivane_direct.
DIRECT_ID=""
for _ in $(seq 1 50); do
  DIRECT_ID="$(pw-dump | python3 -c '
import json, sys
for obj in json.load(sys.stdin):
    p = obj.get("info", {}).get("props", {})
    if p.get("node.name") == "audivane_direct":
        print(obj["id"])
        break
' 2>/dev/null || true)"
  if [[ -n "$DIRECT_ID" ]]; then
    break
  fi
  sleep 0.1
done
if [[ -z "$DIRECT_ID" ]]; then
  echo "ERROR: audivane_direct did not appear in PipeWire" >&2
  cat "$TMPDIR/rig.log" >&2 || true
  exit 1
fi
wpctl set-volume "$DIRECT_ID" 1.0
wpctl set-mute "$DIRECT_ID" 0

wait_rate_ratio() {
  local want_rate="$1"
  local want_ratio="$2"
  for _ in $(seq 1 60); do
    if python3 -c '
import json, sys
want_r, want_rat = int(sys.argv[2]), int(sys.argv[3])
snaps = [json.loads(l) for l in open(sys.argv[1]) if l.strip().startswith("{")]
snaps = [s for s in snaps if s.get("type") == "snapshot"]
if snaps and snaps[-1]["graph_rate"] == want_r and snaps[-1]["output_rate"] == want_r * want_rat and snaps[-1]["ratio"] == want_rat and snaps[-1]["state"] != "switching":
    sys.exit(0)
sys.exit(1)
' "$TMPDIR/rig.out" "$want_rate" "$want_ratio" 2>/dev/null; then
      return 0
    fi
    sleep 0.1
  done
  echo "ERROR: timed out waiting for graph_rate=$want_rate ratio=$want_ratio" >&2
  tail -n 20 "$TMPDIR/rig.out" >&2 || true
  cat "$TMPDIR/rig.log" >&2 || true
  exit 1
}

echo "Generating 44.1k & 48k 24-bit test WAVs..."
python3 "$SCRIPT_DIR/analyze_rig.py" gen_bitperfect "$TMPDIR/noise44.wav" 44100 3.0
python3 "$SCRIPT_DIR/analyze_rig.py" gen_bitperfect "$TMPDIR/noise48.wav" 48000 3.0
sox -n -r 44100 -b 24 -c 2 "$TMPDIR/warm44.wav" synth 1.2 sine 1000 gain -24
sox -n -r 48000 -b 24 -c 2 "$TMPDIR/warm48.wav" synth 1.2 sine 1000 gain -24

echo "=== Subtest 1: Direct sink rate switch to 44100 Hz (ratio=1) + bit-perfect ==="
pw-play --target audivane_direct --volume=1.0 "$TMPDIR/warm44.wav" > "$TMPDIR/play_w44.log" 2>&1 &
W44_PID=$!
PIDS+=("$W44_PID")
wait_rate_ratio 44100 1
wait "$W44_PID" || true
sleep 0.3

arecord -D hw:Loopback,1,0 -f S32_LE -r 44100 -c 2 -t wav -d 4 "$TMPDIR/cap44.wav" \
  > "$TMPDIR/arec44.log" 2>&1 &
REC44=$!
PIDS+=("$REC44")
sleep 0.3
pw-play --target audivane_direct --volume=1.0 "$TMPDIR/noise44.wav" > "$TMPDIR/play44.log" 2>&1 &
P44=$!
PIDS+=("$P44")
wait "$P44"
wait "$REC44"
python3 "$SCRIPT_DIR/analyze_rig.py" verify_bitperfect "$TMPDIR/noise44.wav" "$TMPDIR/cap44.wav"

echo "=== Subtest 2: Direct sink rate switch 44100 -> 48000 Hz (ratio=1) + bit-perfect ==="
pw-play --target audivane_direct --volume=1.0 "$TMPDIR/warm48.wav" > "$TMPDIR/play_w48.log" 2>&1 &
W48_PID=$!
PIDS+=("$W48_PID")
wait_rate_ratio 48000 1
wait "$W48_PID" || true
sleep 0.3

arecord -D hw:Loopback,1,0 -f S32_LE -r 48000 -c 2 -t wav -d 4 "$TMPDIR/cap48.wav" \
  > "$TMPDIR/arec48.log" 2>&1 &
REC48=$!
PIDS+=("$REC48")
sleep 0.3
pw-play --target audivane_direct --volume=1.0 "$TMPDIR/noise48.wav" > "$TMPDIR/play48.log" 2>&1 &
P48=$!
PIDS+=("$P48")
wait "$P48"
wait "$REC48"
python3 "$SCRIPT_DIR/analyze_rig.py" verify_bitperfect "$TMPDIR/noise48.wav" "$TMPDIR/cap48.wav"

echo "quit" >&3
wait "$RIG_PID" || true
exec 3>&-

# Verify zero xruns across entire Direct test
python3 -c '
import json, sys
snaps = [json.loads(l) for l in open(sys.argv[1]) if l.strip().startswith("{")]
snaps = [s for s in snaps if s.get("type") == "snapshot"]
xruns = max(s["xruns"] for s in snaps)
assert xruns == 0, f"Expected 0 xruns, got {xruns}"
print(f"PASS: engine_direct complete (snapshots={len(snaps)}, xruns={xruns})")
' "$TMPDIR/rig.out"
