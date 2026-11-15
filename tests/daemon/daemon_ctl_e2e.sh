#!/usr/bin/env bash
# End-to-end: real `audivane-dsp` daemon on the loopback "DAC", driven only
# through `avdsp ctl`. Covers DSP path + upsampling, A/B, selftest, Direct
# bit-perfect, release/acquire and shutdown. Exit non-zero on any failure.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
T="$(mktemp -d)"
PIDS=()
export AUDIVANE_DSP_SOCKET="$T/sock"
cleanup() {
  local rc=$?
  "$B/avdsp" ctl shutdown >/dev/null 2>&1 || true
  for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null || true; done
  sleep 1
  rm -rf "$T"
  exit "$rc"
}
trap cleanup EXIT INT TERM
CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-24} cargo build -q --manifest-path "$ROOT/Cargo.toml" \
  --profile fastrel -p audivane-dsp-daemon -p audivane-dsp-cli
B="$ROOT/target/fastrel"
C="$B/avdsp ctl"
fail() { echo "FAIL: $*"; exit 1; }

echo '{"device":"hw:Loopback,0,0","routing":{"mode":"apps","rules":[]}}' > "$T/config.json"
"$B/audivane-dsp" --config "$T/config.json" --socket "$T/sock" --foreground > "$T/daemon.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 50); do $C status >/dev/null 2>&1 && break; sleep 0.1; done
$C status | grep -q 'state: *ready' || fail "daemon not ready"

# --- DSP path: EQ (-3 dB preamp, -2 dB @ 1 kHz) + 4x upsampling -> -11 dBFS tone
printf 'Preamp: -3 dB\nFilter 1: ON PK Fc 1000 Hz Gain -2 dB Q 1.0\n' > "$T/eq.txt"
$C eq set "$T/eq.txt" >/dev/null
$C upsample --ratio 4 --taps 65536 >/dev/null
sox -n -r 48000 -b 24 -c 2 "$T/sine.wav" synth 6 sine 1000 gain -6
pw-play --target audivane_dsp --volume 1.0 "$T/sine.wav" &
PLAY=$!; PIDS+=($PLAY)
sleep 1.5
arecord -q -D hw:Loopback,1,0 -f S32_LE -c 2 -r 192000 -d 2 "$T/dsp.wav"
$C status | grep -q '192000 Hz DAC (x4)' || fail "not upsampling x4"
$C status | grep -q 'xruns 0' || fail "xruns during DSP playback"
python3 - "$T/dsp.wav" <<'PY' || fail "DSP level"
import sys, wave, numpy as np
w = wave.open(sys.argv[1]); x = np.frombuffer(w.readframes(w.getnframes()), '<i4').reshape(-1, 2) / 2**31
seg = x[len(x)//4:, 0]; lvl = 20*np.log10(np.sqrt(np.mean(seg**2))*np.sqrt(2))
print(f"DSP tone level {lvl:.3f} dBFS (expected -11.000)")
sys.exit(0 if abs(lvl + 11.0) < 0.05 else 1)
PY

# --- A/B: B = -9 dB preamp -> -15 dBFS; selftest
printf 'Preamp: -9 dB\n' > "$T/b.txt"
$C ab set "$T/b.txt" --level off >/dev/null
$C ab b --fade 10 >/dev/null
sleep 0.5
arecord -q -D hw:Loopback,1,0 -f S32_LE -c 2 -r 192000 -d 1 "$T/b.wav"
python3 - "$T/b.wav" <<'PY' || fail "A/B level"
import sys, wave, numpy as np
w = wave.open(sys.argv[1]); x = np.frombuffer(w.readframes(w.getnframes()), '<i4').reshape(-1, 2) / 2**31
seg = x[len(x)//4:, 0]; lvl = 20*np.log10(np.sqrt(np.mean(seg**2))*np.sqrt(2))
print(f"A/B side B level {lvl:.3f} dBFS (expected -15.000)")
sys.exit(0 if abs(lvl + 15.0) < 0.05 else 1)
PY
$C selftest | grep -q PASS || fail "selftest"
wait "$PLAY"
# A/B coupling regression: turning upsampling off must work with A/B set.
$C upsample --off >/dev/null || fail "upsample --off with A/B configured"
$C ab off >/dev/null

# --- Direct path bit-perfect at 44.1 kHz (stereo 24-bit white noise)
"$B/avdsp" gen white --rate 44100 --bits 24 --seconds 4 --seed 7 --channels 2 "$T/white.wav" >/dev/null
pw-play --target audivane_direct --volume 1.0 "$T/white.wav" &
PLAY=$!; PIDS+=($PLAY)
sleep 1.2
$C status | grep -q 'sink: *direct' || fail "direct sink not active"
arecord -q -D hw:Loopback,1,0 -f S32_LE -c 2 -r 44100 -d 2 "$T/direct.wav"
python3 - "$T/white.wav" "$T/direct.wav" <<'PY' || fail "Direct not bit-perfect"
import sys, wave, numpy as np
s = wave.open(sys.argv[1]); raw = np.frombuffer(s.readframes(s.getnframes()), np.uint8).reshape(-1, 3).astype(np.int32)
v = raw[:, 0] | (raw[:, 1] << 8) | (raw[:, 2] << 16); v = np.where(v >= 1 << 23, v - (1 << 24), v).reshape(-1, 2)
c = wave.open(sys.argv[2]); y = np.frombuffer(c.readframes(c.getnframes()), '<i4').reshape(-1, 2)
y = y[np.nonzero(y[:, 0])[0][0]:]
if np.count_nonzero(y & 0xff): sys.exit("low byte not zero")
y = y >> 8; key = y[:32, 0]
idx = [i for i in range(len(v) - 32) if np.array_equal(v[i:i+32, 0], key)]
if not idx: sys.exit("no alignment")
i = idx[0]; m = min(len(y), len(v) - i); z = y[:m]; z = z[:np.nonzero(z[:, 0])[0][-1] + 1]
ok = np.array_equal(z, v[i:i+len(z)]); print(f"Direct: {len(z)} stereo frames bit-identical: {ok}")
sys.exit(0 if ok and len(z) > 40000 else 1)
PY
wait "$PLAY"

# --- release / acquire / shutdown
$C release >/dev/null
for _ in $(seq 1 50); do wpctl status | grep -q 'Loopback Analog Stereo' && break; sleep 0.1; done
wpctl status | grep -q 'Loopback Analog Stereo' || fail "WirePlumber did not retake the card"
$C acquire >/dev/null
$C status | grep -q 'state: *ready' || fail "not ready after acquire"
$C shutdown >/dev/null
for _ in $(seq 1 50); do pgrep -x audivane-dsp >/dev/null || break; sleep 0.1; done
pgrep -x audivane-dsp >/dev/null && fail "daemon did not exit"
[ -e "$T/sock" ] && fail "socket not removed"
echo "PASS: daemon + ctl end-to-end"
