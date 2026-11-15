#!/usr/bin/env bash
set -euo pipefail

# Two apps at different native rates (44.1k and 48k) into the DSP sink at once.
# The graph rate must settle on the first app's rate and stay put (no
# flip-flop, audio keeps flowing), in both start orders; when the first app
# stops, the rate moves to the remaining one.

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
HOLD_S="${HOLD_S:-20}"

TMPDIR="$(mktemp -d)"
PIDS=()

ORIG_ALLOWED_RATES="$(pw-metadata -n settings | awk -F"value:'" '/clock\.allowed-rates/{split($2,a,"'\''"); print a[1]}')"
ORIG_FORCE_RATE="$(pw-metadata -n settings | awk -F"value:'" '/clock\.force-rate/{split($2,a,"'\''"); print a[1]}')"

cleanup() {
  local rc=$?
  if [[ $rc -ne 0 && -f "$TMPDIR/rig.out" ]]; then
    echo '--- last engine snapshot (gap counters) ---' >&2
    grep '"type":"snapshot"' "$TMPDIR/rig.out" | tail -n 1 >&2 || true
    echo '--- rate decisions ---' >&2
    grep 'Rate' "$TMPDIR/rig.log" >&2 || true
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
  "chain": { "eq": { "name": "trim", "preamp_db": -1.0, "bands": [] } },
  "rate": {
    "follow_source": true,
    "allowed_rates": [44100, 48000, 88200, 96000, 176400, 192000],
    "default_rate": 96000,
    "conflict": "keep_current"
  },
  "quantum": 1024
}
EOF

LONG_S=$((HOLD_S + 40))
sox -n -r 44100 -b 24 -c 2 "$TMPDIR/t44.wav" synth "$LONG_S" sine 1000 gain -24.0
sox -n -r 48000 -b 24 -c 2 "$TMPDIR/t48.wav" synth "$LONG_S" sine 1000 gain -24.0

mkfifo "$TMPDIR/rig.in"
exec 3<> "$TMPDIR/rig.in"
RUST_LOG=info "$BIN" "$TMPDIR/cfg.json" --interval-ms 200 < "$TMPDIR/rig.in" > "$TMPDIR/rig.out" 2> "$TMPDIR/rig.log" &
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
  [[ -n "$SINK_ID" ]] && break
  sleep 0.1
done
if [[ -z "$SINK_ID" ]]; then
  echo "ERROR: audivane_dsp did not appear" >&2
  exit 1
fi
wpctl set-volume "$SINK_ID" 1.0
wpctl set-mute "$SINK_ID" 0

# Last snapshot as "graph_rate state rate_switches dsp_popped_frames".
snap() {
  python3 -c '
import json, sys
snaps = [json.loads(l) for l in open(sys.argv[1]) if l.strip().startswith("{")]
snaps = [s for s in snaps if s.get("type") == "snapshot"]
s = snaps[-1] if snaps else None
print(s["graph_rate"], s["state"], s["gaps"]["rate_switches"], s["gaps"]["dsp_popped_frames"]) if s else print("0 none 0 0")
' "$TMPDIR/rig.out"
}

wait_rate() {
  local want="$1" timeout_ds="$2"
  for _ in $(seq 1 "$timeout_ds"); do
    read -r rate state _ _ < <(snap)
    if [[ "$rate" == "$want" && "$state" == "playing" ]]; then
      return 0
    fi
    sleep 0.1
  done
  echo "ERROR: timed out waiting for graph_rate=$want (now: $(snap))" >&2
  exit 1
}

# Graph rate must stay at $1 with audio flowing for $2 seconds.
hold_rate() {
  local want="$1" secs="$2"
  # graph_rate changes before the ALSA reopen for it is counted: settle first.
  sleep 1
  read -r _ _ sw0 pop0 < <(snap)
  for _ in $(seq 1 $((secs * 2))); do
    sleep 0.5
    read -r rate state sw pop < <(snap)
    if [[ "$rate" != "$want" || "$state" != "playing" || "$sw" != "$sw0" ]]; then
      echo "ERROR: rate moved while both apps play: graph_rate=$rate state=$state rate_switches $sw0 -> $sw" >&2
      exit 1
    fi
  done
  read -r _ _ _ pop1 < <(snap)
  if (( pop1 - pop0 < (secs * want) / 2 )); then
    echo "ERROR: audio stalled: only $((pop1 - pop0)) frames in ${secs}s at $want" >&2
    exit 1
  fi
  echo "  held $want for ${secs}s, $((pop1 - pop0)) frames out, no rate switch"
}

run_order() {
  local first="$1" second="$2"
  echo "=== ${first} first, then ${second} ==="
  pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/t${first:0:2}.wav" > "$TMPDIR/pa.log" 2>&1 &
  local pa=$!; PIDS+=("$pa")
  wait_rate "$first" 100
  sleep 1
  pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/t${second:0:2}.wav" > "$TMPDIR/pb.log" 2>&1 &
  local pb=$!; PIDS+=("$pb")
  sleep 1
  hold_rate "$first" "$HOLD_S"
  local t0; t0=$(date +%s.%N)
  kill "$pa"; wait "$pa" 2>/dev/null || true
  wait_rate "$second" 80
  printf '  first app stopped: rate moved to %s after %.1fs\n' "$second" "$(awk -v a="$(date +%s.%N)" -v b="$t0" 'BEGIN{print a-b}')"
  hold_rate "$second" 5
  kill "$pb"; wait "$pb" 2>/dev/null || true
  sleep 1.5
}

# The first app stops right after the switch it caused: the hand-over waits for
# the minimum dwell (2 s after that switch) instead of switching straight back.
quick_stop() {
  echo "=== 44100 starts and stops within a second; 48000 keeps playing ==="
  pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/t44.wav" > "$TMPDIR/pa.log" 2>&1 &
  local pa=$!; PIDS+=("$pa")
  wait_rate 44100 100
  local t0; t0=$(date +%s.%N)
  pw-play --target audivane_dsp --volume=1.0 "$TMPDIR/t48.wav" > "$TMPDIR/pb.log" 2>&1 &
  local pb=$!; PIDS+=("$pb")
  sleep 0.5
  kill "$pa"; wait "$pa" 2>/dev/null || true
  wait_rate 48000 80
  local dt; dt="$(awk -v a="$(date +%s.%N)" -v b="$t0" 'BEGIN{print a-b}')"
  printf '  rate moved to 48000 %.1fs after the switch to 44100\n' "$dt"
  if awk -v d="$dt" 'BEGIN{exit !(d < 1.4)}'; then
    echo "ERROR: switched back after ${dt}s, inside the 2 s dwell" >&2
    exit 1
  fi
  hold_rate 48000 5
  kill "$pb"; wait "$pb" 2>/dev/null || true
  sleep 1.5
}

run_order 48000 44100
run_order 44100 48000
quick_stop

echo "--- rate decisions logged ---"
grep 'Rate:' "$TMPDIR/rig.log" | sed 's/^/  /' || true

echo "quit" >&3
wait "$RIG_PID" || true
exec 3>&-
echo "PASS: mixed 44.1k + 48k settles on the first app's rate in both orders and hands over when it stops."
