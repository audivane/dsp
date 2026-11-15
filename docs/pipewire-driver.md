# PipeWire driver mode

How the engine drives a PipeWire graph from the DAC clock: the APIs it uses, the PipeWire and ALSA behaviour it depends on, and the measurements from the original driver-mode spike (`crates/io-alsa`, `crates/io-pw`, `tests/spike/`).

## Verdict

**A pure-Rust out-of-process `pw_stream` client with `PW_STREAM_FLAG_DRIVER` works without requiring a C SPA plugin or in-process PipeWire module.**

When configured with `media.class = "Audio/Sink"`, `node.driver = "true"`, `priority.driver = "300000"`, `StreamFlags::DRIVER | StreamFlags::RT_PROCESS`, and explicit `SPA_IO_Position` / `SPA_IO_Clock` updates before `pw_stream_trigger_process()`, PipeWire schedules the entire upstream audio graph at the source sample rate paced strictly by our ALSA `hw:` DAC period wakeups, with `rate_diff = 1.0` (zero adaptive resampling, zero clock drift, zero xruns).

---

## API Investigation: `pw_stream` vs `pw_filter` vs C Module

| Approach | Driver Scheduling & Clock Control | Language / Deployment | Verdict |
| --- | --- | --- | --- |
| **`pw_stream` (`DRIVER \| RT_PROCESS`)** | Exposes server-mapped `SPA_IO_Position` (`clock`) via `io_changed`; calls `pw_stream_trigger_process()` from ALSA wakeup loop; negotiates `F32LE` port format cleanly. | Pure Rust (`pipewire` 0.10 crate), out-of-process daemon. | **Chosen.** Meets every Phase-2/3 requirement cleanly. |
| `pw_filter` (`DRIVER \| RT_PROCESS`) | Lower-level multi-port DSP wrapper without built-in format negotiation / `audioadapter`; better suited for internal filter nodes. | Pure Rust, out-of-process. | Unnecessary complexity for a sink endpoint; keeps `audioadapter` format conversion on upstream client edge if needed. |
| C SPA node / PipeWire module | Direct SPA node callbacks inside server RT loop. | Requires C plugin packaged in `/usr/lib/pipewire-0.3/` and restarting/loading into PipeWire server. | **Not needed** (kept only as a contingency if future zero-copy/custom-protocol requirements ever arise). |

### Key PipeWire Driver Mechanics Discovered

1. **Clock Ownership (`SPA_IO_Position` & `pw_stream_trigger_process`)**:
   - In `.io_changed`, PipeWire supplies `SPA_IO_Position` (whose first member is `struct spa_io_clock clock`) and `SPA_IO_Clock`.
   - The driver owns updating `clock.nsec`, `clock.rate = { 1, rate }`, `clock.position` (monotonic cumulative frame counter), `clock.duration` (quantum frames), `clock.delay` (DAC latency in graph-rate frames = `alsa_delay / ratio`), `clock.rate_diff = 1.0`, and `clock.next_nsec`, then calling `pw_stream_trigger_process(stream)`.
   - Reading `clock.target_rate` and `clock.target_duration` uses a seqlock retry loop over `clock.target_seq` (odd bit = writer in progress).
   - Setting `rate_diff = 1.0` keeps PipeWire's `audioadapter` resampler disabled (`1:1` pass-through) whenever a client connects at the graph rate.

2. **Never Overlap PipeWire Cycles (`pw-top ERR=0`)**:
   - Calling `pw_stream_trigger_process()` back-to-back without waiting for the previous graph cycle to finish (e.g., firing multiple triggers in a tight prefill loop) causes PipeWire's profiler to count cycle misses (`pw-top ERR` climbs rapidly).
   - **Solution implemented in [`driver.rs`](../crates/io-pw/src/driver.rs)**:
     - During prefill (`!alsa_running`), each trigger waits synchronously on an `eventfd` (`data_efd`) + `cycles_completed` atomic counter before issuing the next trigger, keeping prefill strictly sequential.
     - Once running (`alsa_running`), the ALSA thread maintains 1 in-flight PipeWire cycle per ALSA `POLLOUT` period wakeup: wakeup -> write 1 `period_in * ratio` block -> advance clock & trigger next cycle. This yields `pw-top ERR=0` and constant `drift_frames = -quantum` (bounded to 1 period).

3. **Stream Pause Tail Drain**:
   - When a client stops or reaches EOF (e.g. `pw-play`), PipeWire transitions `audivane_dsp` from `Streaming` to `Paused`.
   - Dropping the ALSA PCM immediately upon seeing `!want_active` truncates the final ~4096 frames still buffered in the lock-free `rtrb` ring and ALSA DMA buffer.
   - `drain_alsa_tail()` flushes remaining `rtrb` audio into ALSA, pads 1 silence period, and waits up to 250 ms for `delay <= 0` before calling `snd_pcm_drop()`.

4. **Bit-Exact `F32LE` -> `i32` / `S24` Conversion**:
   - PipeWire converts 24-bit integer sources into normalized `f32` via `x / 8_388_608.0` (`2^23`), which is exact in IEEE-754 single precision across the full signed 24-bit range `[-8_388_608, 8_388_607]`.
   - `f32_to_left_justified_i32()` multiplies by `2_147_483_648.0` (`2^31`) in `f64`, rounds, and clamps to `[i32::MIN, i32::MAX]`, recovering `x << 8` with zero LSB error.

---

## ALSA `hw:`, `ReserveDevice1`, and `rtkit` Findings

1. **`ReserveDevice1` Race with WirePlumber (~1 ms re-probe window)**:
   - When WirePlumber owns `org.freedesktop.ReserveDevice1.Audio10` and receives `RequestRelease(1000)`, it emits `true`, releases its D-Bus name, and re-probes/re-acquires the name within ~1 ms if no new owner claims it immediately.
   - Calling `RequestName(..., DO_NOT_QUEUE | ALLOW_REPLACEMENT | REPLACE_EXISTING)` immediately after `RequestRelease` returns `true` (with zero pre-sleep), followed by a 50 ms settle pause + `EBUSY` retry loop on `snd_pcm_open`, cleanly hands ownership from WirePlumber to Audivane and restores it to WirePlumber on `Drop`.
   - Note: `zbus` v6 removed the `zbus::blocking` module; `crates/io-alsa` uses `zbus::block_on(async { ... })` cleanly on setup/teardown threads (never on RT threads).

2. **ALSA Manual Start (`start_threshold`)**:
   - Setting `sw_params.set_start_threshold(buffer_frames)` causes ALSA to auto-start on the final prefill write, making an explicit `snd_pcm_start()` return `EBADFD (-77)`.
   - Fix in [`pcm.rs`](../crates/io-alsa/src/pcm.rs): `swp.set_start_threshold(swp.get_boundary()?)`, plus guarding `AlsaOutput::start()` on `self.pcm.state() != State::Running`.

3. **Real-Time Priority via `rtkit`**:
   - Calling `org.freedesktop.RealtimeKit1.MakeThreadRealtime` requires setting `RLIMIT_RTTIME` to `RTTimeUSecMax` first. On this host, `audivane-alsa-rt` cleanly promotes to `SCHED_FIFO` priority 20 and falls back gracefully if rtkit denies promotion.

---

## Spike measurements

All tests were executed unattended against ALSA loopback card 10 (`hw:Loopback,0,0` playback -> `hw:Loopback,1,0` capture) with PipeWire 1.6.8 + WirePlumber running as user services.

### 1. Unit Tests & Lints (`audivane-io-alsa` & `audivane-io-pw`)
- `cargo test -p audivane-io-alsa -p audivane-io-pw`: **7/7 passed** (6 in `io-alsa` covering `S32`, `S24_LE`, `S24_3LE`, `S16_LE` packing and card parsing; 1 in `io-pw` verifying `f32 -> left-justified i32` bit-exactness across 24-bit boundaries and strided full-range sweep).
- `cargo clippy -p audivane-io-alsa -p audivane-io-pw --all-targets -- -D warnings`: **0 warnings / 0 errors**.

### 2. 60-Second Driver-Mode Upsampling Stability (`tests/spike/test_driver.sh`)
- **Command**: `driver_spike --device hw:Loopback,0,0 --rate 48000 --ratio 4 --format s32 --period 1024 --seconds 64`
- **Source**: 60s 48 kHz 24-bit stereo 1 kHz sine (`-6 dBFS`) played via `pw-play --target audivane_dsp --volume=1.0`, upsampled 4× (`SpikeUpsampler`, 16-tap Blackman-windowed polyphase sinc) to `192 kHz` `S32_LE`, captured via `arecord -D hw:Loopback,1,0 -f S32_LE -r 192000`.
- **Measured Results**:
  - Telemetry: `rx_frames=2880512`, `tx_frames=11521024`, `quantum=256`, `graph_rate=48000`
  - Errors/Underruns: `alsa_xruns=0`, `ring_underruns=0`, `pw-top ERR=0`
  - Drift bound: `max_abs_drift=256.00 frames` (exactly 1 PipeWire quantum = 1 ALSA `period_in`, `period_out/ratio = 1024/4`)
  - Captured L channel: `duration=59.60s`, `rms_med=0.35438` (`0.35438..0.35438`), `max_residual=-92.9dB`, `max_phase_jump=0.00000rad`
  - Captured R channel: `duration=59.60s`, `rms_med=0.35438` (`0.35438..0.35438`), `max_residual=-92.9dB`, `max_phase_jump=0.00000rad`

### 3. 24-Bit Bit-Perfect 1:1 Passthrough (`tests/spike/test_bitperfect.sh`)
- **Command**: `driver_spike --device hw:Loopback,0,0 --rate 48000 --ratio 1 --format s32 --period 1024 --seconds 7`
- **Source**: 3s 48 kHz 24-bit deterministic white-noise WAV (`144,000` stereo frames) played via `pw-play --target audivane_dsp --volume=1.0` with sink volume `1.0` and unmuted.
- **Measured Result**:
  - `144000 consecutive stereo frames (288000 samples) 100% bit-identical in top 24 bits of S32_LE (offset=26800 frames, low_byte_nonzero=0)`

### 4. Sample-Rate Switching & Resampling (`tests/spike/test_rate.sh`)
- **Case 1 (Fixed 48 kHz driver + 44.1 kHz stream)**:
  - `pw-top`: `audivane_dsp` `QUANT=256 RATE=48000 ERR=0`, `pw-play` resampled by PipeWire (`44100 -> 48000`, `ERR=0`), `xruns=0`, `ring_underruns=0`, `drift_frames=-256.00`.
- **Case 2 (`--follow-rate` + `clock.allowed-rates = [44100 48000 88200 96000 176400 192000]`, no `clock.force-rate`)**:
  - Pinning `node.rate = "1/48000"` on a sink keeps PipeWire's graph rate at `48000` when `48000` is already in `clock.allowed-rates` (`pw-play` resamples `44100 -> 48000`, `ERR=0`, `xruns=0`, `drift_frames=-256.00`).
- **Case 3 (`--follow-rate` + `clock.force-rate = 44100`)**:
  - PipeWire switched `audivane_dsp` to `RATE=44100 QUANT=128` (`F32LE 2 44100`).
  - `audivane-io-pw` detected `target_rate=44100`, dropped and reopened `hw:Loopback,0,0` at `176400 Hz` (`44100 * 4`) with `period=512 buffer=1536` (matching `target_quantum * ratio = 128 * 4`), achieving `xruns=0` and `drift_frames=-128.00` locked across playback.
  - Note: a single startup `ring_underruns=1` and `pw-top ERR=2` occurred during the initial reopen transition before settling cleanly.

---

## Hardening and chain warm-up cost

1. **Dual-Generation `DesiredDspConfig` & Dynamic `RateTarget` Negotiation ([`lifecycle.rs`](../crates/io-pw/src/lifecycle.rs), [`engine_driver.rs`](../crates/io-pw/src/engine_driver.rs))**:
   - Runtime `apply` propagates `(dsp_enabled, rate_target, max_ratio)` with a monotonic `generation` counter (`RtControlCmd::UpdateDesiredDsp`) to the RT ALSA thread so `Direct -> DSP` transitions always recompute against the latest config rather than startup defaults.
   - `RateTarget` (`Off` / `Ratio` / `MaxRate`) is resolved dynamically against the live PipeWire graph rate on every open/reopen/acquire, e.g. `MaxRate(192000)` picks `4x` at `44.1k`/`48k` and `2x` at `96k`.

2. **Serialized Release/Acquire, Device Handoff & Card Reservations ([`lifecycle.rs`](../crates/io-pw/src/lifecycle.rs))**:
   - `release_device()` only resolves and publishes `EngineState::Idle` after RT ALSA close, PipeWire teardown (`core.sync`), and `ReserveDevice1` release complete. `acquire_device()` serializes behind any pending release teardown and waits for `RtEvent::Resumed`; timeouts return errors instead of succeeding optimistically.
   - Cross-card device switches close A and release A's D-Bus reservation before reserving B; same-card substream changes (e.g. `hw:Loopback,0,0` ↔ `hw:Loopback,0,1`) keep the per-card reservation intact. Any open/reserve failure clears old device state into `EngineState::Error` without silently retaining the previous device.

3. **PipeWire `settings` Metadata Ownership & Crash Recovery ([`metadata.rs`](../crates/io-pw/src/metadata.rs))**:
   - Startup waits for initial `metadata::property` events + a `core.sync(0)` barrier before writing `clock.allowed-rates` / `clock.force-rate` / `clock.force-quantum`. Originals are atomically persisted (`tmp` + `fsync` + `rename`) to `$XDG_RUNTIME_DIR/audivane-dsp/settings-restore.json` before overwriting, recovered on startup after any crash (`SIGKILL`), and only restored on teardown for keys still owned (externally edited keys are left untouched).

4. **Bounded `SynchronizedIo` Quiesce & Zero-Drop RT Stash ([`clock.rs`](../crates/io-pw/src/clock.rs), [`processor.rs`](../crates/daemon/src/engine/processor.rs))**:
   - `SynchronizedIo::quiesce_reader` bounds writer wait to 500 ms (spin -> yield) with a logged error if stalled.
   - `DaemonProcessor` splits chain swaps onto `swap_rx`, holds up to 4 retired chains in a fixed RT-owned `recycle_stash` if `recycle_tx` is full, and pauses popping `swap_rx` rather than ever deallocating on the RT thread.

5. **Parallel Chain Warm-Up & Latency Alignment CPU Cost ([`processor.rs`](../crates/daemon/src/engine/processor.rs))**:
   - Replacing a running chain runs `new_chain` in parallel with `active_chain` for `latency_out_frames + 1024` output frames (~197 ms for a 65,536-tap ×4 linear-phase upsampler at 48 -> 192 kHz) before transitioning: equal-latency chains crossfade over 10 ms; mismatched-latency chains fade old out over 5 ms -> switch at zero -> fade new in over 5 ms without comb-filtering.
   - **RT CPU budget**: Because `SwapPhase` permits at most one swap in flight, RT DSP load temporarily doubles (~2× active-chain CPU) during that warm-up window (e.g., ~2× ~12% = ~24% of one core / cycle budget for dual 65,536-tap ×4 upsampler + EQ) and drops back to 1× immediately afterward with zero heap allocation or deallocation.


## Startup Priming: Why the Driver Must Not Consume Faster Than Real Time

A rare "bit-perfect mismatch after the first ~1024 frames" flake in the Direct
rig was traced to the ALSA prefill loop, not to the ring or the sink policy.

The ALSA RT thread primes the DAC buffer by triggering `n_prefill_periods`
PipeWire graph cycles, then starts ALSA DMA. Because the graph is triggered from
the RT thread's own loop, those cycles complete as fast as the graph can run
(observed: three 512-frame cycles in ~130 us). The source client (e.g.
`pw-play`) is a *follower* with its own converter and a larger latency (its node
quantum is typically ~100 ms). Consuming several quanta in a burst outruns the
converter; PipeWire then hands the sink an all-zero block for one cycle, which
lands in the DAC output as a block of silence right after playback starts. The
RT thread's own counters stay clean, which is why this looked like a ring or
reopen bug: the zeros were produced *upstream* of the sink ring.

Fix: pace the priming cycles at the nominal graph rate (`period_in / rate`)
using a monotonic deadline, and pace the cushion cycle as well. The DAC starts
~`n_prefill_periods` quanta later, but every primed cycle has real source data.

Telemetry kept for this class of bug (see `DriverGapCounters`):
`direct_zero_buffers` / `dsp_zero_buffers` count all-zero PipeWire blocks while
a sink has links; `direct_pushed_frames` vs `direct_popped_frames` and
`direct_drained_prime` / `_idle` / `_unused` show whether the engine or an
upstream client lost the data; `stream_starts_*` / `stream_pauses_*` and
`reopens` / `resumes` / `rate_switches` cover lifecycle transitions.

Repro: `tests/stress/direct_start.sh 30` (fresh rig + 44.1 kHz rate switch +
Direct playback per iteration, under CPU load). With the unmodified prefill the
flake appeared in 3 of 3 baseline runs, 13-14 fresh starts in (~7% per start;
experimental variants that only changed tempo/link timing failed in every run).
With the paced prefill, 70 of 70 starts and two full rig-suite passes were
bit-perfect.
