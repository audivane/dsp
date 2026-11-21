#!/usr/bin/env bash
# Sink volume ownership and the volume guard on the loopback "DAC".
#
# A real `audivane-dsp` daemon with `volume.sink_volume: follow`, driven by
# `wpctl` (what the desktop slider and media keys do) and `avdsp ctl`. The
# loopback capture shows what reaches the DAC: the sink volume must be
# applied exactly once (by the engine), never by PipeWire on top of it.
#
#  1. wpctl set-volume 0.5 on audivane_dsp -> engine -18.06 dB, PipeWire at unity
#  2. the same write again does not apply PipeWire's volume on top
#  3. avdsp ctl volume -10 shows on the sink (channelVolumes 0.316)
#  4. +10.6 dB from the desktop is capped at the ceiling (-3 dB)
#  5. guard lock reverts a desktop change; guard limit raises gradually;
#     app streams are locked / capped the same way
#  6. audivane_direct is held at unity, bit-perfect at 0 dB, attenuated below
#  7. a restart starts the sink at the engine's volume (no WirePlumber restore)
#  8. sink_volume ignore hands the sink back at unity
#  9. switching back to follow adopts the desktop's quieter level
# 10. a softVolumes-only write is put back
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
T="$(mktemp -d)"
PIDS=()
export AUDIVANE_DSP_SOCKET="$T/sock"
# The volume failsafe's state stays in the scratch directory; its
# "released" marker lives next to PipeWire's socket, so it is removed
# afterwards unless it was there already.
export XDG_STATE_HOME="$T/state"
RELEASED="${XDG_RUNTIME_DIR:-/nonexistent}/audivane-dsp/dac-released"
[ -e "$RELEASED" ] && KEEP_RELEASED=1 || KEEP_RELEASED=0
cleanup() {
  local rc=$?
  "$B/avdsp" ctl shutdown >/dev/null 2>&1 || true
  for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null || true; done
  # The daemon writes the marker on its way out: wait for it first.
  for p in "${PIDS[@]:-}"; do
    [ -n "$p" ] || continue
    for _ in $(seq 1 100); do kill -0 "$p" 2>/dev/null || break; sleep 0.1; done
  done
  sleep 0.2
  [ "$KEEP_RELEASED" = 1 ] || rm -f "$RELEASED"
  if [[ $rc -ne 0 && -f "$T/daemon.log" ]]; then tail -n 30 "$T/daemon.log" >&2; fi
  rm -rf "$T"
  exit "$rc"
}
trap cleanup EXIT INT TERM
CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-16} cargo build -q --manifest-path "$ROOT/Cargo.toml" \
  --profile fastrel -p audivane-dsp-daemon -p audivane-dsp-cli
B="$ROOT/target/fastrel"
C="$B/avdsp ctl"
fail() { echo "FAIL: $*"; exit 1; }

node_id() {
  pw-dump | python3 -c '
import json, sys
for o in json.load(sys.stdin):
    if (o.get("info", {}).get("props") or {}).get("node.name") == sys.argv[1]:
        print(o["id"]); break' "$1"
}
# channelVolumes[0] mute of a node, from its Props.
props() {
  pw-dump "$1" | python3 -c '
import json, sys
for o in json.load(sys.stdin):
    for p in (o.get("info", {}).get("params") or {}).get("Props", []):
        if "channelVolumes" in p:
            print("%.4f %s" % (p["channelVolumes"][0], str(p.get("mute", False)).lower())); sys.exit()'
}
# What a node plays (master volume x channelVolumes[0]).
played() {
  pw-dump "$1" | python3 -c '
import json, sys
for o in json.load(sys.stdin):
    for p in (o.get("info", {}).get("params") or {}).get("Props", []):
        if "channelVolumes" in p:
            print("%.4f" % (p.get("volume", 1.0) * p["channelVolumes"][0])); sys.exit()'
}
blocked() { $C --json volume | python3 -c 'import json,sys; print(json.load(sys.stdin)["blocked"])'; }
# Peak level (dBFS) of channel 0 of a capture, middle half.
level() {
  python3 - "$1" <<'PY'
import sys, wave, numpy as np
w = wave.open(sys.argv[1]); x = np.frombuffer(w.readframes(w.getnframes()), '<i4').reshape(-1, 2) / 2**31
seg = x[len(x) // 4: 3 * len(x) // 4, 0]
print("%.3f" % (20 * np.log10(np.max(np.abs(seg)) + 1e-12)))
PY
}
capture() { arecord -q -D hw:Loopback,1,0 -f S32_LE -c 2 -r 48000 -d "${2:-1}" "$1"; }
near() { python3 -c "import sys; sys.exit(0 if abs($1 - ($2)) <= ${3:-0.05} else 1)"; }
cfg_volume() { $C --json volume | python3 -c "import json,sys; print(json.load(sys.stdin)['$1'])"; }

start_daemon() {
  "$B/audivane-dsp" --config "$T/config.json" --socket "$T/sock" --foreground >> "$T/daemon.log" 2>&1 &
  DAEMON=$!
  PIDS+=($DAEMON)
  for _ in $(seq 1 50); do $C status >/dev/null 2>&1 && break; sleep 0.1; done
  local st
  for _ in $(seq 1 30); do
    st=$($C status 2>&1 || true)
    grep -q 'state: *ready' <<<"$st" && break
    sleep 0.1
  done
  grep -q 'state: *ready' <<<"$st" || { echo "$st"; tail -n 20 "$T/daemon.log"; fail "daemon not ready"; }
  for _ in $(seq 1 30); do DSP=$(node_id audivane_dsp); [ -n "$DSP" ] && break; sleep 0.1; done
  DIRECT=$(node_id audivane_direct)
  sleep 0.3
}

# `software`: the loopback card has a `PCM` mixer control, which `auto` would
# use, but it does not scale what the capture side receives.
cat > "$T/config.json" <<'EOF'
{
  "device": "hw:Loopback,0,0",
  "routing": { "mode": "apps", "rules": [] },
  "rate": { "follow_source": false, "allowed_rates": [48000], "default_rate": 48000 },
  "chain": { "output": { "bits": 24, "dither": "none" } },
  "volume": { "volume_db": 0, "mode": "software", "sink_volume": "follow", "max_volume_db": -3 }
}
EOF
start_daemon
echo "dsp sink $DSP, direct sink $DIRECT"

sox -n -r 48000 -b 24 -c 2 "$T/sine.wav" synth 40 sine 1000 gain -6
pw-play --target audivane_dsp --volume 1.0 "$T/sine.wav" &
PLAY=$!; PIDS+=($PLAY)
sleep 1.5
STREAM=$(pw-dump | python3 -c '
import json, sys
for o in json.load(sys.stdin):
    p = o.get("info", {}).get("props") or {}
    if p.get("media.class") == "Stream/Output/Audio" and "pw-play" in str(p.get("application.name", "")):
        print(o["id"]); break')
[ -n "$STREAM" ] || fail "pw-play stream not found"
# WirePlumber restores pw-play's stream volume from earlier runs.
wpctl set-volume "$STREAM" 1.0
sleep 0.3

# Start: the ceiling holds, the sink shows the engine's level.
capture "$T/a.wav"
L=$(level "$T/a.wav"); echo "start: $L dBFS (expected -9.00: -6 tone, -3 ceiling)"
near "$L" -9.0 || fail "ceiling at start"
read -r CV _ <<<"$(props "$DSP")"
near "$CV" 0.7079 0.001 || fail "sink shows $CV, expected 0.708 (-3 dB)"

# The node's current channelmix.max-volume (PipeWire's own gain limit).
max_volume() {
  local dump
  dump=$(pw-dump "$1")
  grep -A1 '"channelmix.max-volume",' <<<"$dump" | tail -n1 | tr -d ' ,'
}

# 1. Desktop sets 50 % (cubic: 0.125 linear, -18.06 dB).
wpctl set-volume "$DSP" 0.5
sleep 0.4
capture "$T/b.wav"
L=$(level "$T/b.wav"); echo "wpctl 0.5: $L dBFS (expected -24.06, applied once)"
near "$L" -24.06 || fail "sink volume applied $L, not once"
V=$(cfg_volume volume_db); echo "engine volume_db $V"
near "$V" -18.062 0.002 || fail "engine did not follow the sink"
read -r CV _ <<<"$(props "$DSP")"
near "$CV" 0.125 0.0005 || fail "sink should still show 0.125, shows $CV"

# 2. The same write again: PipeWire must not apply it on top.
wpctl set-volume "$DSP" 0.5
sleep 0.4
capture "$T/c.wav"
L=$(level "$T/c.wav"); echo "same write again: $L dBFS (expected -24.06)"
near "$L" -24.06 || fail "a repeated write was applied twice"

# 3. Our own volume shows on the sink.
$C volume -10 >/dev/null
sleep 0.4
read -r CV _ <<<"$(props "$DSP")"
echo "ctl volume -10: sink shows $CV (expected 0.3162)"
near "$CV" 0.3162 0.0005 || fail "engine volume not reflected on the sink"
capture "$T/d.wav"
L=$(level "$T/d.wav"); near "$L" -16.0 || fail "ctl volume level $L"

# Mute from the desktop is the engine mute.
wpctl set-mute "$DSP" 1
sleep 0.4
[ "$(cfg_volume muted)" = "True" ] || fail "desktop mute not followed"
capture "$T/m.wav"
L=$(level "$T/m.wav"); echo "muted: $L dBFS"
python3 -c "import sys; sys.exit(0 if $L < -200 else 1)" || fail "mute leaks $L"
wpctl set-mute "$DSP" 0
sleep 0.3

# 4. +10.6 dB from the desktop: capped at the ceiling.
wpctl set-volume "$DSP" 1.5
sleep 0.4
capture "$T/e.wav"
L=$(level "$T/e.wav"); echo "wpctl 1.5: $L dBFS (expected -9.00, ceiling)"
near "$L" -9.0 || fail "ceiling not applied to the sink request"
read -r CV _ <<<"$(props "$DSP")"
near "$CV" 0.7079 0.001 || fail "sink should show the ceiling, shows $CV"
$C volume | grep -q blocked || fail "blocked change not reported"

# 5a. Guard lock: a desktop change is reverted.
$C volume -20 --guard lock >/dev/null
sleep 0.3
wpctl set-volume "$DSP" 1.0
sleep 0.4
capture "$T/f.wav"
L=$(level "$T/f.wav"); echo "lock, wpctl 1.0: $L dBFS (expected -26.00)"
near "$L" -26.0 || fail "lock did not hold"
read -r CV _ <<<"$(props "$DSP")"
near "$CV" 0.1 0.0005 || fail "lock should show 0.1 (-20 dB), shows $CV"

# 5b. Guard limit at 10 dB/s: -30 -> -10 takes about 2 s.
$C volume -30 --guard limit --rise 10 >/dev/null
sleep 0.3
wpctl set-volume "$DSP" 0.7079   # cubic 0.7079 -> -9 dB linear; capped by nothing, -10 below
sleep 0.5
V=$(cfg_volume volume_db); echo "limit after 0.5 s: $V dB"
python3 -c "import sys; sys.exit(0 if -27.0 <= $V <= -23.0 else 1)" || fail "limit rise not gradual ($V)"
sleep 2.5
V=$(cfg_volume volume_db); echo "limit after 3 s: $V dB"
near "$V" -9.0 0.1 || fail "limit did not reach the target ($V)"
wpctl set-volume "$DSP" 0.2
sleep 0.3
V=$(cfg_volume volume_db); echo "limit fall: $V dB (expected -41.94 at once)"
near "$V" -41.938 0.01 || fail "limit fall not immediate"

# 5c. App stream guard: lock blocks the app's rise (and accepts a fall),
#     limit caps it.
wpctl set-volume "$STREAM" 0.5   # cubic: 0.125 linear
sleep 0.3
$C volume --guard lock >/dev/null
sleep 0.3
wpctl set-volume "$STREAM" 1.0
sleep 0.4
read -r CV _ <<<"$(props "$STREAM")"
echo "stream lock, wpctl 1.0: stream shows $CV (expected 0.125, held)"
near "$CV" 0.125 0.0005 || fail "stream lock did not block the rise"
wpctl set-volume "$STREAM" 0.4   # 0.064: a fall, the new held level
sleep 0.4
read -r CV _ <<<"$(props "$STREAM")"
echo "stream lock, wpctl 0.4: stream shows $CV (expected 0.064, accepted)"
near "$CV" 0.064 0.0005 || fail "stream lock held a fall up"
$C volume --guard limit --stream-max -12 --rise 0 >/dev/null
sleep 0.3
wpctl set-volume "$STREAM" 0.2
sleep 0.4
read -r CV _ <<<"$(props "$STREAM")"
near "$CV" 0.008 0.0005 || fail "stream limit should pass a fall, shows $CV"
wpctl set-volume "$STREAM" 1.0
sleep 0.4
read -r CV _ <<<"$(props "$STREAM")"
echo "stream limit, wpctl 1.0: stream shows $CV (expected 0.2512, -12 dB cap)"
near "$CV" 0.2512 0.0005 || fail "stream cap not applied"
# The app's master volume counts: 2.0 is capped as a whole and not
# rewritten over and over.
pw-cli s "$STREAM" Props '{ volume: 2.0 }' >/dev/null
sleep 0.4
P=$(played "$STREAM"); echo "stream limit, master 2.0: plays $P (expected 0.2512)"
near "$P" 0.2512 0.0005 || fail "stream master volume escaped the cap"
B1=$(blocked); sleep 1; B2=$(blocked)
echo "guard writes in the second after: $((B2 - B1)) (expected 0)"
[ $((B2 - B1)) -eq 0 ] || fail "guard keeps rewriting the stream"
$C --json volume | grep -q '"kind": "stream"' || fail "stream block not reported"
$C volume --guard off >/dev/null
sleep 0.2
wpctl set-volume "$STREAM" 1.0   # leave WirePlumber's stored value at 100 %
wait "$PLAY" 2>/dev/null || true
kill "$PLAY" 2>/dev/null || true

# 6. Direct: held at unity, bit-perfect at 0 dB, and never louder than the
#    volume otherwise (software mode has no hardware part).
$C volume --ceiling none >/dev/null
$C volume -20 >/dev/null
pw-play --target audivane_direct --volume 1.0 "$T/sine.wav" &
PLAY=$!; PIDS+=($PLAY)
sleep 1.2
capture "$T/dd.wav"
kill "$PLAY" 2>/dev/null || true
wait "$PLAY" 2>/dev/null || true
L=$(level "$T/dd.wav"); echo "direct at -20 dB (software): $L dBFS (expected -26.00)"
near "$L" -26.0 || fail "Direct ignored the volume"
$C volume 0 >/dev/null
wpctl set-volume "$DIRECT" 0.5
sleep 0.4
read -r CV _ <<<"$(props "$DIRECT")"
echo "direct after wpctl 0.5: shows $CV (expected 1.0)"
near "$CV" 1.0 0.0001 || fail "Direct volume not held at unity"
"$B/avdsp" gen white --rate 48000 --bits 24 --seconds 4 --seed 7 --channels 2 "$T/white.wav" >/dev/null
pw-play --target audivane_direct --volume 1.0 "$T/white.wav" &
PLAY=$!; PIDS+=($PLAY)
sleep 1.2
wpctl set-volume "$DIRECT" 0.3
sleep 0.2
capture "$T/direct.wav" 2
wait "$PLAY" 2>/dev/null || true
python3 - "$T/white.wav" "$T/direct.wav" <<'PY' || fail "Direct not bit-perfect"
import sys, wave, numpy as np
s = wave.open(sys.argv[1]); raw = np.frombuffer(s.readframes(s.getnframes()), np.uint8).reshape(-1, 3).astype(np.int32)
v = raw[:, 0] | (raw[:, 1] << 8) | (raw[:, 2] << 16); v = np.where(v >= 1 << 23, v - (1 << 24), v).reshape(-1, 2)
c = wave.open(sys.argv[2]); y = np.frombuffer(c.readframes(c.getnframes()), '<i4').reshape(-1, 2) >> 8
seg = y[len(y) // 4: len(y) // 4 + 20000, 0]
src = v[:, 0]
for off in range(len(src) - len(seg)):
    if src[off] == seg[0] and np.array_equal(src[off:off + len(seg)], seg):
        print(f"Direct bit-perfect over {len(seg)} frames (offset {off})"); sys.exit(0)
print("no bit-exact match"); sys.exit(1)
PY

MV=$(max_volume "$DSP"); echo "follow: channelmix.max-volume $MV (expected 1.0)"
near "$MV" 1.0 0.0001 || fail "PipeWire may scale above unity while following"

# 7. Restart: the sink starts at the engine's volume, not a WirePlumber restore.
$C volume -12 --ceiling -3 >/dev/null
sleep 0.3
$C shutdown >/dev/null
wait "$DAEMON" 2>/dev/null || true
start_daemon
read -r CV _ <<<"$(props "$DSP")"
echo "after restart: sink shows $CV (expected 0.2512, -12 dB)"
near "$CV" 0.2512 0.0005 || fail "restart did not start at the engine volume"
DUMP=$(pw-dump "$DSP")
grep -Eq '"state.restore-props": "?false' <<<"$DUMP" || fail "restore-props not off"

# 8. Ignore: the sinks go back to plain unity.
$C volume --sink-volume ignore >/dev/null
$C volume 0 --ceiling none >/dev/null
sleep 0.4
read -r CV _ <<<"$(props "$DSP")"
echo "ignore: sink shows $CV (expected 1.0)"
near "$CV" 1.0 0.0001 || fail "ignore did not release the sink"
MV=$(max_volume "$DSP"); echo "ignore: channelmix.max-volume $MV (expected 10.0)"
near "$MV" 10.0 0.0001 || fail "ignore did not restore PipeWire's limit"

# 9. Back to follow while the desktop had the sink at 30 %: the engine takes
#    that level over instead of jumping to its own.
pw-play --target audivane_dsp --volume 1.0 "$T/sine.wav" &
PLAY=$!; PIDS+=($PLAY)
sleep 1.2
wpctl set-volume "$DSP" 0.3      # cubic: 0.027 linear, -31.37 dB
sleep 0.4
capture "$T/g.wav"
L=$(level "$T/g.wav"); echo "ignore, desktop 30 %: $L dBFS (expected -37.37: -6 tone, -31.37 PipeWire)"
near "$L" -37.37 0.1 || fail "PipeWire volume in ignore mode"
# The sink's own max-volume (here set by hand) is what ignore restores later.
pw-cli s "$DSP" Props '{ params = [ "channelmix.max-volume" 5.0 ] }' >/dev/null
sleep 0.3
MV=$(max_volume "$DSP"); near "$MV" 5.0 0.0001 || fail "could not set max-volume 5 ($MV)"
$C volume --sink-volume follow >/dev/null
sleep 0.5
capture "$T/h.wav"
L=$(level "$T/h.wav"); echo "follow again: $L dBFS (expected -37.37, adopted by the engine)"
near "$L" -37.37 0.1 || fail "switching to follow changed the level"
V=$(cfg_volume volume_db); near "$V" -31.37 0.02 || fail "engine did not adopt the sink level ($V)"

# 10. A write of PipeWire's own scaling only is put back.
pw-cli s "$DSP" Props '{ softVolumes: [ 0.25, 0.25 ] }' >/dev/null
sleep 0.4
capture "$T/i.wav"
L=$(level "$T/i.wav"); echo "softVolumes-only write: $L dBFS (expected -37.37)"
near "$L" -37.37 0.1 || fail "softVolumes write applied"
MV=$(max_volume "$DSP"); near "$MV" 1.0 0.0001 || fail "max-volume not 1 while following ($MV)"
$C volume --sink-volume ignore >/dev/null
sleep 0.4
MV=$(max_volume "$DSP"); echo "ignore again: channelmix.max-volume $MV (expected 5.0, the sink's own)"
near "$MV" 5.0 0.0001 || fail "ignore did not restore the sink's own max-volume"
pw-cli s "$DSP" Props '{ params = [ "channelmix.max-volume" 10.0 ] }' >/dev/null
kill "$PLAY" 2>/dev/null || true
echo "PASS"
