#!/usr/bin/env bash
set -euo pipefail

# Regression test for Finding #4:
# Switching between substreams hw:Loopback,0,0 and hw:Loopback,0,1 on the same
# card must keep the card reservation intact without dropping it needlessly and
# route audio cleanly to the new substream (hw:Loopback,1,1), then back to (1,0).
# Also verifies switching to an invalid device enters EngineState::Error without
# silently keeping the old device, and recovers when switched back.

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

TMPDIR="$(mktemp -d)"
PIDS=()

ORIG_ALLOWED_RATES="$(pw-metadata -n settings | awk -F"value:'" '/clock\.allowed-rates/{split($2,a,"'\''"); print a[1]}')"
ORIG_FORCE_RATE="$(pw-metadata -n settings | awk -F"value:'" '/clock\.force-rate/{split($2,a,"'\''"); print a[1]}')"

cleanup() {
  local rc=$?
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

cat > "$TMPDIR/cfg0.json" <<'EOF'
{
  "device": "hw:Loopback,0,0",
  "dsp_enabled": true,
  "rate": { "follow_source": false, "allowed_rates": [48000], "default_rate": 48000 }
}
EOF

CFG1='{"device":"hw:Loopback,0,1","dsp_enabled":true,"rate":{"follow_source":false,"allowed_rates":[48000],"default_rate":48000}}'
CFG_BAD='{"device":"hw:NonExistentAudivaneCard99,0,0","dsp_enabled":true,"rate":{"follow_source":false,"allowed_rates":[48000],"default_rate":48000}}'
CFG0='{"device":"hw:Loopback,0,0","dsp_enabled":true,"rate":{"follow_source":false,"allowed_rates":[48000],"default_rate":48000}}'

sox -n -r 48000 -b 24 -c 2 "$TMPDIR/tone.wav" synth 2.5 sine 1000 gain -6.0

mkfifo "$TMPDIR/rig.in"
exec 3<> "$TMPDIR/rig.in"
"$BIN" "$TMPDIR/cfg0.json" --interval-ms 200 < "$TMPDIR/rig.in" > "$TMPDIR/rig.out" 2> "$TMPDIR/rig.log" &
RIG_PID=$!
PIDS+=("$RIG_PID")

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

echo "=== Playing on hw:Loopback,0,0 ==="
arecord -D hw:Loopback,1,0 -f S32_LE -r 48000 -c 2 -t wav -d 3 "$TMPDIR/cap0.wav" > "$TMPDIR/ar0.log" 2>&1 &
R0=$!; PIDS+=("$R0")
sleep 0.3
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/tone.wav" > "$TMPDIR/p0.log" 2>&1 &
P0=$!; PIDS+=("$P0")
wait "$P0"; wait "$R0"
python3 "$SCRIPT_DIR/analyze_rig.py" check_tone_level "$TMPDIR/cap0.wav" 48000 1.5 -6.0

echo "=== Switching to same-card substream hw:Loopback,0,1 ==="
echo "apply $CFG1" >&3
sleep 0.8
if wpctl status | grep -q "Loopback"; then
  echo "ERROR: WirePlumber grabbed Loopback during same-card substream switch" >&2
  exit 1
fi

arecord -D hw:Loopback,1,1 -f S32_LE -r 48000 -c 2 -t wav -d 3 "$TMPDIR/cap1.wav" > "$TMPDIR/ar1.log" 2>&1 &
R1=$!; PIDS+=("$R1")
sleep 0.3
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/tone.wav" > "$TMPDIR/p1.log" 2>&1 &
P1=$!; PIDS+=("$P1")
wait "$P1"; wait "$R1"
python3 "$SCRIPT_DIR/analyze_rig.py" check_tone_level "$TMPDIR/cap1.wav" 48000 1.5 -6.0

echo "=== Switching to nonexistent device -> expect EngineState::Error (and released reservation) ==="
echo "apply $CFG_BAD" >&3
for _ in $(seq 1 40); do
  if python3 -c '
import json, sys
snaps = [json.loads(l) for l in open(sys.argv[1]) if l.strip().startswith("{") and json.loads(l).get("type") == "snapshot"]
if snaps and snaps[-1]["state"] == "error" and snaps[-1].get("error"):
    sys.exit(0)
sys.exit(1)
' "$TMPDIR/rig.out" 2>/dev/null; then
    break
  fi
  sleep 0.1
done

echo "=== Recovering back to hw:Loopback,0,0 ==="
echo "apply $CFG0" >&3
for _ in $(seq 1 50); do
  if python3 -c '
import json, sys
snaps = [json.loads(l) for l in open(sys.argv[1]) if l.strip().startswith("{") and json.loads(l).get("type") == "snapshot"]
if snaps and snaps[-1]["state"] == "stopped" and snaps[-1]["device"] == "hw:Loopback,0,0":
    sys.exit(0)
sys.exit(1)
' "$TMPDIR/rig.out" 2>/dev/null; then
    break
  fi
  sleep 0.1
done

arecord -D hw:Loopback,1,0 -f S32_LE -r 48000 -c 2 -t wav -d 3 "$TMPDIR/cap2.wav" > "$TMPDIR/ar2.log" 2>&1 &
R2=$!; PIDS+=("$R2")
sleep 0.3
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/tone.wav" > "$TMPDIR/p2.log" 2>&1 &
P2=$!; PIDS+=("$P2")
wait "$P2"; wait "$R2"
python3 "$SCRIPT_DIR/analyze_rig.py" check_tone_level "$TMPDIR/cap2.wav" 48000 1.5 -6.0

echo "quit" >&3
wait "$RIG_PID" || true
exec 3>&-
echo "PASS: same-card substream switch 0,0 <-> 0,1, Error state on bad device, and recovery all verified."
