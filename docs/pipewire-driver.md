# PipeWire driver mode

How the engine drives a PipeWire graph from the DAC clock: the APIs it uses,
the PipeWire and ALSA behaviour it depends on, and the measurements from the
driver-mode spike (`crates/io-alsa`, `crates/io-pw`, `tests/spike/`).

## Verdict

A pure-Rust out-of-process `pw_stream` client with `PW_STREAM_FLAG_DRIVER`
works; no C SPA plugin or in-process PipeWire module is needed.

With `media.class = "Audio/Sink"`, `node.driver = "true"`, `priority.driver =
"300000"`, `StreamFlags::DRIVER | StreamFlags::MAP_BUFFERS |
StreamFlags::RT_PROCESS`, and explicit `SPA_IO_Position` / `SPA_IO_Clock`
updates before `pw_stream_trigger_process()`, PipeWire schedules the upstream
graph at the source sample rate, paced by our ALSA `hw:` DAC period wakeups,
with `rate_diff = 1.0`: no adaptive resampling and no clock drift.

## API investigation: `pw_stream` vs `pw_filter` vs C module

| Approach | Driver scheduling and clock control | Language and deployment | Verdict |
| --- | --- | --- | --- |
| `pw_stream` (`DRIVER \| MAP_BUFFERS \| RT_PROCESS`) | Exposes server-mapped `SPA_IO_Position` (`clock`) via `io_changed`, calls `pw_stream_trigger_process()` from the ALSA wakeup loop, negotiates an `F32LE` port format. | Pure Rust (`pipewire` crate), out-of-process daemon. | Chosen. |
| `pw_filter` (`DRIVER \| RT_PROCESS`) | Lower-level multi-port DSP wrapper without format negotiation / `audioadapter`; suited to internal filter nodes. | Pure Rust, out-of-process. | More complexity than a sink endpoint needs; keeps `audioadapter` conversion on the upstream client edge. |
| C SPA node / PipeWire module | Direct SPA node callbacks inside the server's RT loop. | Needs a C plugin in `/usr/lib/pipewire-0.3/` and a restart or load into the PipeWire server. | Not needed; kept as a contingency for zero-copy or a custom protocol. |

### PipeWire driver mechanics

1. **Clock ownership (`SPA_IO_Position` and
   `pw_stream_trigger_process`).** In `io_changed`, PipeWire supplies
   `SPA_IO_Position` (whose first member is `struct spa_io_clock clock`) and
   `SPA_IO_Clock`. The driver updates `clock.nsec`, `clock.rate = { 1, rate }`,
   `clock.position` (a monotonic cumulative frame counter), `clock.duration`
   (quantum frames), `clock.delay` (DAC latency in graph-rate frames =
   `alsa_delay / ratio`), `clock.rate_diff = 1.0` and `clock.next_nsec`, then
   calls `pw_stream_trigger_process(stream)`.
   Reading `clock.target_rate` and `clock.target_duration` uses a seqlock
   retry loop over `clock.target_seq` (odd bit = writer in progress).
   `rate_diff = 1.0` keeps PipeWire's `audioadapter` resampler at 1:1 whenever
   a client connects at the graph rate.

2. **Never overlap PipeWire cycles (`pw-top ERR=0`).** Calling
   `pw_stream_trigger_process()` back to back without waiting for the previous
   graph cycle (for example in a tight prefill loop) makes PipeWire's profiler
   count cycle misses (`pw-top ERR` climbs).
   [`driver.rs`](../crates/io-pw/src/driver.rs) handles this:
   - During prefill (`!alsa_running`), each trigger waits synchronously on an
     `eventfd` (`data_efd`) and a `cycles_completed` atomic before the next
     one, so prefill stays sequential.
   - Once running (`alsa_running`), the ALSA thread keeps one in-flight
     PipeWire cycle per ALSA `POLLOUT` period wakeup: wakeup, write one
     `period_in * ratio` block, advance the clock, trigger the next cycle.
     This gives `pw-top ERR=0` and a constant `drift_frames = -quantum`,
     bounded to one period.
   - `period_in` is opened equal to the quantum PipeWire actually runs:
     `scaled_quantum` mirrors PipeWire's `node.latency` scaling, which rounds
     `q48 * rate / 48000` down to a power of two (`clock.power-of-two-quantum`,
     default on) and caps it at the quantum limit (8192). A 1024 quantum runs
     512 at 44.1 kHz, 1024 at 88.2 kHz, 2048 at 176.4 kHz and 4096 at
     352.8 kHz. Matching `period_in` to the scaled quantum avoids firing two
     cycles per period.
   - When the quantum still differs from the period (`clock.force-quantum`, a
     client asking for lower latency, non-default settings), cycles are
     counted per quantum consumed and the extra ones are fired from the poll
     timeout, at least half a quantum apart (`CyclePacer`), never back to
     back. The start cushion holds one period plus two quanta, so the ring
     never zero-fills across graph cycles.

3. **Stream pause tail drain.** When a client stops or reaches EOF (for
   example `pw-play`), PipeWire moves `audivane_dsp` from `Streaming` to
   `Paused`. Dropping the ALSA PCM at `!want_active` would truncate the final
   ~4096 frames still buffered in the `rtrb` ring and the ALSA DMA buffer.
   `drain_alsa_tail()` in `engine_driver.rs` flushes the remaining ring audio
   to ALSA, then feeds silence through the DSP for what processing still holds
   back: the varispeed stage's tail, the multi-device alignment line and the
   chain's own tail (`chain_tail_frames`, from `Chain::tail_out_frames`: the
   upsampler's, convolution's and limiter lookahead's latency plus a
   convolution IR's whole length, since a room IR's peak can sit far into it).
   Without it, a stream's final stretch, up to the filter's latency (371 ms
   for `block_fft`), would never play and would stop at full level. It then
   pads silence until the DAC has played everything and calls
   `snd_pcm_drop()`. The deadline covers the ring, the tails and the device's
   delay (at least 250 ms).
   The drain ends under a 5 ms fade (`TailCut`):
   - over its last frames (the ring's last frames when the chain delays less
     than that), so a stream stopped mid-music never steps from full level to
     silence. A stream that ends in silence is unchanged, as is a path with no
     chain delay (Direct, DSP off at 1:1).
   - at once, during the flush, when a stream arrives (any sink wants the DAC
     again) or a device release is requested, so neither waits for the tail.
   The flush is not capped: the chain's delay holds the stream's last stretch
   of music (a `block_fft_large` tail is 1.5 s at ×16 and 11.9 s at ×2).
   Volume, balance and mute changes still apply during it (each flushed block
   syncs the volume), and the deadline bounds it. After a fade the device
   stops as soon as the fade has played. A new stream then starts as usual:
   it waits about one device buffer, never for the rest of the tail.

4. **Bit-exact `F32LE` to `i32` / `S24` conversion.** PipeWire converts 24-bit
   integer sources into normalized `f32` via `x / 8_388_608.0` (`2^23`), which
   is exact in IEEE-754 single precision across the signed 24-bit range
   `[-8_388_608, 8_388_607]`. `f32_to_left_justified_i32()` multiplies by
   `2_147_483_648.0` (`2^31`) in `f64`, rounds and clamps to `[i32::MIN,
   i32::MAX]`, recovering `x << 8` with zero LSB error.

## ALSA `hw:`, `ReserveDevice1` and `rtkit`

1. **`ReserveDevice1` race with WirePlumber (~1 ms re-probe window).** When
   WirePlumber owns `org.freedesktop.ReserveDevice1.Audio10` and receives
   `RequestRelease(1000)`, it emits `true`, releases its D-Bus name, and
   re-probes and re-acquires the name within ~1 ms if no new owner claims it
   immediately. Calling `RequestName(..., DO_NOT_QUEUE | ALLOW_REPLACEMENT |
   REPLACE_EXISTING)` immediately after `RequestRelease` returns `true` (no
   pre-sleep), followed by a 50 ms settle pause and an `EBUSY` retry loop on
   `snd_pcm_open`, hands ownership from WirePlumber to Audivane, and
   `Drop` restores it. `zbus` 6 has no `zbus::blocking` module; `crates/io-alsa`
   uses `zbus::block_on(async { ... })` on setup and teardown threads, never on
   RT threads.

2. **ALSA manual start (`start_threshold`).** Setting
   `sw_params.set_start_threshold(buffer_frames)` makes ALSA auto-start on the
   final prefill write, after which an explicit `snd_pcm_start()` returns
   `EBADFD (-77)`. [`pcm.rs`](../crates/io-alsa/src/pcm.rs) sets
   `swp.set_start_threshold(swp.get_boundary()?)` and guards
   `AlsaOutput::start()` on `self.pcm.state() != State::Running`.

3. **Real-time priority via `rtkit`.** Calling
   `org.freedesktop.RealtimeKit1.MakeThreadRealtime` requires setting
   `RLIMIT_RTTIME` to `RTTimeUSecMax` first. `audivane-alsa-rt` promotes to
   real-time priority 20 (`SCHED_RR`, what rtkit grants) and falls back to
   normal scheduling when rtkit denies it.

---

## Spike measurements

Verified unattended against ALSA loopback (`hw:Loopback,0,0` playback, then
`hw:Loopback,1,0` capture) with PipeWire 1.6.8 and WirePlumber
(`tests/spike/`):

| Test | Setup | Result |
| --- | --- | --- |
| 60 s 4× upsampling (`test_driver.sh`) | 48 kHz 24-bit 1 kHz sine (`-6 dBFS`) to 192 kHz `S32_LE`, `period=1024`, `quantum=256` | `alsa_xruns=0`, `ring_underruns=0`, `pw-top ERR=0`, `max_abs_drift=256.00 frames` (1 quantum), residual `−92.9 dB`, `0.00000 rad` phase jumps |
| 24-bit bit-perfect (`test_bitperfect.sh`) | 3 s 48 kHz 24-bit deterministic white noise, 1:1 `S32_LE` | 144,000 consecutive stereo frames 100 % bit-identical in the top 24 bits (`low_byte_nonzero=0`) |
| Rate switching (`test_rate.sh`) | Fixed 48 kHz, `--follow-rate` with `node.rate = "1/48000"`, and `--follow-rate` with `clock.force-rate = 44100` | Fixed and pinned cases stayed at 48 kHz (`ERR=0`, `xruns=0`); forced 44.1 kHz reopened ALSA at 176.4 kHz (`period=512`, `buffer=1536`, `quantum=128`) and locked with `xruns=0`, `drift_frames=-128.00` |

## Hardening and chain warm-up cost

1. **`DesiredDspConfig` generations and dynamic `RateTarget`**
   ([`lifecycle.rs`](../crates/io-pw/src/lifecycle.rs),
   [`engine_driver.rs`](../crates/io-pw/src/engine_driver.rs)). Runtime
   `apply` propagates `(dsp_enabled, rate_target, max_ratio)` with a monotonic
   `generation` counter (`RtControlCmd::UpdateDesiredDsp`) to the RT ALSA
   thread, so Direct to DSP transitions always recompute against the latest
   config, not startup defaults. `RateTarget` (`Off`, `Ratio`, `MaxRate`,
   `Rate`, `MaxRateAny`) is resolved against the live PipeWire graph rate on
   every open, reopen and acquire: for example `MaxRate(192000)` picks 4× at
   44.1 and 48 kHz and 2× at 96 kHz.
   The engine bypass resampler (`rate.bypass_resampler: engine`) rides on the
   same snapshot: `DesiredDspConfig::bypass` (`BypassLeg`: on/off, the bypass
   mask of pairs its chain builds, the configured allowed rates as bits) and
   `DesiredDspConfig::bypass_landing(G, rates)`, a pure allocation-free
   function of the device's native rates. The Direct and DSP-off ladder is
   `[O/G, ×1]` when it lands, so `open_landing` and `decide_rt_reopen` handle
   it unchanged.
   Policies per device (`rate_caps::RatePolicies`): the DSP sink's
   (`effective_rate_policy`: its native base, widened to every playable rate
   when none of the configured ones plays, plus with DSP off the configured
   rates the engine converts), Direct's (its native rates plus the converted
   ones), each sink's native base (`dsp_native`, `direct_native`) and the
   published union (`clock.allowed-rates`, both sinks' `audio.allowed-rates`
   and format pods, `Status.effective_rates`). The arbiter maps each sink's
   clients through that sink's rates (`RateInputs::dsp_policy` /
   `direct_policy`, shared as one `sink_policies` snapshot) with
   `rate_arbiter::sink_rate`: a client at a converted rate runs the graph
   there, and any other rate maps through the native base. `plan_graph_rate`
   and the follow rate do the same, so a native rate is never converted and
   nothing is converted twice. The control loop resends the policies
   (`SentRatePolicies`) when either sink's policy changes, even when the
   published union does not.
   A negotiation whose resample-only chain does not build
   (`NegotiatedChain::bypass_unbuildable`), or whose chain the engine is
   still building (`bypass_pending`), goes through
   `lifecycle::open_with_bypass_fallback`, shared by start-up, reacquire and
   reopen. The pair is held back (`BypassHeld`: a failed pair until the
   device changes, a pending one until `BypassNotifier::designed`, either
   until the engine's design epoch moves on), the policies are republished
   without it, and the device opens again at a native rate (the PipeWire
   path) before anything resumes. Every entry point keeps the config the open
   ran with (`lifecycle::open_and_keep`) before it resumes: it carries a
   newer generation than the one the control loop held, so later updates
   build on it and the RT thread takes them (`lifecycle::rt_takes`).
   `CtrlReq::Bypass` carries the engine's `BypassNote`s
   (`lifecycle::bypass_note`) and republishes when a hold changes. `Designed`
   brings a pending pair back (or keeps a failed one out); the engine sends
   it only for a design the driver waits for, asked for in the current design
   epoch. `Epoch` says an edit of what the designs depend on (the device, the
   output section as the negotiated format caps it, the quantum, the bypass
   mode) dropped the old ones: holds from older epochs are retired
   (`BypassHeld::retire`; each miss carries the epoch its negotiation ran in,
   `NegotiatedChain::bypass_epoch`), so the policies offer the pair again and
   the next open asks for its new design.

2. **Serialized release/acquire, device handoff and card reservations**
   ([`lifecycle.rs`](../crates/io-pw/src/lifecycle.rs)). `release_device()`
   publishes `EngineState::Idle` only after the RT ALSA close, the PipeWire
   teardown (`core.sync`) and the `ReserveDevice1` release complete.
   `acquire_device()` serializes behind any pending release teardown and waits
   for `RtEvent::Resumed`; timeouts return errors.
   Cross-card device switches close A and release A's D-Bus reservation before
   reserving B; same-card substream changes (for example `hw:Loopback,0,0` to
   `hw:Loopback,0,1`) keep the per-card reservation. An open or reserve
   failure clears the old device state into `EngineState::Error`; the previous
   device is not retained.
   The negotiation callback (`RatioNegotiator`) and the release hook
   (`ReleaseHook`) both carry the device they concern. An open the engine
   reports as `NegotiatedChain::superseded` (its config moved to another
   device while the open ran, and that change is queued behind it) is closed
   without `Resume`: a reopen ends with `ReopenAborted` and the queued
   `UpdateConfig` (a device change always forces a reopen) reopens on the new
   device; an acquire goes back to `Idle` and is redone when that
   `UpdateConfig` arrives, its waiters still pending.

3. **PipeWire `settings` metadata ownership and crash recovery**
   ([`metadata.rs`](../crates/io-pw/src/metadata.rs)). Startup waits for
   initial `metadata::property` events and a `core.sync(0)` barrier before
   writing `clock.allowed-rates`, `clock.force-rate` and
   `clock.force-quantum`. Originals are persisted atomically (`tmp`, `fsync`,
   `rename`) to `$XDG_RUNTIME_DIR/audivane-dsp/settings-restore.json` before
   they are overwritten, recovered on startup after a crash (`SIGKILL`), and
   restored on teardown only for keys still owned; externally edited keys are
   left untouched.

4. **Bounded `SynchronizedIo` quiesce and zero-drop RT stash**
   ([`clock.rs`](../crates/io-pw/src/clock.rs),
   [`processor.rs`](../crates/daemon/src/engine/processor.rs)).
   `SynchronizedIo::quiesce_reader` bounds the writer wait to 500 ms (spin,
   then yield) and logs an error if it stalls. `DaemonProcessor` splits chain
   swaps onto `swap_rx`, holds up to 4 retired chains in a fixed RT-owned
   `recycle_stash` when `recycle_tx` is full, and pauses popping `swap_rx`
   rather than deallocating on the RT thread.

5. **Parallel chain warm-up and latency alignment CPU cost**
   ([`processor.rs`](../crates/daemon/src/engine/processor.rs)). Replacing a
   running chain runs `new_chain` in parallel with `active_chain` for
   `latency_out_frames + 1024` output frames (~197 ms for a 65,536-tap ×4
   linear-phase upsampler at 48 to 192 kHz) before switching.
   An edit before an unchanged convolution skips that: the new chain takes
   `active_chain` over in place (`ActiveChain::take_over_upstream`: its
   convolver and later stages move in, the edit fades in at the convolution's
   input, the fade as long as the IR's fade bound asks) and replaces it at
   once, with no second chain running. When no fade up to 500 ms keeps the
   bound, it warms up as below. A convolution whose filter and input the edit
   keeps is handed over from `active_chain` as the warm-up starts
   (`ActiveChain::inherit_filter_state`: a copy of the convolver state and the
   stages before it, at most a quarter of the period at 2 GB/s); otherwise
   (an EQ edit, a new IR, a state over that budget) the new chain warms up for
   its IR's ring too (at most 1 s). Equal-latency chains crossfade over 10 ms;
   mismatched-latency chains fade the old one out over 5 ms, switch at zero,
   and fade the new one in over 5 ms without comb filtering.
   `SwapPhase` permits at most one swap in flight, so RT DSP load doubles
   during the warm-up window (about 2× the active chain's CPU, for example 2×
   12 % = 24 % of one core for a dual 65,536-tap ×4 upsampler with EQ) and
   drops back to 1× afterwards, with no heap allocation or deallocation.

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

## Startup priming

The ALSA RT thread primes the DAC buffer by triggering `n_prefill_periods`
PipeWire graph cycles (as many whole periods as the negotiated buffer holds,
at least one: `prefill_periods`), then starts ALSA DMA. Because the RT
thread's own loop triggers the graph, those cycles complete as fast as the
graph can run (three 512-frame cycles in ~130 µs). The source client (for
example `pw-play`) is a follower with its own converter and a larger latency
(its node quantum is typically ~100 ms). Consuming several quanta in a burst
outruns the converter; PipeWire then hands the sink an all-zero block for one
cycle, which lands in the DAC output as a block of silence right after
playback starts, as a Direct bit-perfect mismatch. The RT thread's own
counters stay clean: the zeros come from upstream of the sink ring.

The fix is to pace the priming cycles at the nominal graph rate
(`period_in / rate`) on a monotonic deadline, and to pace the cushion cycle
too. The DAC starts `n_prefill_periods` quanta later, but every primed cycle
has real source data.

Telemetry for this class of bug (`DriverGapCounters`): `direct_zero_buffers`
and `dsp_zero_buffers` count all-zero PipeWire blocks while a sink has links;
`direct_pushed_frames` against `direct_popped_frames` and
`direct_drained_prime` / `_idle` / `_unused` show whether the engine or an
upstream client lost the data; `stream_starts_*` / `stream_pauses_*` and
`reopens` / `resumes` / `rate_switches` cover lifecycle transitions.

Repro: `tests/stress/direct_start.sh 30` (fresh rig, 44.1 kHz rate switch and
Direct playback per iteration, under CPU load).
