#!/usr/bin/env bash
set -euo pipefail

# Regression test for Finding #5:
# Custom PipeWire settings metadata (`clock.allowed-rates`, `clock.force-rate`)
# must be captured before Audivane overwrites them, restored on clean exit,
# AND crash-recovered via `$XDG_RUNTIME_DIR/audivane-dsp/settings-restore.json`
# after SIGKILL (`kill -9`).

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

TMPDIR="$(mktemp -d)"
PIDS=()

ORIG_ALLOWED_RATES="$(pw-metadata -n settings | awk -F"value:'" '/clock\.allowed-rates/{split($2,a,"'\''"); print a[1]}')"
ORIG_FORCE_RATE="$(pw-metadata -n settings | awk -F"value:'" '/clock\.force-rate/{split($2,a,"'\''"); print a[1]}')"
RESTORE_FILE="${XDG_RUNTIME_DIR:-/tmp}/audivane-dsp/settings-restore.json"

cleanup() {
  local rc=$?
  for pid in "${PIDS[@]:-}"; do
    kill -9 "$pid" 2>/dev/null || true
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

rm -f "$RESTORE_FILE"
CUSTOM_RATES="[ 44100 48000 96000 ]"
CUSTOM_FORCE="44100"
pw-metadata -n settings 0 clock.allowed-rates "$CUSTOM_RATES" Spa:String:JSON >/dev/null
pw-metadata -n settings 0 clock.force-rate "$CUSTOM_FORCE" Spa:String:JSON >/dev/null
sleep 0.2

cat > "$TMPDIR/cfg.json" <<'EOF'
{
  "device": "hw:Loopback,0,0",
  "dsp_enabled": true,
  "rate": {
    "follow_source": false,
    "allowed_rates": [48000, 192000],
    "default_rate": 48000
  }
}
EOF

get_meta() {
  local key="$1"
  pw-metadata -n settings | awk -F"value:'" "/${key//./\\.}/{split(\$2,a,\"'\"); print a[1]}"
}

echo "=== Part A: clean start and stop restores exact original metadata ==="
mkfifo "$TMPDIR/rig1.in"
exec 3<> "$TMPDIR/rig1.in"
"$BIN" "$TMPDIR/cfg.json" --interval-ms 250 < "$TMPDIR/rig1.in" > "$TMPDIR/rig1.out" 2> "$TMPDIR/rig1.log" &
RIG1=$!; PIDS+=("$RIG1")

for _ in $(seq 1 50); do
  if pw-dump | grep -q audivane_dsp && [[ -f "$RESTORE_FILE" ]]; then
    break
  fi
  sleep 0.1
done
[[ -f "$RESTORE_FILE" ]] || { echo "ERROR: restore file not created" >&2; exit 1; }

CUR_RATES="$(get_meta clock.allowed-rates)"
CUR_FORCE="$(get_meta clock.force-rate)"
[[ "$CUR_RATES" == "[ 48000 ]" ]] || { echo "ERROR: engine did not update clock.allowed-rates (got $CUR_RATES)" >&2; exit 1; }
[[ "$CUR_FORCE" == "48000" ]] || { echo "ERROR: engine did not update clock.force-rate (got $CUR_FORCE)" >&2; exit 1; }

echo "quit" >&3
wait "$RIG1" || true
exec 3>&-
sleep 0.2

RESTORED_RATES="$(get_meta clock.allowed-rates)"
RESTORED_FORCE="$(get_meta clock.force-rate)"
[[ "$RESTORED_RATES" == "$CUSTOM_RATES" ]] || {
  echo "ERROR: clean exit did not restore clock.allowed-rates: expected '$CUSTOM_RATES', got '$RESTORED_RATES'" >&2
  exit 1
}
[[ "$RESTORED_FORCE" == "$CUSTOM_FORCE" ]] || {
  echo "ERROR: clean exit did not restore clock.force-rate: expected '$CUSTOM_FORCE', got '$RESTORED_FORCE'" >&2
  exit 1
}
[[ ! -f "$RESTORE_FILE" ]] || { echo "ERROR: restore file was not deleted on clean exit" >&2; exit 1; }

echo "=== Part B: kill -9 crash leaves restore file; next start/stop recovers custom originals ==="
"$BIN" "$TMPDIR/cfg.json" --interval-ms 250 < /dev/null > "$TMPDIR/rig2.out" 2> "$TMPDIR/rig2.log" &
RIG2=$!; PIDS+=("$RIG2")
for _ in $(seq 1 50); do
  if pw-dump | grep -q audivane_dsp && [[ -f "$RESTORE_FILE" ]]; then
    break
  fi
  sleep 0.1
done
sleep 0.3
kill -9 "$RIG2"
wait "$RIG2" 2>/dev/null || true

[[ -f "$RESTORE_FILE" ]] || { echo "ERROR: restore file missing after kill -9" >&2; exit 1; }
# Metadata is currently stuck at Audivane's overridden values ([ 48000 192000 ] / 48000).
# Restarting must recover the pre-crash originals first (and re-save them before modifying).
mkfifo "$TMPDIR/rig3.in"
exec 4<> "$TMPDIR/rig3.in"
"$BIN" "$TMPDIR/cfg.json" --interval-ms 250 < "$TMPDIR/rig3.in" > "$TMPDIR/rig3.out" 2> "$TMPDIR/rig3.log" &
RIG3=$!; PIDS+=("$RIG3")
for _ in $(seq 1 50); do
  if pw-dump | grep -q audivane_dsp; then
    break
  fi
  sleep 0.1
done
sleep 0.3
echo "quit" >&4
wait "$RIG3" || true
exec 4>&-
sleep 0.2

POST_CRASH_RATES="$(get_meta clock.allowed-rates)"
POST_CRASH_FORCE="$(get_meta clock.force-rate)"
[[ "$POST_CRASH_RATES" == "$CUSTOM_RATES" ]] || {
  echo "ERROR: post-crash stop did not restore '$CUSTOM_RATES', got '$POST_CRASH_RATES'" >&2
  exit 1
}
[[ "$POST_CRASH_FORCE" == "$CUSTOM_FORCE" ]] || {
  echo "ERROR: post-crash stop did not restore '$CUSTOM_FORCE', got '$POST_CRASH_FORCE'" >&2
  exit 1
}
[[ ! -f "$RESTORE_FILE" ]] || { echo "ERROR: restore file not cleaned up after recovery exit" >&2; exit 1; }

echo "PASS: PipeWire metadata capture, exact clean restore, and kill -9 crash-file recovery verified."
