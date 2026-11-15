#!/usr/bin/env bash
set -euo pipefail

# Regression test for Finding #2:
# MaxRate(192000) with follow_source=true must resolve ratio against the actual
# PipeWire graph rate on every source switch:
#   44100 Hz -> 4x (176400 Hz)
#   48000 Hz -> 4x (192000 Hz)
#   96000 Hz -> 2x (192000 Hz, NOT 4x = 384000 Hz!)

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
    "eq": { "name": "trim", "preamp_db": -1.0, "bands": [] },
    "upsampler": { "target": { "max_rate": 192000 }, "taps": 8192, "phase": "linear", "kaiser_beta": 14.0, "cutoff": 1.0 }
  },
  "rate": {
    "follow_source": true,
    "allowed_rates": [44100, 48000, 88200, 96000, 176400, 192000],
    "default_rate": 48000
  },
  "quantum": 1024
}
EOF

sox -n -r 44100 -b 24 -c 2 "$TMPDIR/t44.wav" synth 3 sine 1000 gain -6.0
sox -n -r 48000 -b 24 -c 2 "$TMPDIR/t48.wav" synth 3 sine 1000 gain -6.0
sox -n -r 96000 -b 24 -c 2 "$TMPDIR/t96.wav" synth 3 sine 1000 gain -6.0

mkfifo "$TMPDIR/rig.in"
exec 3<> "$TMPDIR/rig.in"
"$BIN" "$TMPDIR/cfg.json" --interval-ms 200 < "$TMPDIR/rig.in" > "$TMPDIR/rig.out" 2> "$TMPDIR/rig.log" &
RIG_PID=$!
PIDS+=("$RIG_PID")

SINK_ID=""
for _ in $(seq 1 50); do
  SINK_ID="$(pw-dump | python3 -c '
import json, sys
for obj in json.load(sys.stdin):
    p = obj.get("info", {}).get("props", {})
    if p.get("node.name") == "audivane_dsp":
        print(obj["id"]); break
' 2>/dev/null || true)"
  if [[ -n "$SINK_ID" ]]; then
    break
  fi
  sleep 0.1
done
if [[ -z "$SINK_ID" ]]; then
  echo "ERROR: audivane_dsp did not appear" >&2
  exit 1
fi
wpctl set-volume "$SINK_ID" 1.0
wpctl set-mute "$SINK_ID" 0

wait_rate_ratio() {
  local want_rate="$1"
  local want_ratio="$2"
  for _ in $(seq 1 60); do
    if python3 -c '
import json, sys
want_r, want_rat = int(sys.argv[2]), int(sys.argv[3])
snaps = [json.loads(l) for l in open(sys.argv[1]) if l.strip().startswith("{")]
snaps = [s for s in snaps if s.get("type") == "snapshot"]
if snaps:
    s = snaps[-1]
    if s["state"] == "playing" and s["graph_rate"] == want_r and s["ratio"] == want_rat and s["output_rate"] == want_r * want_rat:
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

sox -n -r 44100 -b 24 -c 2 "$TMPDIR/w44.wav" synth 1.2 sine 1000 gain -24.0
sox -n -r 48000 -b 24 -c 2 "$TMPDIR/w48.wav" synth 1.2 sine 1000 gain -24.0
sox -n -r 96000 -b 24 -c 2 "$TMPDIR/w96.wav" synth 1.2 sine 1000 gain -24.0

echo "=== 44.1 kHz -> 4x (176400 Hz) ==="
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/w44.wav" > "$TMPDIR/pw44.log" 2>&1 &
PW44=$!; PIDS+=("$PW44")
wait_rate_ratio 44100 4
wait "$PW44" || true
sleep 0.2
arecord -D hw:Loopback,1,0 -f S32_LE -r 176400 -c 2 -t wav -d 4 "$TMPDIR/cap44.wav" > "$TMPDIR/ar44.log" 2>&1 &
R44=$!; PIDS+=("$R44")
sleep 0.3
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/t44.wav" > "$TMPDIR/p44.log" 2>&1 &
P44=$!; PIDS+=("$P44")
wait "$P44"; wait "$R44"
python3 "$SCRIPT_DIR/analyze_rig.py" check_tone_level "$TMPDIR/cap44.wav" 176400 1.8 -7.0
sleep 0.4

echo "=== 48 kHz -> 4x (192000 Hz) ==="
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/w48.wav" > "$TMPDIR/pw48.log" 2>&1 &
PW48=$!; PIDS+=("$PW48")
wait_rate_ratio 48000 4
wait "$PW48" || true
sleep 0.2
arecord -D hw:Loopback,1,0 -f S32_LE -r 192000 -c 2 -t wav -d 4 "$TMPDIR/cap48.wav" > "$TMPDIR/ar48.log" 2>&1 &
R48=$!; PIDS+=("$R48")
sleep 0.3
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/t48.wav" > "$TMPDIR/p48.log" 2>&1 &
P48=$!; PIDS+=("$P48")
wait "$P48"; wait "$R48"
python3 "$SCRIPT_DIR/analyze_rig.py" check_tone_level "$TMPDIR/cap48.wav" 192000 1.8 -7.0
sleep 0.4

echo "=== 96 kHz -> 2x (192000 Hz) ==="
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/w96.wav" > "$TMPDIR/pw96.log" 2>&1 &
PW96=$!; PIDS+=("$PW96")
wait_rate_ratio 96000 2
wait "$PW96" || true
sleep 0.2
arecord -D hw:Loopback,1,0 -f S32_LE -r 192000 -c 2 -t wav -d 4 "$TMPDIR/cap96.wav" > "$TMPDIR/ar96.log" 2>&1 &
R96=$!; PIDS+=("$R96")
sleep 0.3
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/t96.wav" > "$TMPDIR/p96.log" 2>&1 &
P96=$!; PIDS+=("$P96")
wait "$P96"; wait "$R96"
python3 "$SCRIPT_DIR/analyze_rig.py" check_tone_level "$TMPDIR/cap96.wav" 192000 1.8 -7.0

echo "quit" >&3
wait "$RIG_PID" || true
exec 3>&-
echo "PASS: MaxRate(192000) resolved dynamically across 44.1k (4x), 48k (4x), 96k (2x)."
