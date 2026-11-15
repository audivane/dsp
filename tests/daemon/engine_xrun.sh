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
  "rate": {
    "follow_source": false,
    "allowed_rates": [48000],
    "default_rate": 48000
  }
}
EOF

sox -n -r 48000 -b 24 -c 2 "$TMPDIR/tone5s.wav" synth 5 sine 1000 gain -6.0

AUDIVANE_TEST_STALL_MS=200 AUDIVANE_TEST_STALL_AFTER_MS=1000 \
  "$BIN" "$TMPDIR/cfg.json" --interval-ms 250 < /dev/null > "$TMPDIR/rig.out" 2> "$TMPDIR/rig.log" &
RIG_PID=$!
PIDS+=("$RIG_PID")

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
  echo "ERROR: audivane_dsp did not appear" >&2
  exit 1
fi
wpctl set-volume "$SINK_ID" 1.0
wpctl set-mute "$SINK_ID" 0

arecord -D hw:Loopback,1,0 -f S32_LE -r 48000 -c 2 -t wav -d 5 "$TMPDIR/cap_xrun.wav" \
  > "$TMPDIR/arecord.log" 2>&1 &
REC_PID=$!
PIDS+=("$REC_PID")

sleep 0.4
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/tone5s.wav" > "$TMPDIR/play.log" 2>&1 &
PLAY_PID=$!
PIDS+=("$PLAY_PID")

wait "$PLAY_PID"
wait "$REC_PID"
kill "$RIG_PID" 2>/dev/null || true
wait "$RIG_PID" 2>/dev/null || true

python3 "$SCRIPT_DIR/analyze_rig.py" check_xrun "$TMPDIR/rig.out" "$TMPDIR/cap_xrun.wav"
