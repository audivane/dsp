#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

TMPDIR="$(mktemp -d)"
PIDS=()

# Save current PipeWire settings metadata so we restore cleanly.
ORIG_ALLOWED_RATES="$(pw-metadata -n settings | awk -F"value:'" '/clock\.allowed-rates/{split($2,a,"'\''"); print a[1]}')"
ORIG_FORCE_RATE="$(pw-metadata -n settings | awk -F"value:'" '/clock\.force-rate/{split($2,a,"'\''"); print a[1]}')"

cleanup() {
  local rc=$?
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  # Restore exactly what was there before, deleting keys that were absent.
  if [[ -n "$ORIG_ALLOWED_RATES" ]]; then
    pw-metadata -n settings 0 clock.allowed-rates "$ORIG_ALLOWED_RATES" >/dev/null 2>&1 || true
  else
    pw-metadata -n settings -d 0 clock.allowed-rates >/dev/null 2>&1 || true
  fi
  if [[ -n "$ORIG_FORCE_RATE" ]]; then
    pw-metadata -n settings 0 clock.force-rate "$ORIG_FORCE_RATE" >/dev/null 2>&1 || true
  else
    pw-metadata -n settings -d 0 clock.force-rate >/dev/null 2>&1 || true
  fi
  rm -rf "$TMPDIR"
  exit "$rc"
}
trap cleanup EXIT INT TERM

CARGO_BUILD_JOBS=12 cargo build --manifest-path "$ROOT_DIR/Cargo.toml" \
  --profile fastrel -p audivane-io-pw --example driver_spike
BIN="$ROOT_DIR/target/fastrel/examples/driver_spike"

sox -n -r 44100 -b 24 -c 2 "$TMPDIR/sine44k.wav" synth 4 sine 1000 gain -6

echo "=== Case 1: Fixed-rate 48k driver receiving 44.1k stream ==="
"$BIN" --device hw:Loopback,0,0 --rate 48000 --ratio 4 --format s32 --period 1024 --seconds 5 \
  > "$TMPDIR/case1.log" 2>&1 &
SPIKE_PID=$!
PIDS+=("$SPIKE_PID")
sleep 1.0
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/sine44k.wav" > "$TMPDIR/play1.log" 2>&1 &
PLAY_PID=$!
PIDS+=("$PLAY_PID")
sleep 1.5
pw-top -b -n 2 > "$TMPDIR/pwtop1.txt" 2>&1 || true
wait "$PLAY_PID" || true
wait "$SPIKE_PID" || true
echo "--- pw-top Case 1 ---"
grep -E "NAME|audivane_dsp|pw-play" "$TMPDIR/pwtop1.txt" || cat "$TMPDIR/pwtop1.txt"
echo "--- FINAL Case 1 ---"
grep -E "FINAL|xruns" "$TMPDIR/case1.log" | tail -n 3

echo ""
echo "=== Case 2: --follow-rate with clock.allowed-rates=[44100 48000 ...] (no force-rate) ==="
pw-metadata -n settings 0 clock.allowed-rates "[ 44100 48000 88200 96000 176400 192000 ]" >/dev/null
"$BIN" --device hw:Loopback,0,0 --rate 48000 --ratio 4 --format s32 --period 1024 --follow-rate --seconds 5 \
  > "$TMPDIR/case2.log" 2>&1 &
SPIKE_PID=$!
PIDS+=("$SPIKE_PID")
sleep 1.0
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/sine44k.wav" > "$TMPDIR/play2.log" 2>&1 &
PLAY_PID=$!
PIDS+=("$PLAY_PID")
sleep 1.5
pw-top -b -n 2 > "$TMPDIR/pwtop2.txt" 2>&1 || true
wait "$PLAY_PID" || true
wait "$SPIKE_PID" || true
echo "--- pw-top Case 2 ---"
grep -E "NAME|audivane_dsp|pw-play" "$TMPDIR/pwtop2.txt" || cat "$TMPDIR/pwtop2.txt"
echo "--- Case 2 log ---"
cat "$TMPDIR/case2.log"

echo ""
echo "=== Case 3: --follow-rate with clock.force-rate=44100 ==="
pw-metadata -n settings 0 clock.force-rate 44100 >/dev/null
"$BIN" --device hw:Loopback,0,0 --rate 48000 --ratio 4 --format s32 --period 1024 --follow-rate --seconds 5 \
  > "$TMPDIR/case3.log" 2>&1 &
SPIKE_PID=$!
PIDS+=("$SPIKE_PID")
sleep 1.0
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/sine44k.wav" > "$TMPDIR/play3.log" 2>&1 &
PLAY_PID=$!
PIDS+=("$PLAY_PID")
sleep 1.5
pw-top -b -n 2 > "$TMPDIR/pwtop3.txt" 2>&1 || true
wait "$PLAY_PID" || true
wait "$SPIKE_PID" || true
echo "--- pw-top Case 3 ---"
grep -E "NAME|audivane_dsp|pw-play" "$TMPDIR/pwtop3.txt" || cat "$TMPDIR/pwtop3.txt"
echo "--- Case 3 log ---"
cat "$TMPDIR/case3.log"

# Verify Case 3 actually adapted ALSA to 44100 * 4 = 176400 Hz with 0 xruns!
grep -q "graph_rate=44100" "$TMPDIR/case3.log" || {
  echo "ERROR: Case 3 did not switch graph_rate to 44100" >&2
  exit 1
}
grep -q "rate=176400" "$TMPDIR/case3.log" || {
  echo "ERROR: Case 3 did not reopen ALSA at 176400 Hz" >&2
  exit 1
}
echo "PASS: Exploratory rate test completed cleanly (44.1k resampling, allowed-rates, and 176.4k rate switch verified)."
