#!/usr/bin/env bash
# Stress repro for the "one quantum of real audio then zeros" Direct-path flake.
#
# Each iteration starts a fresh engine_rig (mimicking the start of
# `tests/daemon/engine_direct.sh` right after another rig test tore down),
# switches the graph to 44.1 kHz with a short warm-up playback, then records
# the loopback card while a 44.1 kHz 24-bit noise file plays into
# `audivane_direct` and verifies the capture is bit-perfect.
#
# Usage: tests/stress/direct_start.sh [iterations] [--keep]
#   iterations  number of fresh-rig starts to run (default 30)
#   --keep      keep the temp dir and print it on failure
#
# Env knobs for diagnosis: PLAY_VOL (default 1.0, empty drops --volume),
# SET_VOL=0 skips `wpctl set-volume`, AUDIVANE_TEST_STALL_* are honoured by
# engine_rig as usual.
#
# Exits non-zero on the first failure and dumps the engine counters and rig log.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

ITERS="${1:-30}"
KEEP=0
[[ "${2:-}" == "--keep" ]] && KEEP=1

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
  if [[ "$KEEP" == "1" ]]; then
    echo "kept stress artifacts in $TMPDIR"
  else
    rm -rf "$TMPDIR"
  fi
  exit "$rc"
}
trap cleanup EXIT INT TERM

echo "Building engine_rig (fastrel)..."
CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-12}" cargo build --manifest-path "$ROOT_DIR/Cargo.toml" \
  --profile fastrel -p audivane-dsp-daemon --example engine_rig
BIN="$ROOT_DIR/target/fastrel/examples/engine_rig"

python3 "$SCRIPT_DIR/../daemon/analyze_rig.py" gen_bitperfect "$TMPDIR/noise44.wav" 44100 2.2
sox -n -r 44100 -b 24 -c 2 "$TMPDIR/warm44.wav" synth 0.6 sine 1000 gain -24

cat > "$TMPDIR/cfg.json" <<'EOF'
{
  "device": "hw:Loopback,0,0",
  "dsp_enabled": true,
  "chain": { "upsampler": { "target": { "ratio": 4 } } },
  "rate": {
    "follow_source": true,
    "allowed_rates": [44100, 48000, 88200, 96000, 176400, 192000],
    "default_rate": 48000
  }
}
EOF

# Keep CPU busy so RT wakeups occasionally land late (raises the hit rate of
# scheduling races), but never so much that the rig starves.
if command -v stress-ng >/dev/null 2>&1; then
  stress-ng --cpu "$(( $(nproc) / 8 + 1 ))" --timeout 600s >/dev/null 2>&1 &
  PIDS+=("$!")
  LOADER=stress-ng
else
  for _ in 1 2 3; do ( timeout 600s bash -c 'while :; do :; done' ) & PIDS+=("$!"); done
  LOADER=spin
fi
echo "CPU load: $LOADER"

snapshot_json() {
  python3 -c '
import json, sys
snaps = [json.loads(l) for l in open(sys.argv[1]) if l.strip().startswith("{")]
snaps = [s for s in snaps if s.get("type") == "snapshot"]
print(json.dumps(snaps[-1] if snaps else {}, indent=1))
' "$1"
}

start_rig() {
  local iter="$1"
  RIG_OUT="$TMPDIR/rig.out.$iter"
  RIG_LOG="$TMPDIR/rig.log.$iter"
  mkfifo "$TMPDIR/rig.in.$iter"
  exec 3<> "$TMPDIR/rig.in.$iter"
  "$BIN" "$TMPDIR/cfg.json" --interval-ms 100 < "$TMPDIR/rig.in.$iter" \
    > "$RIG_OUT" 2> "$RIG_LOG" &
  RIG_PID=$!
  PIDS+=("$RIG_PID")
  for _ in $(seq 1 100); do
    if pw-dump 2>/dev/null | python3 -c '
import json, sys
sys.exit(0 if any(o.get("info", {}).get("props", {}).get("node.name") == "audivane_direct" for o in json.load(sys.stdin)) else 1)
' 2>/dev/null; then
      return 0
    fi
    sleep 0.05
  done
  echo "ERROR: audivane_direct did not appear" >&2
  return 1
}

stop_rig() {
  echo "quit" >&3 2>/dev/null || true
  exec 3>&- 2>/dev/null || true
  wait "$RIG_PID" 2>/dev/null || true
}

wait_rate_ratio() {
  local want_rate="$1"
  local want_ratio="$2"
  for _ in $(seq 1 100); do
    if python3 -c '
import json, sys
want_r, want_rat = int(sys.argv[2]), int(sys.argv[3])
snaps = [json.loads(l) for l in open(sys.argv[1]) if l.strip().startswith("{")]
snaps = [s for s in snaps if s.get("type") == "snapshot"]
if snaps and snaps[-1]["graph_rate"] == want_r and snaps[-1]["output_rate"] == want_r * want_rat and snaps[-1]["ratio"] == want_rat and snaps[-1]["state"] != "switching":
    sys.exit(0)
sys.exit(1)
' "$RIG_OUT" "$want_rate" "$want_ratio" 2>/dev/null; then
      return 0
    fi
    sleep 0.05
  done
  echo "ERROR: timed out waiting for graph_rate=$want_rate ratio=$want_ratio" >&2
  return 1
}

fails=0
for i in $(seq 1 "$ITERS"); do
  start_rig "$i"

  DIRECT_ID="$(pw-dump | python3 -c '
import json, sys
for obj in json.load(sys.stdin):
    if obj.get("info", {}).get("props", {}).get("node.name") == "audivane_direct":
        print(obj["id"]); break
' 2>/dev/null || true)"
  if [[ "${SET_VOL:-1}" == "1" ]]; then
    wpctl set-volume "$DIRECT_ID" 1.0 >/dev/null
  fi
  wpctl set-mute "$DIRECT_ID" 0 >/dev/null

  # Force the 48k -> 44.1k rate switch, then stop immediately.
  pw-play --target audivane_direct --volume=${PLAY_VOL:-1.0} "$TMPDIR/warm44.wav" >/dev/null 2>&1 &
  W=$!
  if ! wait_rate_ratio 44100 1; then
    fails=$((fails+1))
    snapshot_json "$RIG_OUT" >&2 || true
    KEEP=1
    break
  fi
  wait "$W" 2>/dev/null || true

  # Start capture and playback back-to-back (no settle sleep).
  # CAP_ARGS lets a run mirror the engine's ALSA period/buffer (512/1536 frames)
  # instead of arecord's defaults (period 5513, buffer 22052).
  arecord -D hw:Loopback,1,0 -f S32_LE -r 44100 -c 2 -t wav -d 4 ${CAP_ARGS:-} "$TMPDIR/cap.wav" \
    > "$TMPDIR/arec.log" 2>&1 &
  R=$!
  sleep 0.05
  pw-play --target audivane_direct --volume=${PLAY_VOL:-1.0} "$TMPDIR/noise44.wav" > "$TMPDIR/play.log" 2>&1 &
  P=$!
  wait "$P" || true
  wait "$R" || true

  if ! python3 "$SCRIPT_DIR/../daemon/analyze_rig.py" verify_bitperfect "$TMPDIR/noise44.wav" "$TMPDIR/cap.wav" \
      > "$TMPDIR/verify.log" 2>&1; then
    fails=$((fails+1))
    echo "FAIL at iteration $i"
    # Stop the engine before collecting artifacts.
    echo "quit" >&3 2>/dev/null || true
    wait "$RIG_PID" 2>/dev/null || true
    exec 3>&- 2>/dev/null || true
    cat "$TMPDIR/verify.log"
    echo "--- last snapshot ---"
    snapshot_json "$RIG_OUT"
    echo "--- rig.log tail ---"
    tail -n 40 "$RIG_LOG" || true
    KEEP=1
    break
  fi
  echo "iter $i/$ITERS OK: $(cat "$TMPDIR/verify.log")"
  stop_rig
done

if [[ "$fails" == "0" ]]; then
  echo "PASS: $ITERS/$ITERS iterations bit-perfect at 44.1 kHz after a fresh start"
else
  echo "FAILED: $fails/$ITERS iterations"
  exit 1
fi
