#!/usr/bin/env bash
set -euo pipefail

# Regression test for Finding #1:
# Start at ratio=1 on audivane_dsp, apply config enabling 4x upsampling, play via
# audivane_direct (ratio=1), then return to audivane_dsp -> must preserve 4x
# upsampling (192000 Hz output) and cleanly capture audio. Also tests enabling
# 4x while audivane_direct is actively playing and then returning to audivane_dsp.

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

cat > "$TMPDIR/cfg_init.json" <<'EOF'
{
  "device": "hw:Loopback,0,0",
  "dsp_enabled": true,
  "chain": {
    "upsampler": { "target": "off" }
  },
  "rate": {
    "follow_source": false,
    "allowed_rates": [48000],
    "default_rate": 48000
  },
  "quantum": 1024
}
EOF

CFG_4X='{
  "device": "hw:Loopback,0,0",
  "dsp_enabled": true,
  "chain": {
    "eq": { "name": "g", "preamp_db": -2.0, "bands": [] },
    "upsampler": { "target": { "ratio": 4 }, "taps": 8192, "phase": "linear", "kaiser_beta": 14.0, "cutoff": 1.0 }
  },
  "rate": { "follow_source": false, "allowed_rates": [48000], "default_rate": 48000 },
  "quantum": 1024
}'

CFG_1X='{
  "device": "hw:Loopback,0,0",
  "dsp_enabled": true,
  "chain": { "upsampler": { "target": "off" } },
  "rate": { "follow_source": false, "allowed_rates": [48000], "default_rate": 48000 },
  "quantum": 1024
}'

sox -n -r 48000 -b 24 -c 2 "$TMPDIR/tone3s.wav" synth 3 sine 1000 gain -6.0
sox -n -r 48000 -b 24 -c 2 "$TMPDIR/tone2s.wav" synth 2 sine 1000 gain -6.0

mkfifo "$TMPDIR/rig.in"
exec 3<> "$TMPDIR/rig.in"
"$BIN" "$TMPDIR/cfg_init.json" --interval-ms 200 < "$TMPDIR/rig.in" > "$TMPDIR/rig.out" 2> "$TMPDIR/rig.log" &
RIG_PID=$!
PIDS+=("$RIG_PID")

for _ in $(seq 1 50); do
  if pw-dump | grep -q audivane_dsp && pw-dump | grep -q audivane_direct; then
    break
  fi
  sleep 0.1
done
for s in audivane_dsp audivane_direct; do
  SID="$(pw-dump | python3 -c "
import json, sys
for obj in json.load(sys.stdin):
    if obj.get('info', {}).get('props', {}).get('node.name') == '$s':
        print(obj['id']); break
" 2>/dev/null || true)"
  if [[ -z "$SID" ]]; then
    echo "ERROR: $s did not appear" >&2
    exit 1
  fi
  wpctl set-volume "$SID" 1.0
  wpctl set-mute "$SID" 0
done

wait_state() {
  local want_sink="$1"
  local want_ratio="$2"
  local want_out="$3"
  for _ in $(seq 1 60); do
    if python3 -c '
import json, sys
want_sink, want_rat, want_out = sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
snaps = [json.loads(l) for l in open(sys.argv[1]) if l.strip().startswith("{")]
snaps = [s for s in snaps if s.get("type") == "snapshot"]
if snaps:
    s = snaps[-1]
    if s["state"] == "playing" and s["active_sink"] == want_sink and s["ratio"] == want_rat and s["output_rate"] == want_out:
        sys.exit(0)
sys.exit(1)
' "$TMPDIR/rig.out" "$want_sink" "$want_ratio" "$want_out" 2>/dev/null; then
      return 0
    fi
    sleep 0.1
  done
  echo "ERROR: timed out waiting for sink=$want_sink ratio=$want_ratio out=$want_out" >&2
  tail -n 20 "$TMPDIR/rig.out" >&2 || true
  cat "$TMPDIR/rig.log" >&2 || true
  exit 1
}

echo "1. Play initial DSP at ratio=1..."
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/tone2s.wav" > "$TMPDIR/p1.log" 2>&1 &
P1=$!
PIDS+=("$P1")
wait_state "dsp" 1 48000
wait "$P1"

echo "2. Enable 4x upsampling via apply..."
echo "apply $(echo "$CFG_4X" | tr -d '\n')" >&3
sleep 0.4

echo "3. Play via Direct -> verifies ratio drops to 1 (48000 Hz)..."
pw-play --target audivane_direct --volume=1.0 "$TMPDIR/tone2s.wav" > "$TMPDIR/p2.log" 2>&1 &
P2=$!
PIDS+=("$P2")
wait_state "direct" 1 48000
wait "$P2"
sleep 0.3

sox -n -r 48000 -b 24 -c 2 "$TMPDIR/warm1s.wav" synth 1.2 sine 1000 gain -24.0

echo "4. Return to DSP -> MUST recover ratio=4 (192000 Hz)..."
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/warm1s.wav" > "$TMPDIR/w3.log" 2>&1 &
W3=$!
PIDS+=("$W3")
wait_state "dsp" 4 192000
wait "$W3" || true
sleep 0.2

arecord -D hw:Loopback,1,0 -f S32_LE -r 192000 -c 2 -t wav -d 4 "$TMPDIR/cap_ret192.wav" \
  > "$TMPDIR/arec1.log" 2>&1 &
R1=$!
PIDS+=("$R1")
sleep 0.3
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/tone3s.wav" > "$TMPDIR/p3.log" 2>&1 &
P3=$!
PIDS+=("$P3")
wait "$P3"
wait "$R1"
python3 "$SCRIPT_DIR/analyze_rig.py" check_tone_level "$TMPDIR/cap_ret192.wav" 192000 2.0 -8.0

echo "5. Apply off then enable 4x WHILE Direct is actively playing -> return to DSP..."
echo "apply $(echo "$CFG_1X" | tr -d '\n')" >&3
sleep 0.4
pw-play --target audivane_direct --volume=1.0 "$TMPDIR/tone3s.wav" > "$TMPDIR/p4.log" 2>&1 &
P4=$!
PIDS+=("$P4")
wait_state "direct" 1 48000
echo "apply $(echo "$CFG_4X" | tr -d '\n')" >&3
wait "$P4"
sleep 0.3

pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/warm1s.wav" > "$TMPDIR/w5.log" 2>&1 &
W5=$!
PIDS+=("$W5")
wait_state "dsp" 4 192000
wait "$W5" || true
sleep 0.2

arecord -D hw:Loopback,1,0 -f S32_LE -r 192000 -c 2 -t wav -d 4 "$TMPDIR/cap_ret2_192.wav" \
  > "$TMPDIR/arec2.log" 2>&1 &
R2=$!
PIDS+=("$R2")
sleep 0.3
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/tone3s.wav" > "$TMPDIR/p5.log" 2>&1 &
P5=$!
PIDS+=("$P5")
wait "$P5"
wait "$R2"
python3 "$SCRIPT_DIR/analyze_rig.py" check_tone_level "$TMPDIR/cap_ret2_192.wav" 192000 2.0 -8.0

echo "quit" >&3
wait "$RIG_PID" || true
exec 3>&-
echo "PASS: Direct <-> DSP upsampling preserved across config changes in both orders."
