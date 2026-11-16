#!/usr/bin/env bash
set -euo pipefail

# High graph rates (352.8k-768k) once allowed: the graph follows a 352.8/384/
# 705.6/768 kHz stream, MaxRate(768000) falls back to the ratio that still fits
# (2x, then 1x), the quantum is capped at 8192, and the tone reaches the
# loopback DAC at the right level. Needs snd-aloop (hw:Loopback) and the
# PipeWire session; PipeWire's clock settings are restored on exit.

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
    "upsampler": { "target": { "max_rate": 768000 }, "taps": 8192, "phase": "linear", "kaiser_beta": 14.0, "cutoff": 1.0 }
  },
  "rate": {
    "follow_source": true,
    "allowed_rates": [44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000],
    "default_rate": 48000
  },
  "quantum": 1024
}
EOF

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


# The graph's quantum in the latest snapshot (PipeWire caps it at 8192).
check_quantum() {
  python3 - "$TMPDIR/rig.out" "$1" <<'PY'
import json, sys
snaps = [json.loads(l) for l in open(sys.argv[1]) if l.strip().startswith("{")]
snaps = [s for s in snaps if s.get("type") == "snapshot"]
q = snaps[-1].get("quantum") if snaps else None
print(f"  quantum {q}")
if q != int(sys.argv[2]):
    sys.exit(f"ERROR: quantum {q}, expected {sys.argv[2]}")
PY
}

run_rate() {
  local src="$1" ratio="$2" quantum="$3"
  local out=$((src * ratio))
  echo "=== ${src} Hz -> ${ratio}x (${out} Hz) ==="
  sox -n -r "$src" -b 24 -c 2 "$TMPDIR/w$src.wav" synth 1.2 sine 1000 gain -24.0
  sox -n -r "$src" -b 24 -c 2 "$TMPDIR/t$src.wav" synth 3 sine 1000 gain -6.0
  pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/w$src.wav" > "$TMPDIR/pw$src.log" 2>&1 &
  local pw=$!; PIDS+=("$pw")
  wait_rate_ratio "$src" "$ratio"
  check_quantum "$quantum"
  wait "$pw" || true
  sleep 0.2
  arecord -D hw:Loopback,1,0 -f S32_LE -r "$out" -c 2 -t wav -d 4 "$TMPDIR/cap$src.wav" > "$TMPDIR/ar$src.log" 2>&1 &
  local rec=$!; PIDS+=("$rec")
  sleep 0.3
  pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/t$src.wav" > "$TMPDIR/p$src.log" 2>&1 &
  local play=$!; PIDS+=("$play")
  wait "$play"; wait "$rec"
  python3 "$SCRIPT_DIR/analyze_rig.py" check_tone_level "$TMPDIR/cap$src.wav" "$out" 1.8 -7.0
  sleep 0.4
}

run_rate 352800 2 4096
run_rate 384000 2 8192
run_rate 705600 1 8192
run_rate 768000 1 8192

echo "quit" >&3
wait "$RIG_PID" || true
exec 3>&-
echo "PASS: 352.8k/384k (2x) and 705.6k/768k (1x) follow the source and reach the DAC at the right level."
