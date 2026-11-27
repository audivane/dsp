# Audivane DSP: architecture

Linux with PipeWire only. How to build and test is in [DEV.md](../DEV.md); the
wire protocol is in [control-plane.md](control-plane.md); how the engine drives
PipeWire from the DAC clock is in [pipewire-driver.md](pipewire-driver.md).

A desktop app with a background engine that applies DSP to system-wide or
single-app audio. With processing off, its output is bit-perfect, and the app
checks and shows that.

## Goals and scope

| Feature | Requirement |
| --- | --- |
| Input | System-wide (default sink), or only chosen apps (e.g. a music player) |
| EQ | Parametric; imports AutoEQ / Equalizer APO `ParametricEQ.txt` |
| Convolution | Long FIR / room-correction impulse responses |
| Precision | f64 inside the engine, quantized once at the very end |
| Bypass | Bit-perfect output, checked and shown in the UI |
| Sample rate | DAC follows the source rate (44.1k and 48k families) |
| Clock | No drift; the DAC clock paces everything |
| Upsampling | Built in; linear, minimum or blended phase, up to 8M taps |
| A/B | Compare two DSP chains with K-weighted, never-boost level matching; blind tests in the app |
| UI | Desktop app + tray; engine keeps running when the UI is closed |

Out of scope: Windows/macOS, DSD output, a raw 32-bit-integer input path,
GPU filters.

## Decision: the engine owns the DAC and drives the graph

The engine opens the DAC directly through ALSA `hw:` and exposes PipeWire
sinks that it drives, timed by the DAC's buffer position.

Why: **a PipeWire graph runs at one sample rate per clock driver.** With an
ordinary filter node, a 44.1 kHz player feeding a DAC running at 176.4 kHz is
resampled by PipeWire *before* our DSP sees it, so our upsampler never gets
the original samples.

| Option | Upsampling | Drift | f64 to the last sample | Decision |
| --- | --- | --- | --- | --- |
| Filter node inside PipeWire's graph | No: graph resamples first | None | No: f32 bus to the DAC | Rejected |
| Two clocks (virtual sink + DAC with async resampling) | Yes | Corrected by adaptive async resampling | Yes | Rejected: permanent async resampler in the path |
| **Engine is the driver** | Yes | None: DAC clock paces the graph | Yes, plus our own dither | **Chosen** |

What this gives us:

- The graph runs at the source rate; the engine writes to ALSA at source × N
  on integer ratios up to ×64 (44.1 → 176.4 / 352.8 / 705.6 / 1411.2 /
  2822.4, 48 → 192 / 384 / 768 / 1536 / 3072 kHz).
- Integer output (S16, S24, S24_3 or S32) with our own dither, applied exactly
  once.
- True bypass: samples copied to ALSA unchanged.
- Rate following: reopen the ALSA device with new hw params when the source
  rate changes.
- PipeWire releases the DAC through `org.freedesktop.ReserveDevice1`, the same
  D-Bus protocol JACK uses.

Cost: while the engine runs, it holds the DAC exclusively. In system-wide mode
that's fine, since all audio goes through our sinks anyway. `release_device`
hands it back to WirePlumber on demand.

The pure-Rust `pw_stream` driver this needs works without a C SPA plugin or
an in-process PipeWire module; see [pipewire-driver.md](pipewire-driver.md).

## Signal flow

```
 Sources              PipeWire graph            Engine (f64)                 DAC
                      (source rate)

 System audio ──────► Audivane DSP ───────────► DC block
 (browsers, games)    (f32 in)                    │
                          ▲                       ▼
 Music player ────────────┤                     Pitch shift, varispeed
 (routed per app)         │                       │
        │                 │                       ▼
        │                 │                     Stereo image (crossfeed, width)
        │                 │                       │
        │                 │                       ▼
        │                 │                     EQ (f64 biquads)
        │                 │                       │
        │                 │                       ▼
        │                 │                     Convolution (FFT)
        │                 │                       │
        │                 │                       ▼
        │                 │                     Upsampler ×N
        │                 │                       │
        │                 │                       ▼
        │                 │                     Harmonics
        │                 │                       │
        │                 │                       ▼
        │                 │                     Volume/balance → Limiter
        │                 │                       │
        │                 │                       ▼
        │                 │                     Dither, once ──────────► ALSA hw:
        │                 │                                             rate × N
        └───────────────► Audivane Direct ────► Bypass: exact copy ───► S16/S24/
                          (no DSP)            (digital volume, if         S24_3/S32
                                               any, + dither)              │
                              ▲                                            │
                              └──── DAC buffer position drives each ───────┘
                                    graph cycle (one clock, no drift)
```

Every processing stage can be switched off. The DSP path always has the final
volume/balance, quantizer and dither; the Direct sink has none of the chain.

## Peak protection

The linked limiter lives in `dsp-core::limiter`, after digital volume/balance
and immediately before the shared quantizer, so volume attenuation provides
headroom before limiting. A/B runs one limiter after mixing and level
matching, and both sides share its config. True-peak detection interpolates
only the sidechain, at a density set by the output rate family (8 points per
44.1/48 kHz sample, the output samples alone from ×8, 352.8 or 384 kHz). At
unity gain the audio is an exact delayed copy, and release snaps back to
exactly unity. Monotonic queues schedule a bounded linear attack and hold over
the detector support; release is exponential. The full look-ahead contributes
to chain latency. All memory is allocated during construction, and
processing/reset accept arbitrary call sizes without heap operations.

Auto headroom learns the material's need in 0.5 dB steps (cap 12 dB) before
the digital volume (the chain passes the block's volume gain), never releases
it mid-stream, and subtracts the volume's own attenuation. The daemon carries
it across rebuilds with an unchanged limiter config and output rate, and keeps
a warming chain from deciding. `ClipStats` carries incremental events and the
window's largest attenuation through the atomic meter bank. Direct and DSP-off
paths do not run this stage.

## Feature design

All DSP runs in f64 from the sink to the final quantizer. The audio thread
never allocates, locks, logs or does I/O; filters are rebuilt on a worker
thread and swapped in lock-free. An ALSA underrun it recovers from is
counted in an atomic (also when the handle closes right after a tail
drain), and the driver's control thread logs it. alsa-lib's own messages
on that thread go to a counter too (a thread-local error handler set
before it turns real-time), logged the same way.

### PCM quantization and noise shaping

After volume and the limiter, `dither::Quantizer` converts planar f64 to
interleaved, left-justified i32 at the negotiated output word length. It adds
no signal latency and accepts arbitrary block lengths. Fixed dither and feedback modes
keep a KISS generator, up to fifteen integrator states, five clean-input error
delays and a sixth-order coloured-noise filter per channel. All memory is
allocated at construction; process and reset perform no heap operations.
Dither enters inside the feedback loop, with `w = scaled_input - raw_q`, so
both dither and rounding error are shaped. Input headroom is specified in
LSBs. A final packing clamp prevents signed-word wraparound without feeding
the saturated value back into the fixed loops. `tpdf` uses the original
generator at full-scale gain, `shaped` a rate-adaptive filter, and
`headroom_tpdf` the per-channel KISS source. Explicit fixed modes enable a
quantizer-only DSP path; Direct and the master DSP bypass still copy samples
unchanged.

### Input and routing

- Two sinks: **Audivane DSP** (full chain) and **Audivane Direct** (no DSP,
  bypass path).
- System-wide: set `default.audio.sink` in PipeWire metadata.
- Single app: move only the chosen streams with `target.object`, as
  pavucontrol does.
- Read MPRIS to show the current track and its native rate.
- Input through the graph is f32, which is exact for sources up to 24-bit.

### DC blocking

- High-pass stage (`crates/dsp-core/src/dc_block.rs`), first at the source
  rate, before pitch shifting and the stereo image. Off by default.
- 2nd-order Butterworth high-pass (`Q = 1/sqrt(2)`, prewarped bilinear/RBJ) in
  Direct Form I `f64`, cutoff `1.0..=20.0` Hz (default 5 Hz, where the
  magnitude response is −0.017 dB at 20 Hz and −0.0097 dB at 23 Hz), zero
  latency, per channel.
- Bounded-input sample-peak maximum (L1 norm) is `‖h‖₁ ≈ 2.4345` (+7.73 dB on a
  sign-matched worst-case input). Measured peak gains at 48 kHz / 5 Hz relative
  to input peak: full-scale step from silence −0.004 dB, bass-rich multitone
  +0.066 dB, synthetic kick −0.169 dB, settled full-scale bipolar step +6.0 dB,
  5 Hz square wave +7.3 dB (bounded-input sign-matched maximum +7.73 dB). No
  automatic gain compensation is applied (handled by EQ preamp or limiter if
  needed). Direct and DSP-off paths skip it.

### Stereo image

- Stereo-only stage (`crates/dsp-core/src/crossfeed.rs`) at the source rate,
  placed before EQ so headphone-correction EQ stays last before the
  transducer.
- Crossfeed modes `bs2b_default`, `bs2b_chu_moy`, `bs2b_jan_meier`,
  `bs2b_custom` and `linkwitz` implement the published bs2b design (cross
  low-pass, direct high shelf with its own corner), or Linkwitz's mono-flat
  complementary pair with a 0.3 ms interaural delay. `bs2b_custom` takes a
  cutoff (300..=2000 Hz) and feed (1..=15 dB); `none` runs the stage for its
  width alone. First-order IIRs, zero latency.
- Width (`width`, side gain 0..=2, 1 leaves the image unchanged) scales the
  side signal after the crossfeed: 0 is mono, 2 twice as wide. `width_shape`
  applies it at every frequency (`flat`) or above a corner only
  (`above_bass`, corner 200..=2000 Hz), with a first-order high-pass on the
  side's change. The mid is never changed, so mono passes bit-exact at every
  width.
- The crossfeed part scales both paths by one constant chosen at build time
  that keeps the A-weighted loudness of stereo pink noise (correlation 0.5)
  unchanged. The width is not loudness-compensated, so a step wider is
  louder. Nothing cuts for safety: worst-case signals can exceed the input
  peak by +1.4 to +5.6 dB, more with widening, and the limiter and the output
  headroom catch it. The EQ preamp stays independent.

### Harmonic distortion

- The stage (`crates/dsp-core/src/harmonics.rs`) runs after the upsampler, at
  the output rate, and before volume and the quantizer, so H2…H9 sit below
  Nyquist when the ratio is high enough.
- A Chebyshev layer sets H2…H9 (`levels_db`, dB relative to the
  fundamental for a full-scale sine at drive 0 dB, `null` = off), a sign per
  order (`signs`, `invert_even`), `drive_db` and `amount`, and can add a
  frequency tilt, supply sag and bias shift.
  `y = x + Σ a_k (T_k(x) − T_k(0))` gives exact levels for a full-scale sine.
  The terms are evaluated one by one (Chebyshev recurrence) as pure
  distortion `D_k = T_k − T_k'(0)·x − T_k(0)`, with the linear parts on the
  direct path (`L = 1 + Σ_odd a_k T_k'(0)`), ready for a filter per order.
  A 5 Hz DC blocker filters only the even (DC-making) terms. The stage plays
  at its natural gain; its certified peak bound is reported (and counted by
  the A/B headroom), never applied as a cut.
- The stage is a list of layers (Chebyshev curves, transformer or tape-core
  flux, tape hysteresis), added in parallel or run in series. A layer preset
  (`harmonics_presets.json`) fills one layer; a system preset
  (`harmonics_systems.json`: a single-ended triode amp, a tube preamp into a
  solid-state power amp, push-pull amps, a tube output stage, tape then
  tubes) replaces the whole stage with layers and routing in one step. Either
  way every layer stays editable and can be reset to its layer preset. The
  Harmonics page offers systems at the top of its Layers panel (`avdsp ctl
  harmonics systems` and `system <id>` on the command line).

### Bit-perfect bypass

- Bypass copies samples straight through: no f64 round trip, exact
  f32 → S16/S24/S24_3/S32 conversion.
- The UI shows "Bit-perfect", or "Not bit-perfect" because a volume is not
  100%, streams are mixing, the rate differs, or DSP is on. Volumes are shown
  on the cubic scale desktop sliders use (linear 0.27 is 65%).
- Self-test: play a known signal and hash the bytes handed to ALSA. Users with
  S/PDIF loopback can also hash at the DAC input.

### Sample rate and clock

- When one stream plays, the graph switches to its rate; the engine reopens
  ALSA at that rate, at rate × N when upsampling, or at an explicit output
  rate (`{"rate": 48000}`: fractional or decimating, see "Rational
  resampling" below).
- Filters are recomputed for each new rate: biquad coefficients, resampled
  impulse responses (cached per rate), upsampler kernels.
- The upsampler stage runs behind the `UpsamplerRuntime` trait
  (`crates/dsp-core/src/presets/runtime.rs`), so the polyphase windowed-sinc
  engine (`Upsampler`), the rational runtime and the runtimes with fixed
  filters (`crates/dsp-core/src/presets/`: x2/x3 stage cascades, short
  interpolators, block FFT interpolation) share the same real-time path and
  offline response measurement.
- Every upsampler is built from one design type, `FilterDesign`
  (`crates/protocol/src/design.rs`, re-exported from
  `crates/dsp-core/src/design/`): `UpsamplerConfig` carries either a named
  catalogue entry (`preset`, including `"recommended"`) or a full custom
  `design` (`FilterDesign`). One
  `BuildContext` (rate, ratio, channels, block, pipelines, the output an
  automatic budget resolves against, the design workspace cap) drives
  three steps, all off the audio thread: `feasibility` (cheap: ratio
  rules and the budgets of `design::budget`, giving a `Plan` of grid
  taps, construction workspace, resident memory and callback work),
  `prepare` (the search, the kernel
  and the post-build safety gate, `design::safety`) and `build` (the
  runtime, headroom applied; none at ×1). The chain builds through
  `design::upsampler`; the daemon validates and publishes its rate masks
  with `feasibility` (a pair the mask offers passes `prepare`), and the
  response measurement prepares its design at the ratio it measures.
  `design::offline_kernel` exports a kernel within the construction
  limits only.
- Mute ramp around each switch, since the DAC needs time to relock.
- Two streams at different rates: one gets resampled; the UI says which.
- Only rates the DAC can play are offered. When the engine opens a device it
  probes which (rate, format) pairs it takes (`crates/io-alsa/src/caps.rs`)
  and publishes only the allowed rates it can play, directly or upsampled
  (`crates/io-pw/src/rate_caps.rs`), to PipeWire. A 48 kHz-only USB device
  therefore runs a 48 kHz graph and PipeWire resamples 44.1 kHz apps into it.
  When the device cannot take source × the wanted ratio, a smaller ratio is
  used (down to ×1), never a "nearest" device rate, so the graph rate is
  always the device rate divided by the ratio.
- Audivane Direct never runs the DSP chain, so a rate the DAC plays
  natively plays ×1, bit-perfect. At a rate the DAC lacks, the engine
  bypass resampler (`rate.bypass_resampler: "engine"`, the default) keeps
  the graph at the source rate and converts it to the native rate PipeWire
  would have moved to, at a fixed L/M (the resample-only mode; the rate is
  `DesiredDspConfig::bypass_landing`). Converted rates are added to each
  sink's native ones, never in their place, and only an app at exactly such
  a rate runs the graph there, so a native rate is never converted and
  nothing is converted twice. With `"pipewire"`, or a pair the engine cannot
  build (or is still building), the stream moves the graph to a native rate
  and PipeWire resamples it (reported as not bit-perfect); with no native
  rate at all, Direct reports an error and the DSP path keeps working. DSP
  off does the same on the DSP sink. A move the graph does not follow fails
  the open after a bounded time (see "Playback modes and reopens").
- The DAC period is the quantum PipeWire actually runs at the graph rate
  (power-of-two rounding included), so one graph cycle feeds one period.
  When PipeWire runs another quantum anyway, the extra cycles are spread out
  and the sink ring keeps a cushion, so the output is never zero-filled.
- Output rates go up to 3.072 MHz (`MAX_OUTPUT_RATE`). The driver probes
  every multiple of a graph rate up to 768 kHz plus 1.4112, 1.536, 2.8224
  and 3.072 MHz (`MASK_OUTPUT_RATES`, 36 rates); other multiples above
  768 kHz are no DAC rate and the ladders step over them. Everything that
  depends on the output rate is sized in time, not frames: the ALSA period
  (the graph quantum × the ratio: 21 ms at 48 kHz × 64 with a 1024
  quantum, 65 536 frames), the RT buffers (from the negotiated buffer),
  the limiter look-ahead, and the post-spectrum ring (42.7 ms at
  3.072 MHz; the analyser decimates by 2⁶ to the base family). A period
  larger than half that ring (×64 at a 2048, 4096 or 8192 quantum) is
  queued as the mean of each 2, 4 or 8 frames, at the rate divided as
  much, so it still fits whole. A push that does not fit anyway drops
  whole frames and starts a new spectrum epoch, so the analyser never
  joins audio across the gap; when the post tap cannot complete an
  analysis, the pre spectrum goes out without it (`post` empty). The processor's scratch covers a whole period at the
  largest ratio (8192 frames × 64 × 2 channels, about 8 MiB for its i32
  and f32 halves together), allocated before the audio thread starts. Very
  high rates are large in bytes: S32 stereo at 3.072 MHz is 24.6 MB/s, and
  ALSA's `*_near` sizing shortens the buffer where a driver caps its size
  (USB audio commonly at 1 MiB, 42 ms there).
- Dither targets the negotiated format's bit depth (S16 to 16 bits).

Which rate wins is decided by `crates/io-pw/src/rate_arbiter.rs`, a pure
function of the running clients, the current rate and the time since the
last switch (unit-tested without PipeWire):

- Direct comes first: whatever plays into Audivane Direct (bit-perfect)
  decides, takes priority over the DSP sink, and maps onto the native (×1)
  rates only.
- Agreeing clients are followed: each client's native rate is mapped to an
  allowed rate, and if all running DSP-sink clients map to the same one, the
  graph runs at it.
- Disagreeing clients follow `rate.conflict`. `keep_current` (default) keeps
  the current rate while any client still plays at it, otherwise the client
  that started running first wins (a per-client start sequence number, never
  map iteration order). `highest` takes the highest native rate, and
  `default_rate` uses the default rate while they disagree.
- Minimum dwell: no switch within 2 s of the previous one. A switch that is
  still wanted when the dwell ends is made then (the PipeWire thread's
  100 ms timer re-checks), so it is delayed, not dropped.
- Our own fallout is ignored: a switch reopens ALSA and makes clients pause
  and resume. For 1.5 s after a switch (longer while the driver still reports
  `Switching`, at most 6 s), a client that pauses and resumes keeps its place
  in the start order, and no new switch is made while the driver is still
  switching.
- Each distinct decision is logged once at info level with its reason, e.g.
  `Rate: kept 48000: Brave still playing at 48000; others are resampled`.

Without these rules, two apps at 44.1 and 48 kHz flip the graph back and
forth forever: each switch pauses and resumes the clients, and whichever
the registry map yields last wins the next round
(`tests/daemon/engine_rate_conflict.sh` covers this on the loopback rig).

### Varispeed

- Plays the source at speed `s` (tempo and pitch together) with an adaptive
  resampler: ×2 → a variable-step core at 2G → ÷2, at the graph rate
  (`crates/dsp-core/src/varispeed.rs`).
- Runs once, in the processor, **before** every chain: the chains (EQ,
  convolution, the upsampler, A/B, swaps) get its planar f64 output at the
  nominal rate and never see a variable ratio
  (`crates/daemon/src/engine/varispeed.rs`). With the stage off they get
  the source block unchanged.
- The driver pulls about `s` times more source frames per period and
  schedules graph cycles by watermark instead of one per quantum
  (`crates/io-pw/src/varispeed_sched.rs`, `varispeed_rt.rs`); the speed
  range is capped to what the negotiated period and quantum sustain.
- DSP sink with DSP on only: Direct and DSP off never run it and stay
  bit-perfect. Starting and stopping cross-fade over 5 ms before the
  chains; a DSP toggle that drops the chain fades the processor's output
  (through one transition that coalesces queued swaps first); the stage
  holds 5 ms of source back so it can fade out when a pull comes back
  empty.
- Configured at the top level of `EngineConfig` (`varispeed`, off by
  default) and controlled live with `set_varispeed` without rebuilding the
  chains; `Status.varispeed` and `PassthroughBreak::Varispeed` report the
  audio thread's applied state (see [control-plane.md](control-plane.md#varispeed)).

### Multi-device output

- Up to three **followers** play the master's source on other DACs
  (`crates/daemon/src/engine/tee.rs`, `follower.rs`). The processor tees the
  source block before the master's varispeed stage into one lock-free ring
  per follower;
  the master never waits on a follower.
- Each follower has its own RT thread (one priority step below the
  master's), its own varispeed stage at `speed · (1 + ε)` with ε from a
  drift loop against the master's DAC position, and its own instance of
  the master's chain, built for its device. Chain edits reach every
  follower; A/B stays on the master; followers mute in Direct and with DSP
  off.
- The master is delayed up to 120 ms so the followers have source ahead; a
  follower that needs more plays late by the rest (`unaligned`), and steps
  back to aligned once it no longer needs it. While a
  follower is configured the master's speed is capped at 1.996.
- Configured in `EngineConfig.outputs.followers`, controlled live with
  `set_follower`, reported in `Status.outputs` (see
  [control-plane.md](control-plane.md#multi-device-output)).

### Pitch shift

- Tempo-preserving, phase-coherent multichannel pitch shift in `f64`
  (Signalsmith Stretch, `crates/dsp-core/src/pitch.rs`), running at the source
  rate before EQ and convolution so transducer/room corrections stay at their
  fixed physical frequencies. Set in cents or semitones, or from one tuning
  frequency to another (440 to 432 Hz); quality Low, High or Max.
- Omitted from the chain (bit-exact bypass) when the effective shift is zero
  or it is disabled; reports its input + output latency in `latency_out` so
  A/B comparison stays time-aligned.

### EQ

- RBJ cookbook biquads in f64; transposed Direct Form II, or Direct Form I for
  low-frequency, high-Q filters.
- AutoEQ / Equalizer APO import.
- Layered sections: linked (both channels) plus optional left / right.
- Linked tilt filter (`TiltConfig`): constant dB/oct slope over `[start_hz, end_hz]`,
  flat outside, with sharp corners and a configurable 0 dB pivot. Minimum
  phase: the sharp target splits exactly into a smooth softplus ramp (a
  first-order bilinear shelf cascade, three per octave) plus a cusp at each
  corner (four peaking-type biquads each), all fitted per stream rate in the
  digital domain at chain build. Each design is checked against the sharp
  target (±0.1 dB outside ±1/6 oct of the corners, a slope-proportional
  allowance inside, the delivered total within 0.1 dB); invalid bands and
  failed checks are build errors. Start/end pivots anchor DC/Nyquist;
  Hz/center pivots anchor the actual curve. An optional treble section
  (`tilt_high`) makes a two-section tilt: bass below X, treble above Y,
  0 dB between, both in one cascade. Bands lie within 20 Hz–20 kHz. Status
  exposes effective bounds and clamping for both A/B chains.
- Bass and treble knobs (`tone`): a low shelf at 140 Hz and a high shelf at
  4.9 kHz on every channel, ±6 dB, outside the band lists.
- Automatic preamp from the peak boost, plus a clip and inter-sample-over meter.

### Convolution

- Non-uniform partitioned FFT convolution: small first partitions for low
  latency, larger ones for long IRs.
- On each rate change, resample the IR offline (its frequency response
  kept) and swap it in.
- An edit before an unchanged convolution (an EQ band) takes the playing
  chain over in place: the new chain moves in its convolver and later
  stages with their state, replaces it in the next period, and fades the
  edit in at the convolution's input. An IR whose far taps cancel can take
  the output past both settings' during the fade, so the chain bounds that
  at build (`fade_bound.rs`) and picks the shortest fade within 0.125 FS,
  or warms up instead. An edit that keeps the convolver's input copies its
  state (budgeted per period); a new IR warms up for its ring (at most 1 s).
  See [control-plane.md](control-plane.md#chain-wire-shapes-eq--convolution),
  "Edits with a convolution loaded".

### Upsampling

- Kaiser-windowed sinc kernels designed in f64, linear, minimum or
  intermediate phase. Intermediate phase blends the two (`phase_mix`, 0 =
  minimum, 1 = linear) with the same magnitude: a fractional all-pass on the
  root path, a partly folded scaled cepstrum on long kernels. The blend is
  not finite, so it is cut where its tails fall under the error the
  minimum-phase contract allows and checked like the minimum-phase twin; the
  kernel ends up about 1.45x the design length at beta 14, up to 3.1x at
  beta 2 (at most 4x).
- Two ways to specify a design: manual (`taps`, Kaiser beta, a cutoff
  fraction or "stop at Nyquist"), or band (`passband_end`, `stopband_start`,
  `attenuation_db`), where the engine picks beta and the shortest length
  from the same calibrated transition width as "stop at Nyquist". Manual
  `taps` can count output taps or taps per input sample (the kernel then
  lasts the same time at every ratio); band designs always do. A band's
  passband end may be an absolute frequency (`passband_end_hz`), converted
  to a fraction of the Nyquist when the design is built (Recommended's and
  the apodizing preset's 20 kHz at every rate, or a Custom design's "Hz"
  unit), so its kernel shortens as the source rate rises; where the
  frequency does not fit the band at a rate, the band's fraction stands in
  and the design check notes it.
- The fixed output headroom (`output.headroom_db`, 0..12 dB, at the end of
  the chain at every ratio) is folded into the kernel when no harmonics
  stage follows, so it costs nothing per sample there.
- Window: Kaiser (default; the narrowest transition for a length and
  stopband) or Gaussian. A Gaussian-windowed sinc is the ideal low-pass
  convolved with a Gaussian: an erf transition about 2x wider than Kaiser's
  at the same length and stopband, no sidelobes of its own, and ringing that
  dies away like a Gaussian (at 3/4 of the half length about 40 dB lower
  than Kaiser's). Its transition is analytic, so band designs and "stop at
  Nyquist" compute its length directly. Third, Tukey (`taper`, default 0.25):
  flat over the middle so most coefficients are the ideal sinc, with a cosine
  taper over the outer fraction of each half. Its stopband slopes down rather
  than sitting at a floor (65,535 taps x8, taper 0.25: about -111 dB from 2%
  past the cutoff, -152 dB from 10%), so it has no calibrated length formula
  and is limited to fraction mode with linear phase. On the preset paths
  the Custom builder also takes the DPSS (Slepian) window, the sequence with
  the most energy within `NW / N` of DC (Kaiser approximates it at `beta =
  pi NW`). It is built in O(N) at any length: the Slepian tridiagonal
  eigenproblem rewritten as a Sturm-Liouville recurrence in a small
  eigenvalue (its row sums are constant), solved by bisection on the
  solution shot from the edge; checked against scipy. Its stopband model is
  Kaiser's at `beta = pi NW`, 12 dB short of the formula (Kaiser: 14); at
  one length it is within about 2 dB of a Kaiser window of the best beta,
  mostly deeper.
- Optimal designs (the `optimal` kind): an equiripple (Parks-McClellan)
  or least-squares low-pass over a band, no window, on the fir paths, with
  every phase. Equiripple is a short-filter design: about 16 M^2 work per
  exchange, capped at 4 095 grid taps (past 4.6k it fails at clusters of
  lengths from about 90 dB) and a 150 dB automatic budget (f64 stops
  levelling the ripple near 1e-10); started from the least-squares
  design's error peaks. Its flat stopband adds up in every polyphase
  branch, so it also needs the depth that keeps the branches' DC gains
  within the safety gate (about 76 dB at x8): fixed lengths short of it,
  or past what f64 levels, are refused from Kaiser's estimate. Least
  squares is solved by conjugate gradients with FFT products, up to 65 535
  taps; its error floors at 137 to 153 dB. Against Kaiser at one length
  (x8, band 0.9 to 1.0 of the Nyquist, the engine's error against the DC
  gain): equiripple within 2.5 dB of the best Kaiser beta, least squares 5
  to 10 dB short; the automatic lengths for one budget: equiripple 4 to 8 %
  shorter than Kaiser's, least squares 1 to 2 %.
- `craven_apod`: Peter Craven's apodizing filter (JAES 52(3), 2004), fully
  down by the source Nyquist so the recording's own brick-wall ringing
  there is removed and replaced by a gentle transition from 20 kHz, at
  every source rate (the passband is the audio band, not a fraction of the
  Nyquist, so the kernel shortens as the rate rises), minimum phase: a
  Kaiser band design at 150 dB with the reflected phase.
- "Keep the input samples" (`preserve_samples`): a linear-phase design at
  cutoff 1.0 made into an exact Nyquist (M-th band) filter, 1 at the centre
  and 0 at every `ratio`-th tap from it, odd length. The response reports
  the leak at the input sample instants for every design.
- FFT-based (zero-stuff, then FFT-convolve at the output rate). Direct
  polyphase with 100k taps at 16× costs about 9 GMAC/s, which is too much CPU.
- The FFT polyphase engine (`resample::Upsampler`) convolves the `ratio`
  polyphase branches with non-uniform partitions, on the convolver's engine
  (`dsp_core::nupc`): stage 0 is uniform at the input block (the chain
  block's power of two, 64..1024, so the latency is that block times the
  ratio for every kernel), and a long kernel's later taps run in stages of
  4x larger blocks up to 32768 (lower above x16, to bound the output
  buffers), each with one forward transform and one delay line shared by
  every branch, time-sliced over its period so the heaviest callback stays
  close to the mean. The schedule (`nupc::polyphase_plan`) merges a stage
  into the one before it while that is cheaper by the job cost model;
  kernels up to about 20 base blocks per branch keep one stage, the
  uniformly partitioned engine, bit for bit. 8M taps x16 from 44.1 kHz on
  512-frame blocks costs about 6 % of a core per channel instead of 110 %,
  no more than from 48 kHz; resident about 130 MiB of shared spectra plus
  20-37 MiB per channel.
- Kernels up to 8M taps (`design::budget::MAX_KERNEL_TAPS`). Building
  one may allocate at most the machine's design workspace, a quarter of
  the total RAM within 1.5 to 4 GiB, read once at start-up. Latency grows
  with length (1M linear-phase taps at 705.6 kHz ≈ 0.7 s), which is why
  minimum phase is offered too. The app shows the engine's measured
  response for the chosen design.

### Rational resampling

Any output rate, not just integer multiples: 44.1 → 48 kHz, 44.1 → 192 kHz,
or decimation (96 → 48 kHz) for a DAC that plays fewer rates than the
sources use.

- The ratio is `out / in = L/M`, reduced (`RateRatio`,
  `crates/protocol/src/ratio.rs`). Integer upsampling (`M = 1`) keeps the
  FFT polyphase engine; everything else runs
  on `crates/dsp-core/src/rational.rs`, a direct polyphase resampler: the
  kernel lives on the intermediate grid at `in × L`, and output `m` is one
  dot product of branch `(m M) mod L` (T taps) with the newest T input
  frames ending at `floor(m M / L)`. AVX2+FMA dot products are picked at
  run time, with a fused stereo path. About 7-10 G multiply-adds per second
  on one core, e.g. 44.1 → 48 kHz with 511 taps per branch, stereo: real-time
  factor 0.006.
- A block of `n` input frames gives a varying number of output frames:
  `ceil((pos + n) L / M) - ceil(pos L / M)`, which depends only on the input
  position `pos` (mod M). Every stage, chain and driver counts the same way
  (`RateRatio::frames_out`), buffers are sized from the maximum per call
  (`ceil(n L / M)`), and the processor keeps every chain it runs (active,
  warming, crossfading, A/B) on the same grid: a chain installed mid-stream
  is moved onto it (`reset_to(pos)`) so its counts and samples line up with
  the one it replaces.
- The RT driver plans each ALSA period from the input position: the fewest
  input frames that fill it, carrying the few extra frames (fewer than
  `ceil(L / M)`) into the next period. The prefill before `start()`
  (auto-start is off, so nothing frees room) writes as many whole periods
  as the negotiated buffer holds, at least one (one ×64 period of 65 536
  frames into a 98 304-frame buffer, three 512-frame periods into 1536),
  and within those only what the buffer takes. A prime with no source data
  yet writes the planned frames as silence (never more than the buffer
  takes) and credits the planner like audio, so the primes after it fit; the frames past it wait in a preallocated carry (allocated before
  the thread turns real-time, bounded for every ratio) and go out ahead of
  every later write, the tail drain on stop included. A short write leaves
  the rest carried and holds the next block back; an underrun leaves it for
  the prefill that follows, which writes it first.
- Latency is the exact sum of the stages' delays in output frames, rounded
  once to the nearest frame (halves up).
- Cost is `output rate × taps per branch` per channel, capped at 800 M
  multiply-adds per second per channel over every rational pipeline that
  can run at once (a swap runs two chains, A/B doubles that), and kernels
  are capped at 8 MiB of coefficients each (past the caches the runtime is
  memory-bound; an A/B swap holds four kernels, so up to 32 MiB in all).
  A design over either limit never runs late: validation rejects it for a
  fixed output rate (`rate`) at the rates the policy allows, the ratio
  ladders skip it for `max_rate_any`, and the chain build refuses it,
  counting the pipelines the config runs.
- Manual designs place their band edge at the lower of the two Nyquists;
  presets follow their rate rules
  (decimation scales the cutoff by out/in and lengthens the kernel by
  in/out).
- `max_rate_any` stays in the source's family (highest playable integer
  multiple under the cap) and crosses to the other family only when no
  integer multiple above ×1 plays, or when the other family's best rate is
  at least twice as high; `rate` fixes one output rate. The driver's ratio
  ladders only offer rate pairs every configured chain (A and, with A/B, B)
  can be built for: a mask the daemon computes from the upsampler designs
  over every rate the driver probes, so a device's rate list can never pick
  an unbuildable ratio. A config change that alters the mask reopens the
  device when the running pair is no longer buildable or a fresh open would
  pick another ratio. The reopen checks predict a fresh open from what the
  last one found (see "Playback modes and reopens" below), so DSP on/off,
  Direct/DSP switches and edits end where a fresh open would, and never
  reopen into the same result. If a chain
  still fails to build at a fractional ratio, the upsampler on both A/B
  sides (always equal) is replaced by a safe design (256 taps per sample at
  the lower of the two rates, flat to about 0.93 of the lower Nyquist); EQ,
  convolution and A/B keep running, and status says so. The last resort plays chain A's upsampler with
  its limiter (a safety stage, always kept) and pitch, stereo image and
  harmonics where they build, and says what was dropped (EQ, convolution,
  A/B, and any of those stages that did not build).

### Playback modes and reopens

The DAC plays Audivane Direct while a running client feeds it, otherwise
the DSP sink while its node streams. A paused client that stays linked to
Direct counts for nothing: not for the sink choice, the rate arbitration,
the recovery after a failed open or the playback sessions, so it never
keeps a client playing on the DSP sink silent.

What a sink plays follows from the sink, DSP on/off and the device ratio:

| Mode | When | Chain |
|---|---|---|
| Bypass ×1 | Direct, or DSP off, the DAC plays the graph rate | none: the samples unchanged (volume aside) |
| Resample-only L/M | Direct, or DSP off, `bypass_resampler: "engine"`, the graph rate not native, the pair builds | the engine bypass resampler's chain (`ActiveChain::Resample`): the fixed recommended upsampler design alone, digital volume, dither to the output word length |
| Full L/M | the DSP sink with DSP on, at any ratio (×1, integer or rational) | the configured chain |

The processor's `PlayMode` follows from the sink, DSP on/off and the
device ratio alone. Direct and DSP off open `[O/G, ×1]` (the bypass
resampler's ratio when it applies, then ×1), and bypass never runs away
from ×1 (repeating samples is not resampling): away from ×1 they play only
the resample-only chain, and the DSP sink never plays it.

- The daemon builds the chain for the input rate and ratio the device was
  negotiated at, also for an edit while a graph-rate change waits for its
  reopen or while the negotiated chain still waits for room in the RT
  queue (so an edit that supersedes it satisfies the same resume gate). While Direct plays, the chain kept warm is the DSP sink's, built
  for where a switch to it would land (`open_landing`, as the RT thread
  predicts it).
- The processor plays the active chain only if it was built for the input
  rate and ratio the device runs at (both are the chain's identity);
  anything else is silence. Direct or DSP off away from ×1 without the
  resample-only chain (after a switch from the DSP sink, or DSP turned
  off in PipeWire mode, until the ×1 reopen) is silence too, reported
  while playing. A DSP toggle at the same ratio with the other chain on
  its way keeps the outgoing chain audible (on the same input) while the
  incoming one warms up, then crossfades, or fades through silence when
  their delays differ. The resume gate after a reopen uses the same check:
  Direct and DSP off at ×1 never wait for a chain, and away from ×1 they
  wait for the resample-only chain as the DSP sink waits for its chain.
  The daemon builds that chain on every negotiation there, and on an edit
  only when its inputs change (the output section, DSP on/off: turning DSP
  off at the ratio the DSP chain runs swaps the chains with no reopen).
  The chains are designed and built on the engine's design thread, off the
  builder lock (one built chain per pair kept ready); an open whose chain
  failed or is still being built makes the driver hold the pair back and
  open again into the PipeWire path before resuming, and a pair whose
  chain is then built comes back. The design thread builds the pairs an
  open or an edit waits for first (never dropped for room), then spare
  refills, then builds ahead of time, in a bounded queue; an edit of the
  output section or quantum drops queued work for the old ones, and a
  device or bypass mode change drops all queued work but what the edit
  itself waits for. A design answers only the negotiation and edit that
  asked for it: an edit's chain is published only in that negotiation, and a
  design from an older epoch (bumped by an edit of the device, the capped
  output section, the quantum or the bypass mode) is not reported; the
  driver hears of the new epoch and releases the pairs it held. An edit
  waiting for its chain (a DSP toggle at the same ratio) carries over a
  volume change; if the design fails or the edit is superseded, the
  outgoing chain's hold ends (silence until the reopen). Only the chain the
  toggle waits for ends the hold when it installs: an older swap of the
  outgoing kind (an edit just before the toggle) does not.
- A sink switch or an edit reopens the device when a fresh open would land
  on another ratio (`decide_rt_reopen`), and never into the same result.
  A switch to a sink that has no chain for the running ratio (none
  installed or on its way in) reopens too, so a negotiation builds it: a
  switch never waits for a chain nobody builds.

The device ratio is where a fresh open lands
(`DesiredDspConfig::open_landing`, `crates/io-pw/src/lifecycle.rs`): the
first ladder entry the device plays in a configured format and the last
open did not see refused. Refusals:

- Only a confirmed capability rejection is *refused*: `EINVAL` from a
  parameter setter or the install, or a rate the device rounds away. Any
  other error (busy, a hot-plug, I/O, a timeout) is *transient*.
- Transient entries are skipped until an edit changes what steers
  negotiation (DSP on/off, the target, the ratio limit, the buildable
  pairs) or reopens anyway. A volume or EQ edit keeps them skipped.
- Both are dropped when a new playback session starts: a stream starting
  on either sink (its node streams and a running client feeds it; the
  node's flag alone is set for both sinks while either drives the group,
  and a paused client stays linked). An open's
  findings belong to the session it started in, so a stream that starts
  while the device opens and the chain builds retries them. The RT thread
  publishes the session and the daemon predicts with it, so both see the
  same open.

The graph rate is the rate policy's (`plan_graph_rate`,
`crates/io-pw/src/rate_caps.rs`): Direct and DSP off need a rate the DAC
plays natively, the DSP sink with DSP on any rate it plays at some ratio.
The states, as the control and RT threads see them:

1. **Playing.** The device is open for the graph rate and the RT thread
   writes. A graph-rate change, a sink switch or an edit that changes the
   landing → **Reopening**, after a short fade-out. Across a graph-rate
   change the fade carries only what the chain already holds for the old
   open: input at the new rate is discarded and the chain gets silence,
   never new-rate samples played at the old rate.
2. **Reopening.** Control plans the open:
   - the rate plays for the sink → **Playing** at its landing;
   - it does not, and the policy has a native rate → control asks PipeWire
     to move the graph there and opens for it → **Playing** (the RT thread
     reopens again as soon as it sees the graph still at the old rate);
   - Direct with no native rate at all, no playable landing, or a move
     PipeWire has not followed 3 s after its first attempt (`MoveAttempts`:
     one deadline per unresolved move, however often it reopens; only a
     landing the RT thread observes, or another requested move, starts a
     new one) → **Failed**. A move slower than that fails briefly, and its
     landing (a graph-rate change) brings it back.
3. **Failed.** The engine reports the error and writes nothing; status
   says no output is playing (never "Playing" with silence). The RT thread
   retries only when the graph rate or the route changes → **Reopening**.
   The DSP path keeps working.
4. **Waiting for the device.** The device failed in a way a stream restart
   cannot fix (usually unplugged: `prepare` or `start` fails), or a reopen
   found it gone, or it was missing at start-up, or the control thread's
   presence watch (`/proc/asound` every 500 ms while it holds the device)
   saw it go while nothing played (ALSA reports nothing then); the idle
   output is handed back like a failed one. The output is closed and
   the card's reservation released; the RT thread drives the graph on its
   own clock (`advance_and_trigger` at the rate and quantum PipeWire asks
   for, paced by a timed `ppoll`) and drains the sink rings, so streams stay
   on Audivane's sinks and keep progressing into silence. The control
   thread looks for the device in `/proc/asound` every 500 ms and, once it
   is there, asks the RT thread for a reopen (`device_retry`) →
   **Reopening** → **Playing**, faded in, with the mixer written by the
   negotiation first. A device that is there but keeps failing is retried
   after 1 s, doubling up to 30 s. An explicit release ends the wait (a
   later acquire opens from scratch), as does a successful acquire. The
   daemon may pause the players whose streams go through Audivane over
   MPRIS when the device goes (`device_lost`, see control-plane.md).

### Channel workers

With `channel_workers` on (the default), the main output's DSP chains run
their per-channel stages on one thread per channel (`dsp_core::workers`,
`dsp_core::chain` "Channel workers"). The split point is the stereo image: DC
block, pitch (phase-coherent across channels) and the stereo image run whole
on the audio thread; then each channel's EQ → convolution → upsampler →
harmonics → output headroom runs as its own single-channel copy of each
stage (kernel spectra, IR spectra and FFT tables shared, read-only; scratch
and per-channel state per thread), channel 0 on the
audio thread and channel 1 on a worker; then everything that couples the
channels runs on the audio thread (A/B delay, meter, mix and
level, volume and balance, the limiter's one gain, quantizer and dither,
peaks and clip counts). Each copy computes its channel exactly as the
whole stage does, so the output is bit for bit the single-threaded one. A
convolution filter matrix (`dsp_core::matrix`) couples the channels too:
with one, the EQ and the matrix also run whole on the audio thread, and
the lanes start at the upsampler.

- Handoff: per block the audio thread copies each channel in, stores
  the lane into the worker's slot (an `Arc` clone and one futex wake), runs
  its own lane, then waits: a 20 µs spin, then a futex wait on the slot
  with a timeout. The worker sleeps on a futex between blocks. No locks, no
  allocation. A worker takes lanes only once its start (the rtkit call)
  has returned; until then the audio thread runs them. The worker drops
  its reference to a lane before it hands it back, and a chain dropped
  while a worker still holds one of its lanes waits for it (off the audio
  thread, up to 10 s, then leaks it), so a lane's memory is always freed
  where the chain is dropped, never on a worker.
- Deadline: two block durations (at least 2 ms) from the handoff. Up to
  one block late the stream underruns as an overloaded single-threaded
  chain would, and the overload fallback sees it. Past the deadline the
  worker still holds that lane's state, so the chain plays digital silence,
  block by block with its frame grid advancing, until the lane is back,
  then resets (as `reset_to` at its grid position) and plays on. A worker
  still busy when a block starts gets no lane: the audio thread runs it.
  A panic on a worker poisons the chain: from that block on it plays
  silence (never the lane's half-written buffers) until it is rebuilt.
  Volume, balance and mute ramps keep moving through silent blocks, so a
  mute set during a stall is done when sound comes back. The pool counts
  late blocks, inline lanes and panics; the daemon logs them every
  overload tick (100 ms).
- Priority: the pool (one worker for the engine's two channels, none on
  a single core) starts with the first chain that wants it; each worker
  asks rtkit for real time at the audio thread's priority, best effort, as
  the audio thread does. A worker is under the audio loop's overload guard
  (see "Overload"): it registers as a helper of the audio thread, so the
  `SIGXCPU` handler demotes it by id whichever thread takes the signal,
  and it runs a `RunGuard` between jobs (a 100 µs nap after 50 ms without
  a sleep). Its priority follows the audio thread's: every overload tick
  the daemon demotes a real-time worker while the audio thread is demoted,
  and promotes it again (through rtkit, retried every 30 s when refused)
  once the audio thread is real-time again.
- Measuring: `cargo run --profile fastrel -p audivane-dsp-core
  --example bench_channel_workers -- 16 8388608` times a block with and
  without the worker and checks the two outputs are identical. On one
  test machine, 44.1 kHz ×16 with an 8 Mi-tap kernel goes from 94 %
  of a block to 49 % (1.9×); light chains lose about 0.1 ms per block to
  the handoff.

### Overload

A chain that costs more than real time must never take the engine down.
rtkit grants real-time scheduling (`SCHED_RR`; the guard below counts it
and `SCHED_FIFO` alike) only with `RLIMIT_RTTIME` set (hard limit
`RTTimeUSecMax`, 200 ms). The kernel counts the CPU time a real-time thread
uses without sleeping (reset only when it wakes from a sleep), sends
`SIGXCPU` at the soft limit (then raises the soft limit by 1 s), and
`SIGKILL`s the process at the hard one. An overloaded audio thread never
sleeps (the device buffer is always short), so with soft = hard the process
would be killed 200 ms in, restarted with the same config by systemd, and
killed again until the unit's start limit stopped it.

- Prevention (`io_alsa::RunGuard`, in the RT loop and after its wait):
  the thread's voluntary context switch count tells when it last really
  slept. After 50 ms without one, and only while the thread is real-time,
  it sleeps 100 µs, which resets the kernel's count (any sleep that blocks
  does, and a real-time thread has no timer slack; the RT test checks that
  every nap blocks and that 400 ms of spinning with them never reaches a
  30 ms soft limit): an overloaded thread stays real-time and keeps playing
  (with underruns) for 0.2 % less work. 50 ms is half the soft limit and
  counts work only (a period that waits for the device sleeps in it).
- Backstop (`io_alsa::rt`): the soft limit is half the hard one
  (100 ms), with a `SIGXCPU` handler. The kernel sends the signal to the
  process and delivers it to the thread that ran over, or to any other
  thread when that one blocks it, so the handler demotes the thread it runs
  on when that one is real-time, the registered audio thread by its id
  (`register_audio_thread`), and every registered helper by its id
  (`register_helper_thread`: the channel workers), each with one
  `sched_setscheduler` call
  keeping `SCHED_RESET_ON_FORK` (an unprivileged thread cannot clear it).
  It counts the signal and, separately, each demotion of the audio thread
  the kernel accepted. Signal masks are left alone: blocking `SIGXCPU`
  everywhere but on the audio thread would route it there even when a
  PipeWire data thread ran over, and that one would reach the kill. One
  very long `process` call is demoted at 100 ms, 100 ms before the kill; a
  `SCHED_OTHER` thread is not counted. The RT loop then makes sure it is
  demoted, keeping three counts apart up to the control plane: signals,
  real real-time to normal transitions (the only overloads), and
  demotions the kernel refused; a signal that finds the thread at normal
  priority already is no demotion. It restores the soft limit the kernel
  raised; the driver's control thread keeps the soft limit at half and promotes the
  thread again through rtkit after 2 s under 80 % load with no xrun.
- PipeWire contexts (`io_pw::rt_context`): PipeWire's RT module, which
  the default `client.conf` loads for client contexts from PipeWire 1.4.0
  on (1.0.5 and 1.2.8 do not; 1.0.5 loads it in `client-rt.conf` only),
  sets `RLIMIT_RTTIME` for the whole process, through rtkit to soft =
  hard. Every context the engine and the daemon create names a one-section
  override config (`config.override.prefix` and `config.override.name`, a
  file in `$XDG_RUNTIME_DIR/audivane-dsp/pipewire/`) with `module.rt.args =
  { rt.time.soft = <hard/2> rt.time.hard = <hard> }`, so the module sets
  the engine's own limits and the data threads stay real-time.
  (`rlimits.enabled = false` would not help: the rtkit path sets the limits
  regardless.) The periodic repair stays as the second line.
- Fallback (`daemon::control::overload`): a second of load at or above
  real time, underruns in 10 % of the processed periods, or a latched
  demotion makes the engine run, as staged steps committed only once the
  engine accepts them and only where the device can play them: while
  varispeed's stage runs, first its steps (the budget pinned at half the
  period, the speed held at 1×, the stage retired; a hold the engine takes
  through `Engine::set_varispeed_hold`, with no config change, so the
  attempt is the running chain's generation under the hold's sequence,
  which lands once the audio thread acknowledges it, and later steps keep
  the stage off), then A/B off (keeping chain A), then the last upsampler
  that ran in this daemon run, then the upsampler off, then bypass (through
  the engine bypass resampler's resample-only chain where the DAC lacks the
  graph rate), and only when bypass does not land (every DAC rate above the
  graph rates) the bypass resampler's design at the requested rate. Nothing
  is judged until the step's chain runs (or ten seconds pass). Each config
  accepted for the step in effect, and each new DAC behind the same name, is
  an attempt with its own readiness, timeout and confirmation: an `Error`
  fails it only after the device answered a probe during it (the driver
  re-probes after a failed open and drops stale capabilities), and ten
  seconds in `Switching` with a client streaming fails it. A failed attempt,
  a refusal or a lost landing moves to the next step that plays (lighter
  first, then from the top, each once per device and policy); when none is
  left the fallback ends, retried while the engine refuses. The startup
  config is judged like an edit, so it can be the one step 1 goes back to. A
  demotion is published with the generation that ran on the audio thread
  (`EngineAudioProcessor::running_generation`: the active chain or one
  warming up next to it) and counts against that chain only. The notice
  names the cause (load, underruns, demotion). The `SIGXCPU` handler is
  installed when the process starts, before any filter design. The saved
  config is never touched, so a restart with the same config falls back the
  same way.
- Admission (`daemon::control::cpu_budget`): a new upsampler is
  estimated per runtime path (the design's `Plan::callback`,
  `design::budget::Callback`) where the driver would land it (its rate
  policy and ratio ladder, `engine::predicted_landings`; integer FFT
  polyphase: blocks of partitions × branches × bins plus `1 + r`
  transforms, and with non-uniform partitions the heaviest callback: each
  tail stage's period sliced as the runtime slices it, its jobs priced per
  kind at the measured FFT and spectral rates, the heaviest callback-long
  window anywhere in the period plus a 15 % margin; rational: output rate × taps per output frame; cascades,
  interpolators and block FFT from their structure) against kernel rates
  measured at engine start (`dsp_core::cpu_bench`, ~0.2 s; measured again
  when under a conservative floor, then floored), and refused above 90 % of
  a core (`RT_BUDGET`, published as `status.limits.cpu_share`
  with the tap limits and the design workspace cap). Measured against real callbacks (stereo, 1024-frame
  blocks, optimised build): 0.9 to 1.2x for the FFT engine, cascades, table
  sinc and block FFT, about 2x (on the safe side) for the rational runtime,
  the recursive cascades and the polynomial interpolators. On non-uniform
  partitions (1M to 8M taps, 44.1 to 192 kHz) the estimate is 1.4 to 1.6x the
  measured mean and 1.15 to 1.4x the 99th percentile of a callback (held to
  at least 1x).

### Known limitations

Rare timing cases the driver does not handle yet:

- An underrun during the carry's room check: if ALSA reports an underrun
  while the driver asks how much room there is (`avail()` recovers by
  preparing the device), the carried frames go into the prepared device and
  the following prefill discards them.
- A missing clock read counts as a landing: when the PipeWire clock cannot be
  read, the driver assumes the graph runs at the opened rate, which counts as
  the graph having followed a move. An unstable clock can so restart the 3 s
  give-up of a pinned graph more than once.

### Volume

- Master volume in hardware (the DAC's mixer), in `f64` before the quantizer,
  or both (`hybrid`: hardware at its best-sounding level, digital for the
  rest), with a hard ceiling applied in the engine (within 0.02 dB of
  mixer rounding). The digital part applies
  on the Direct and bypass paths too, so Direct is bit-perfect only while it
  is unity (below it, they are dithered to the DAC's depth). A hardware
  raise waits until audio rendered at a louder digital gain has played
  out. When the engine lets go of the DAC (release, a ReserveDevice1
  request, stop, or `ExecStopPost=` after a crash) it brings the card down
  to the heard level before handing it over (below the card's floor it
  mutes the playback switch, in every mode), and it never writes the mixer
  while the DAC is not held. Until the first open the audio thread holds
  the whole stored level as digital gain, never unity.
- The desktop's volume for *Audivane DSP* can drive the engine volume
  (`sink_volume: follow`): PipeWire passes the samples at unity (its
  `channelmix.max-volume` held at 1.0) and the engine applies the level
  once; *Audivane Direct* shows 100 %.
- A guard reverts (`lock`) or caps and slows down (`limit`) volume rises of
  app streams and the sink. Details: [control-plane.md](control-plane.md).

### Dither

- Applied exactly once, at the final quantization to the DAC's integer format:
  TPDF by default, noise shaping optional.
- In bypass it runs only where the engine applies digital volume; at unity the
  path is bit-exact and no dither is added.

### A/B level matching

- Both chains run in parallel all the time, so switching is a ~10 ms crossfade
  with no gap (a 5 ms fade out then in when their delays differ).
- Level matching is static per comparison, never a moving AGC (which would
  pump). It never boosts: for B's offset `o`, A plays at `min(0, −o)` dB and
  B at that plus `o`. A shared headroom (`auto` from each side's predicted
  peak gain, or fixed) then cuts both sides equally, so B's own boosts do
  not clip and the comparison stays fair.
- Default (analytic): integrate |H_A(f)|² and |H_B(f)|² over a 20 Hz–20 kHz
  pink reference, K-weighted (BS.1770, as LUFS; A-weighting optional). The
  terms: DC block, EQ, stereo image at correlation 0.5, the IRs' magnitude,
  and the upsamplers' when their designs differ. It matches LUFS on that
  reference to about 0.01 dB.
- Alternative (measured): live track matching via `ab_measure`, or offline
  via `avdsp levelmatch --measure` over a file.

## Stack and libraries

The engine is Rust and runs as a `systemd --user` service. The UI is Tauri 2 +
React/TypeScript.

| Area | Choice | Notes |
| --- | --- | --- |
| PipeWire | `pipewire` (pipewire-rs) | Driver node, sinks, metadata, routing |
| ALSA direct | `alsa` (alsa-rs) | hw params, buffer position for the clock |
| FFT | `rustfft` + `realfft` | f64, MIT/Apache; FFTW avoided (GPL) |
| Real-time safety | `rtrb` | Lock-free rings; filter chains are rebuilt off the RT thread and swapped in |
| D-Bus | `zbus` | `ReserveDevice1`, rtkit (RT priority), MPRIS |
| File I/O | `hound` | Impulse responses, test WAVs |
| IPC | Newline-delimited JSON over a Unix socket | See [control-plane.md](control-plane.md) |
| UI | Tauri 2 + React/TypeScript | Canvas meters and plots; tray via StatusNotifierItem |
| Packaging | `.deb`, `.rpm`, Arch package, AppImage + systemd user unit | Flatpak is a poor fit for direct ALSA access and a user service |

**Licensing (AGPL-3.0-only):** PipeWire libraries are MIT; alsa-lib is LGPL,
fine to link dynamically.

## Repository layout

One Cargo workspace (`dsp-core` has no I/O, so all DSP can be tested offline
against scipy/numpy reference output); see [DEV.md](../DEV.md) for the crate
table, build profiles and test suite.

## Measured automatic filter length

`dsp-core::design::auto` runs during chain construction, before audio
processing. An automatic-length design (`length: { rule: "auto", budget }` in
`FilterDesign`, including the `"recommended"` preset) searches centred
windowed-sinc lengths and measures the final phase-converted response, with
32x FFT sampling and exact band edges. The search starts near the
window-formula length estimate and brackets and bisects from there (a few
dozen measurements at most); configs whose estimate is past the length
limits are rejected in validation, which designs every ratio the engine
can build the config at (clamped by `engine::engine_ratio`) and every output
word length. Results are cached per process, keyed
by everything the design depends on but not the sample rate, so builds at
the same ratio (A/B sides, fallback rebuilds, rate changes that keep the
ratio) do not repeat a search. The
runtime still receives an ordinary prebuilt FIR and reports its actual
latency. Output-dependent budgets resolve before construction and before
caching response measurements.
