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

mkfifo "$TMPDIR/rig.in"
exec 3<> "$TMPDIR/rig.in"
"$BIN" "$TMPDIR/cfg.json" --interval-ms 250 < "$TMPDIR/rig.in" > "$TMPDIR/rig.out" 2> "$TMPDIR/rig.log" &
RIG_PID=$!
PIDS+=("$RIG_PID")

for _ in $(seq 1 50); do
  if pw-dump | grep -q audivane_dsp; then
    break
  fi
  sleep 0.1
done

sleep 0.5
echo "--- wpctl status while Audivane owns Loopback ---"
wpctl status > "$TMPDIR/wp_owned.txt"
if grep -q "Loopback" "$TMPDIR/wp_owned.txt"; then
  echo "ERROR: WirePlumber still holds Loopback while Audivane owns it:" >&2
  cat "$TMPDIR/wp_owned.txt" >&2
  exit 1
fi

echo "=== Sending release ==="
echo "release" >&3

# Wait for Loopback to return to WirePlumber wpctl status (up to 4s).
RETURNED=0
for _ in $(seq 1 40); do
  if wpctl status | grep -q "Loopback"; then
    RETURNED=1
    break
  fi
  sleep 0.1
done
wpctl status > "$TMPDIR/wp_released.txt"
if [[ "$RETURNED" -ne 1 ]]; then
  echo "ERROR: Loopback did not return to WirePlumber after release:" >&2
  cat "$TMPDIR/wp_released.txt" >&2
  cat "$TMPDIR/rig.log" >&2 || true
  exit 1
fi
echo "Confirmed: Loopback returned to WirePlumber in wpctl status:"
grep "Loopback" "$TMPDIR/wp_released.txt"

echo "=== Sending acquire ==="
echo "acquire" >&3

REACQUIRED=0
for _ in $(seq 1 50); do
  if ! wpctl status | grep -q "Loopback"; then
    REACQUIRED=1
    break
  fi
  sleep 0.1
done
if [[ "$REACQUIRED" -ne 1 ]]; then
  echo "ERROR: Loopback did not disappear from WirePlumber after acquire" >&2
  wpctl status >&2
  cat "$TMPDIR/rig.log" >&2 || true
  exit 1
fi

# Verify audio plays cleanly after re-acquire!
sox -n -r 48000 -b 24 -c 2 "$TMPDIR/tone2s.wav" synth 2 sine 1000 gain -6.0
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
  echo "ERROR: audivane_dsp did not reappear after acquire" >&2
  pw-dump | grep -E "audivane|Loopback" >&2 || true
  cat "$TMPDIR/rig.log" >&2 || true
  exit 1
fi
wpctl set-volume "$SINK_ID" 1.0
wpctl set-mute "$SINK_ID" 0

arecord -D hw:Loopback,1,0 -f S32_LE -r 48000 -c 2 -t wav -d 3 "$TMPDIR/cap_acq.wav" \
  > "$TMPDIR/arecord.log" 2>&1 &
REC_PID=$!
PIDS+=("$REC_PID")
sleep 0.3
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/tone2s.wav" > "$TMPDIR/play.log" 2>&1 &
PLAY_PID=$!
PIDS+=("$PLAY_PID")
wait "$PLAY_PID"
wait "$REC_PID"

echo "=== Rapid release/acquire loop x20 ==="
for iter in $(seq 1 20); do
  echo "release" >&3
  echo "acquire" >&3
done

# Wait until 20 more release + acquire replies arrive cleanly, then play audio.
python3 -c '
import json, time, sys
path = sys.argv[1]
deadline = time.time() + 20.0
while time.time() < deadline:
    lines = [json.loads(l) for l in open(path) if l.strip().startswith("{")]
    rels = [r for r in lines if r.get("type") == "reply" and r.get("cmd") == "release"]
    acqs = [r for r in lines if r.get("type") == "reply" and r.get("cmd") == "acquire"]
    if len(rels) >= 21 and len(acqs) >= 21:
        assert all(r["ok"] for r in rels), f"release error: {rels}"
        assert all(a["ok"] for a in acqs), f"acquire error: {acqs}"
        sys.exit(0)
    time.sleep(0.05)
raise AssertionError(f"Timed out waiting for 20x release/acquire replies (rels={len(rels)} acqs={len(acqs)})")
' "$TMPDIR/rig.out"

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
wpctl set-volume "$SINK_ID" 1.0
wpctl set-mute "$SINK_ID" 0

arecord -D hw:Loopback,1,0 -f S32_LE -r 48000 -c 2 -t wav -d 3 "$TMPDIR/cap_acq2.wav" \
  > "$TMPDIR/arecord2.log" 2>&1 &
REC2_PID=$!
PIDS+=("$REC2_PID")
sleep 0.3
pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/tone2s.wav" > "$TMPDIR/play2.log" 2>&1 &
PLAY2_PID=$!
PIDS+=("$PLAY2_PID")
wait "$PLAY2_PID"
wait "$REC2_PID"

echo "quit" >&3
wait "$RIG_PID" || true
exec 3>&-

python3 -c '
import json, math, sys
from pathlib import Path
sys.path.insert(0, str(Path(sys.argv[3])))
from analyze_rig import read_s32_wav

snaps = [json.loads(l) for l in open(sys.argv[1]) if l.strip().startswith("{")]
replies = [r for r in snaps if r.get("type") == "reply"]
rel = [r for r in replies if r.get("cmd") == "release"]
acq = [r for r in replies if r.get("cmd") == "acquire"]
assert len(rel) == 21 and all(r["ok"] for r in rel), f"release failed: {rel}"
assert len(acq) == 21 and all(r["ok"] for r in acq), f"acquire failed: {acq}"

for wav in (sys.argv[2], sys.argv[4]):
    rate, left, _ = read_s32_wav(wav)
    rms = math.sqrt(sum(x * x for x in left) / len(left))
    assert rms > 0.15, f"Expected audible playback after re-acquire in {wav}, got rms={rms:.5f}"
print("PASS: 1+20x rapid release/acquire verified with consistent state and clean audio playback.")
' "$TMPDIR/rig.out" "$TMPDIR/cap_acq.wav" "$SCRIPT_DIR" "$TMPDIR/cap_acq2.wav"
