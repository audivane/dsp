# Audivane DSP: developer notes

See [docs/architecture.md](docs/architecture.md) for the architecture. This file
is how to build, run and test. Design notes: [docs/pipewire-driver.md](docs/pipewire-driver.md)
(how the engine drives PipeWire from the DAC clock) and
[docs/control-plane.md](docs/control-plane.md) (IPC, routing, config, restore files).

## Layout

| Crate | Binary | Role |
| --- | --- | --- |
| `crates/protocol` | | serde config + IPC types (the contract between everything) |
| `crates/dsp-core` | | pure f64 DSP, no I/O, no allocation in `process` |
| `crates/io-alsa` | | direct ALSA `hw:` output, DAC clock, `ReserveDevice1` |
| `crates/io-pw` | | PipeWire sinks driven by the DAC clock, routing |
| `crates/daemon` | `audivane-dsp` | the engine (systemd user service) |
| `crates/cli` | `avdsp` | offline processing, tests, benchmarks, daemon control |

## Build and test

```sh
cargo build                       # dev; dsp-core is always opt-level 3
cargo nextest run                 # all tests, parallel (or: cargo test)
cargo nextest run -p audivane-dsp-core
cargo build --profile fastrel     # optimized, thin LTO, incremental: benches/spikes
cargo build --release             # fat LTO, no debug info: the shipped engine and CLI
python3 tests/reference/run.py    # numpy/scipy cross-checks (needs avdsp built)
```

Rules for code on the audio thread: no allocation, no locks, no syscalls
except the ALSA/PipeWire calls themselves. Long tests (> ~2 s) go behind
`#[ignore]` and run with `cargo nextest run --run-ignored all`: tier B of the
filter fingerprints, the design and drift simulations, the harmonics
identity and speed checks, and the limiter and measurement reports.

The daemon and IPC/CLI suites (driven with raw protocol lines and the real
`avdsp` binary on a temporary socket, against a `MockEngine`; the binary is
built on demand, override with `AVDSP_BIN`):

```sh
cargo nextest run -p audivane-dsp-daemon             # unit + IPC/CLI/MPRIS tests
# Real PipeWire (two temporary f3_it_* null sinks, a pw-play stream):
cargo nextest run -p audivane-dsp-daemon --run-ignored all -E 'test(graph_it)'
# ... including the session-default parts (both metadata keys, normal stop and
# a simulated kill -9; briefly changes the default and restores it exactly):
AUDIVANE_IT_DEFAULT_SINK=1 cargo nextest run -p audivane-dsp-daemon \
    --run-ignored all -E 'test(graph_it)'
```

The default-sink tests hold a file lock (`$TMPDIR/avdsp-f3-default-sink.lock`)
so concurrent nextest processes cannot interleave their changes, and the
original values are restored even if a test panics. The IPC/CLI suite covers
the preset round trip (save/list/get/load/rename/delete, overwrite refusal,
`modified`, active-preset tracking across a restart), APO import, device
listing, `config`/`presets` events and the coalesced config writer (write
counts and flush on shutdown). The ALSA device parser is unit-tested against
canned `/proc/asound/pcm` and `/proc/asound/cards` text, so it needs no sound
card.

## Build from source

Needs Rust (stable), Node 22+, clang, and the development packages for
WebKitGTK 4.1, GTK 3, PipeWire (1.0 or newer), ALSA and
D-Bus. On Ubuntu/Debian:

```sh
sudo apt install build-essential pkg-config clang libclang-dev \
  libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev \
  libpipewire-0.3-dev libspa-0.2-dev libasound2-dev libdbus-1-dev
(cd app && npm ci && npm run build)
cargo build --release -p audivane-dsp-daemon -p audivane-dsp-cli
cargo build --profile release-ui -p audivane-dsp-ui --features custom-protocol
```

That gives `target/release/{audivane-dsp,avdsp}` and
`target/release-ui/audivane-dsp-ui`. On Arch, `packaging/arch/` builds a
package with `makepkg` instead (see its README).

## Release packages (.deb, .rpm, Arch, AppImage)

```sh
packaging/release/release.sh            # committed HEAD -> dist/release/<version>/
packaging/release/release.sh --worktree # include uncommitted changes
packaging/release/release.sh --clean    # cold build (drops the cached target dir)
```

One build makes all four artifacts: the three packages and the AppImage. It
compiles inside a rootless Ubuntu 24.04 root (`packaging/release/baseline.sh`,
set up once in ~6 minutes, no root or containers), so the binaries need only
glibc 2.39 and run on Ubuntu/Debian 24.04+, Fedora 40+ and Arch. nfpm (pinned,
checksummed) wraps the same files into `.deb`, `.rpm` and `.pkg.tar.zst`, then
the Arch package is installed in the Arch build root and the `.deb` in a
throwaway copy of the Ubuntu root as smoke tests, each installed twice so the
upgrade scriptlet runs too (`archroot.sh smoke <pkg> [<upgrade-pkg>]`,
`baseline.sh deb-smoke <deb> [<upgrade-deb>]`). `appimage.sh` then puts the
same staged files into an AppImage (appimagetool and the static type 2
runtime, both pinned and checksummed; see `packaging/appimage/README.md`).
Output: the three packages, the AppImage and `SHA256SUMS`, ready to attach to
a GitHub release. The `Release packages` GitHub workflow runs the same scripts
on demand (manual trigger only) when no local machine is available.

The engine uses the `release` profile (fat LTO, one codegen unit, no debug
info: every packaged binary is stripped); the app uses `release-ui` (thin LTO,
16 units). Both compile at the same time, and the protocol unit tests run
alongside in release mode in a third target dir with thin LTO, so the packages
need one build, not two.

Scriptlets: `packaging/release/postinstall.sh` is the one post-install script
for all three formats; `package.sh` wraps it with a dispatch line per format
(deb `configure <old>`, rpm `$1`, Arch `post_install`), and Arch gets a
separate `post_upgrade`. On upgrade it runs
`systemctl --user -M <user>@ try-restart audivane-dsp.service` for each running
user manager; `packaging/arch/audivane-dsp.install` (makepkg) mirrors it.

`avdsp upgrade` can be tried against a local server instead of GitHub:
`AUDIVANE_UPGRADE_API=http://127.0.0.1:8000` serves `releases/latest` (or
`releases/tags/<tag>`) as GitHub's JSON with `tag_name`, `html_url` and
`assets[].browser_download_url`. Only with that set,
`AUDIVANE_UPGRADE_TEST_AS_ROOT=1` lets root (a build root without sudo) run the
package step directly. Both are for testing only.

## Headless test rig (no real DAC)

On a headless machine, a virtual soundcard stands in for the DAC:

```sh
sudo modprobe snd-aloop index=10 id=Loopback pcm_substreams=4
```

- Playback `hw:Loopback,0,N` is the "DAC" (accepts 8 kHz–768 kHz, S16/S24/S32).
- Capture `hw:Loopback,1,N` returns exactly what was played on substream N, so
  tests can record the engine output and compare/hash it bit for bit.
- The loopback card runs on its own (system timer) clock, so it behaves like a
  real external DAC clock for drift testing.
- PipeWire + WirePlumber run as user services (`systemctl --user status
  pipewire wireplumber`). `pw-play`, `pw-record`, `pw-cat`, `pw-dump`,
  `pw-top` are available for driving and inspecting the graph.

## Running the daemon

```sh
cargo build --profile fastrel -p audivane-dsp-daemon -p audivane-dsp-cli
target/fastrel/audivane-dsp --foreground            # or: systemctl --user start audivane-dsp
avdsp ctl status
avdsp ctl eq set ~/eq/ParametricEQ.txt              # AutoEQ / Equalizer APO file
avdsp ctl upsample --max-rate 768000 --taps 100000  # 44.1k→705.6k, 48k→768k
avdsp ctl ab set other.txt && avdsp ctl ab b        # A/B, K-weighted level match
avdsp ctl route --app strawberry direct             # bit-perfect path for one app
avdsp ctl mode system                               # everything through Audivane DSP
avdsp ctl preset save "HD 650"                      # presets next to config.json
avdsp ctl preset import ~/eq/AutoEQ.txt --name "LCD-2"
avdsp ctl devices                                   # /proc/asound playback devices
avdsp ctl watch                                     # live meters/status
avdsp ctl selftest
```

Config lives in `$XDG_CONFIG_HOME/audivane-dsp/config.json` (minimal:
`{"device":"hw:CARD=DAC,DEV=0"}`). The socket is
`$XDG_RUNTIME_DIR/audivane-dsp.sock`. A systemd user unit is in
`packaging/audivane-dsp.service`. On exit (or on the next start after a crash)
the daemon restores PipeWire's `settings` keys and the session default sink from
`$XDG_RUNTIME_DIR/audivane-dsp/*-restore.json`.

Offline tools (no daemon needed): `avdsp process`, `render`, `eq`, `levelmatch`,
`kernel`, `nulltest`, `bypass-test`, `gen`, `bench`: see `avdsp --help`.
`avdsp upgrade` updates a packaged install (see the README).

## Rig tests (loopback card, run one at a time)

| Script | Proves |
| --- | --- |
| `tests/daemon/daemon_ctl_e2e.sh` | real daemon + `avdsp ctl`: EQ+×4 level exact, A/B level, selftest, Direct bit-perfect, release/acquire, clean shutdown |
| `tests/daemon/engine_direct.sh` | Direct sink bit-perfect at 44.1k and 48k incl. rate switch |
| `tests/daemon/engine_dsp.sh` | 60 s EQ + 65k-tap ×4: no xruns, level within 0.05 dB |
| `tests/daemon/engine_ab.sh` | gapless A/B switching with level offset |
| `tests/daemon/engine_swap_gap.sh` | live EQ edits on a long filter without gaps |
| `tests/daemon/engine_rate_follow.sh` | `MaxRate` resolved per source rate (44.1/48/96k) |
| `tests/daemon/engine_high_rates.sh` | 352.8k/384k graph rates at ×2 and 705.6k/768k at ×1 under `MaxRate(768000)`, quantum capped at 8192, level exact |
| `tests/daemon/engine_rate_conflict.sh` | 44.1k + 48k apps at once: rate settles on the first app in both orders, hands over when it stops, 2 s dwell |
| `tests/daemon/engine_direct_dsp_upsample.sh` | Direct ↔ DSP keeps upsampling across config changes |
| `tests/daemon/engine_release.sh` | 20× rapid release/acquire with WirePlumber |
| `tests/daemon/engine_device_switch.sh` | device handoff, error state, recovery |
| `tests/daemon/engine_metadata.sh` | PipeWire settings restored, also after `kill -9` |
| `tests/daemon/engine_volume.sh` | sink volume applied once (by the engine), ceiling, guard lock/limit, Direct held at unity |
| `tests/daemon/engine_xrun.sh` | forced xrun recovers |
| `tests/spike/test_*.sh` | driver spike tests (60 s drift, bit-perfect, rate switch) |
| `tests/stress/direct_start.sh` | 30x fresh-start stress for the Direct bit-perfect path (not in the suite; `tests/stress/direct_start.sh 30`) |
| `tests/reference/run.py` | DSP vs numpy/scipy (EQ, convolution, upsampling, bypass, level match) |

Run them all: `for t in tests/daemon/*.sh tests/spike/test_*.sh; do bash "$t" || break; done`.

### Gap telemetry

`engine_rig` snapshots (and the engine snapshot exposed by the daemon) carry a
`gaps` object with cumulative driver counters, all bumped with relaxed atomics on the RT thread:
ring zero-fill (`*_zero_periods` / `*_zero_frames`), ring underruns, PipeWire
short/zero input buffers per sink, frames pushed/popped/dropped/drained, ALSA
reopens/resumes/rate switches/fades, idle stops, prime/drain silence periods and
sink stream start/pause counts. The rig scripts print the last snapshot when
they fail, so a dropout can be attributed to the engine ring, the source client
or the capture path without guessing.
Unix-socket tests need a short `$TMPDIR` (socket paths are limited to 108 bytes);
use `TMPDIR=/tmp` if a sandbox sets a long one.
