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

# Chain A: flat 0 dB. Chain B: flat + Manual level offset -6.0 dB.
# Playing -6.0 dBFS tone: A outputs -6.0 dBFS; switching to B crossfades smoothly to -12.0 dBFS.
cat > "$TMPDIR/cfg.json" <<'EOF'
{
  "device": "hw:Loopback,0,0",
  "dsp_enabled": true,
  "chain": {
    "eq": { "name": "flat_a", "preamp_db": 0.0, "bands": [] }
  },
  "ab": {
    "chain_b": {
      "eq": { "name": "flat_b", "preamp_db": 0.0, "bands": [] }
    },
    "level": { "mode": "manual", "offset_db": -6.0 }
  },
  "rate": {
    "follow_source": false,
    "allowed_rates": [48000],
    "default_rate": 48000
  }
}
EOF

sox -n -r 48000 -b 24 -c 2 "$TMPDIR/tone6s.wav" synth 6 sine 1000 gain -6.0

mkfifo "$TMPDIR/rig.in"
exec 3<> "$TMPDIR/rig.in"
"$BIN" "$TMPDIR/cfg.json" --interval-ms 250 < "$TMPDIR/rig.in" > "$TMPDIR/rig.out" 2> "$TMPDIR/rig.log" &
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

echo "Running self-test command..."
echo "selftest" >&3
sleep 0.4

echo "Capturing 7s 48kHz while playing 6s tone and switching A -> B mid-stream..."
arecord -D hw:Loopback,1,0 -f S32_LE -r 48000 -c 2 -t wav -d 7 "$TMPDIR/cap_ab.wav" \
  > "$TMPDIR/arecord.log" 2>&1 &
REC_PID=$!
PIDS+=("$REC_PID")

sleep 0.4
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/tone6s.wav" > "$TMPDIR/play.log" 2>&1 &
PLAY_PID=$!
PIDS+=("$PLAY_PID")

sleep 2.5
echo "ab b 10" >&3

wait "$PLAY_PID"
wait "$REC_PID"

echo "quit" >&3
wait "$RIG_PID" || true
exec 3>&-

python3 "$SCRIPT_DIR/analyze_rig.py" check_ab "$TMPDIR/cap_ab.wav"

python3 -c '
import json, sys
replies = [json.loads(l) for l in open(sys.argv[1]) if l.strip().startswith("{")]
replies = [r for r in replies if r.get("type") == "reply"]
st = [r for r in replies if r.get("cmd") == "selftest"]
ab = [r for r in replies if r.get("cmd") == "ab"]
assert st and st[0]["ok"] and st[0]["selftest"]["passed"], f"selftest failed: {st}"
assert ab and ab[0]["ok"] and ab[0]["ab"]["current"] == "b" and abs(ab[0]["ab"]["offset_db"] - (-6.0)) < 1e-6, f"ab reply mismatch: {ab}"
print("PASS: selftest=%s ab_reply=%s" % (st[0]["selftest"]["expected_hash"], ab[0]["ab"]))
' "$TMPDIR/rig.out"
