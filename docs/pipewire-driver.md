# PipeWire driver mode

How the engine drives a PipeWire graph from the DAC clock: the APIs it uses, the PipeWire and ALSA behaviour it depends on, and the measurements from the original driver-mode spike (`crates/io-alsa`, `crates/io-pw`, `tests/spike/`).

## Verdict

**A pure-Rust out-of-process `pw_stream` client with `PW_STREAM_FLAG_DRIVER` works without requiring a C SPA plugin or in-process PipeWire module.**

When configured with `media.class = "Audio/Sink"`, `node.driver = "true"`, `priority.driver = "300000"`, `StreamFlags::DRIVER | StreamFlags::RT_PROCESS`, and explicit `SPA_IO_Position` / `SPA_IO_Clock` updates before `pw_stream_trigger_process()`, PipeWire schedules the entire upstream audio graph at the source sample rate paced strictly by our ALSA `hw:` DAC period wakeups, with `rate_diff = 1.0` (zero adaptive resampling, zero clock drift, zero xruns).

---

## API Investigation: `pw_stream` vs `pw_filter` vs C Module

| Approach | Driver Scheduling & Clock Control | Language / Deployment | Verdict |
| --- | --- | --- | --- |
| **`pw_stream` (`DRIVER \| RT_PROCESS`)** | Exposes server-mapped `SPA_IO_Position` (`clock`) via `io_changed`; calls `pw_stream_trigger_process()` from ALSA wakeup loop; negotiates `F32LE` port format cleanly. | Pure Rust (`pipewire` crate), out-of-process daemon. | **Chosen.** Meets every requirement cleanly. |
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
     - `period_in` is opened equal to the quantum PipeWire actually runs: `scaled_quantum` mirrors PipeWire's `node.latency` scaling, which rounds `q48 * rate / 48000` *down* to a power of two (`clock.power-of-two-quantum`, default on) and caps it at the quantum limit (8192). A 1024 quantum runs 512 at 44.1 kHz, 1024 at 88.2 kHz, 2048 at 176.4 kHz, 4096 at 352.8 kHz. Matching `period_in` to the scaled quantum avoids firing two cycles back-to-back per period.
     - When the quantum still differs from the period (`clock.force-quantum`, a client asking for a lower latency, non-default PipeWire settings), cycles are counted per quantum consumed and the extra ones are fired from the poll timeout, at least half a quantum apart (`CyclePacer`), never back to back; the start cushion holds one period plus two quanta so the ring never zero-fills across graph cycles.

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
   - Calling `org.freedesktop.RealtimeKit1.MakeThreadRealtime` requires setting `RLIMIT_RTTIME` to `RTTimeUSecMax` first. `audivane-alsa-rt` promotes to real-time priority 20 (`SCHED_RR`, what rtkit grants) and falls back gracefully if rtkit denies promotion.

---

## Spike measurements

Verified unattended against ALSA loopback (`hw:Loopback,0,0` playback -> `hw:Loopback,1,0` capture) with PipeWire 1.6.8 + WirePlumber (`tests/spike/`):

| Test | Setup | Result |
| --- | --- | --- |
| 60 s 4× upsampling (`test_driver.sh`) | 48 kHz 24-bit 1 kHz sine (`-6 dBFS`) → 192 kHz `S32_LE`, `period=1024`, `quantum=256` | `alsa_xruns=0`, `ring_underruns=0`, `pw-top ERR=0`, `max_abs_drift=256.00 frames` (1 quantum), residual `−92.9 dB`, `0.00000 rad` phase jumps |
| 24-bit bit-perfect (`test_bitperfect.sh`) | 3 s 48 kHz 24-bit deterministic white noise, 1:1 `S32_LE` | 144,000 consecutive stereo frames 100 % bit-identical in top 24 bits (`low_byte_nonzero=0`) |
| Rate switching (`test_rate.sh`) | Fixed 48 kHz, `--follow-rate` with `node.rate = "1/48000"`, and `--follow-rate` + `clock.force-rate = 44100` | Fixed/pinned cases stayed at 48 kHz (`ERR=0`, `xruns=0`); forced 44.1 kHz reopened ALSA at 176.4 kHz (`period=512`, `buffer=1536`, `quantum=128`) and locked with `xruns=0`, `drift_frames=-128.00` |

---

## Hardening and chain warm-up cost

1. **Dual-Generation `DesiredDspConfig` & Dynamic `RateTarget` Negotiation ([`lifecycle.rs`](../crates/io-pw/src/lifecycle.rs), [`engine_driver.rs`](../crates/io-pw/src/engine_driver.rs))**:
   - Runtime `apply` propagates `(dsp_enabled, rate_target, max_ratio)` with a monotonic `generation` counter (`RtControlCmd::UpdateDesiredDsp`) to the RT ALSA thread so `Direct -> DSP` transitions always recompute against the latest config rather than startup defaults.
   - `RateTarget` (`Off` / `Ratio` / `MaxRate`) is resolved dynamically against the live PipeWire graph rate on every open/reopen/acquire, e.g. `MaxRate(192000)` picks `4x` at `44.1k`/`48k` and `2x` at `96k`.
   - The engine bypass resampler (`rate.bypass_resampler: engine`) rides on the same snapshot: `DesiredDspConfig::bypass` (`BypassLeg`: on/off, the bypass mask of pairs its chain builds, the configured allowed rates as bits) and `DesiredDspConfig::bypass_landing(G, rates)`, a pure, allocation-free function of the device's native rates. The Direct and DSP-off ladder is `[O/G, ×1]` when it lands, so `open_landing` and `decide_rt_reopen` handle it unchanged.
   - Policies per device (`rate_caps::RatePolicies`): the DSP sink's (`effective_rate_policy`: its native base, what plays without the engine leg, widened as before, plus with DSP off the configured rates the engine converts), Direct's (its native rates plus the converted ones), each sink's native base (`dsp_native`, `direct_native`), and the published union (`clock.allowed-rates`, both sinks' `audio.allowed-rates` and format pods, `Status.effective_rates`). The arbiter maps each sink's clients through that sink's rates (`RateInputs::dsp_policy`/`direct_policy`, shared as one `sink_policies` snapshot) with `rate_arbiter::sink_rate`: a client at a converted rate runs the graph there, any other rate maps through the native base. `plan_graph_rate` and the follow rate do the same, so a native rate is never converted and nothing is converted twice. The control loop resends the policies (`SentRatePolicies`) when either sink's policy changes, even when the published union does not.
   - A negotiation whose resample-only chain does not build (`NegotiatedChain::bypass_unbuildable`), or whose chain the engine is still building (`bypass_pending`), goes through `lifecycle::open_with_bypass_fallback`, shared by start-up, reacquire and reopen: the pair is held back (`BypassHeld`: a failed pair until the device changes, a pending one until `BypassNotifier::designed`, either until the engine's design epoch moves on), the policies are republished without it, and the device is opened again, at a native rate (the PipeWire path), before anything resumes. Every entry point keeps the config the open ran with (`lifecycle::open_and_keep`) before it resumes: it carries a newer generation than the one the control loop held, so later updates build on it and the RT thread takes them (`lifecycle::rt_takes`). `CtrlReq::Bypass` carries the engine's `BypassNote`s (`lifecycle::bypass_note`), and republishes when a hold changes. `Designed` brings a pending pair back (or keeps a failed one out); the engine sends it only for a design the driver waits for, asked for in the current design epoch. `Epoch` says an edit of what the designs depend on (the device, the output section as the negotiated format caps it, the quantum, the bypass mode) dropped the old ones: holds from older epochs are retired (`BypassHeld::retire`; each miss carries the epoch its negotiation ran in, `NegotiatedChain::bypass_epoch`), so the policies offer the pair again and the next open asks for its new design.

2. **Serialized Release/Acquire, Device Handoff & Card Reservations ([`lifecycle.rs`](../crates/io-pw/src/lifecycle.rs))**:
   - `release_device()` only resolves and publishes `EngineState::Idle` after RT ALSA close, PipeWire teardown (`core.sync`), and `ReserveDevice1` release complete. `acquire_device()` serializes behind any pending release teardown and waits for `RtEvent::Resumed`; timeouts return errors instead of succeeding optimistically.
   - Cross-card device switches close A and release A's D-Bus reservation before reserving B; same-card substream changes (e.g. `hw:Loopback,0,0` ↔ `hw:Loopback,0,1`) keep the per-card reservation intact. Any open/reserve failure clears old device state into `EngineState::Error` without silently retaining the previous device.
   - The negotiation callback (`RatioNegotiator`) and the release hook (`ReleaseHook`) both carry the device they concern. An open the engine reports as `NegotiatedChain::superseded` (its config moved to another device while the open ran; that change is queued behind it) is closed without `Resume`: a reopen ends with `ReopenAborted` and the queued `UpdateConfig` (a device change always forces a reopen) reopens on the new device; an acquire goes back to `Idle` and is redone when that `UpdateConfig` arrives, its waiters still pending.

3. **PipeWire `settings` Metadata Ownership & Crash Recovery ([`metadata.rs`](../crates/io-pw/src/metadata.rs))**:
   - Startup waits for initial `metadata::property` events + a `core.sync(0)` barrier before writing `clock.allowed-rates` / `clock.force-rate` / `clock.force-quantum`. Originals are atomically persisted (`tmp` + `fsync` + `rename`) to `$XDG_RUNTIME_DIR/audivane-dsp/settings-restore.json` before overwriting, recovered on startup after any crash (`SIGKILL`), and only restored on teardown for keys still owned (externally edited keys are left untouched).

4. **Bounded `SynchronizedIo` Quiesce & Zero-Drop RT Stash ([`clock.rs`](../crates/io-pw/src/clock.rs), [`processor.rs`](../crates/daemon/src/engine/processor.rs))**:
   - `SynchronizedIo::quiesce_reader` bounds writer wait to 500 ms (spin -> yield) with a logged error if stalled.
   - `DaemonProcessor` splits chain swaps onto `swap_rx`, holds up to 4 retired chains in a fixed RT-owned `recycle_stash` if `recycle_tx` is full, and pauses popping `swap_rx` rather than ever deallocating on the RT thread.

5. **Parallel Chain Warm-Up & Latency Alignment CPU Cost ([`processor.rs`](../crates/daemon/src/engine/processor.rs))**:
   - Replacing a running chain runs `new_chain` in parallel with `active_chain` for `latency_out_frames + 1024` output frames (~197 ms for a 65,536-tap ×4 linear-phase upsampler at 48 -> 192 kHz) before transitioning: equal-latency chains crossfade over 10 ms; mismatched-latency chains fade old out over 5 ms -> switch at zero -> fade new in over 5 ms without comb-filtering.
   - **RT CPU budget**: Because `SwapPhase` permits at most one swap in flight, RT DSP load temporarily doubles (~2× active-chain CPU) during that warm-up window (e.g., ~2× ~12% = ~24% of one core / cycle budget for dual 65,536-tap ×4 upsampler + EQ) and drops back to 1× immediately afterward with zero heap allocation or deallocation.


## Without a device: the engine's own clock

When the DAC is gone (unplugged, or missing at start-up) the driver keeps
both sink nodes and stays the graph's driver, but nothing paces it. While
its node streams with a client, the RT thread then fires one cycle per
quantum on a monotonic deadline (`DriverClockState::next_time_ns`, the
same `advance_and_trigger` as with a DAC: `clock.rate = 1/G`, `duration =
Q`, `rate_diff = 1.0`, `delay = 0`) and drains what the sinks receive.
Clients therefore keep running at real time instead of stalling, and
nothing gives WirePlumber a reason to move them: our nodes never go away.
When the device returns, the reopen primes it as after any reopen
(`clock_state.reset()`, paced priming, fade-in), so the free-running
position does not carry over. A start-up without the device publishes the
configured rates (nothing was probed) and starts the graph at the default
rate; the first open probes and publishes the device's rates.

## Startup Priming: Why the Driver Must Not Consume Faster Than Real Time

A rare "bit-perfect mismatch after the first ~1024 frames" flake in the Direct
rig was traced to the ALSA prefill loop, not to the ring or the sink policy.

The ALSA RT thread primes the DAC buffer by triggering `n_prefill_periods`
PipeWire graph cycles (as many whole periods as the negotiated buffer holds,
at least one: `prefill_periods`), then starts ALSA DMA. Because the graph is triggered from
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
