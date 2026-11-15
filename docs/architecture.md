# Audivane DSP: architecture

Linux with PipeWire only. How to build and test is in [DEV.md](../DEV.md); the
wire protocol is in [control-plane.md](control-plane.md); how the engine drives
PipeWire from the DAC clock is in [pipewire-driver.md](pipewire-driver.md).

A desktop app with a background engine that applies high-quality DSP to
system-wide or single-app audio. With processing off, its output is
bit-perfect, and the app checks and shows that.

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
| Upsampling | Built in; linear or minimum phase, up to 4M taps |
| A/B | Compare two EQ profiles with A-weighted level matching; optional blind mode |
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
| Two clocks (virtual sink + DAC, CamillaDSP-style) | Yes | Corrected by adaptive async resampling | Yes | Rejected: permanent async resampler in the path |
| **Engine is the driver** | Yes | None: DAC clock paces the graph | Yes, plus our own dither | **Chosen** |

What this gives us:

- The graph runs at the source rate; the engine writes to ALSA at source × N
  on integer ratios (44.1 → 176.4 / 352.8 / 705.6, 48 → 192 / 384 / 768).
- Integer output (S24/S32) with our own TPDF or noise-shaped dither, applied
  exactly once.
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

 System audio ──────► Audivane DSP ───────────► EQ (f64 biquads)
 (browsers, games)    (f32 in)                    │
                          ▲                       ▼
 Music player ────────────┤                     Convolution (FFT)
 (routed per app)         │                       │
        │                 │                       ▼
        │                 │                     Upsampler ×N
        │                 │                       │
        │                 │                       ▼
        │                 │                     Dither, once ──────────► ALSA hw:
        │                 │                                             rate × N
        └───────────────► Audivane Direct ────► Bypass: exact copy ───► S24 / S32
                          (no DSP)                                          │
                              ▲                                             │
                              └──── DAC buffer position drives each ────────┘
                                    graph cycle (one clock, no drift)
```

## Feature design

All DSP runs in f64 from the sink to the final quantizer. The audio thread
never allocates, locks, or does I/O; filters are rebuilt on a worker thread
and swapped in lock-free.

### Input and routing

- Two sinks: **Audivane DSP** (full chain) and **Audivane Direct** (no DSP,
  bypass path).
- System-wide: set `default.audio.sink` in PipeWire metadata.
- Single app: move only the chosen streams with `target.object`, as
  pavucontrol does.
- Read MPRIS to show the current track and its native rate.
- Input through the graph is f32, which is exact for sources up to 24-bit.
  True 32-bit-integer passthrough would need a raw-format input path; deferred.

### Bit-perfect bypass

- Bypass copies samples straight through: no f64 round trip, exact
  f32 → S24/S32 conversion.
- The UI shows **passthrough: intact**, or **broken because** volume ≠ 1.0,
  streams are mixing, the rate differs, or DSP is on.
- Self-test: play a known signal and hash the bytes handed to ALSA. Users with
  S/PDIF loopback can also hash at the DAC input.

### Sample rate and clock

- When one stream plays, the graph switches to its rate; the engine reopens
  ALSA at that rate (or rate × N when upsampling).
- Filters are recomputed for each new rate: biquad coefficients, resampled
  impulse responses (cached per rate), upsampler kernels.
- Mute ramp around each switch, since the DAC needs time to relock.
- Two streams at different rates: one gets resampled; the UI says which.

Which rate wins is decided by `crates/io-pw/src/rate_arbiter.rs`, a pure
function of the running clients, the current rate and the time since the
last switch (unit-tested without PipeWire):

- **Direct first.** Whatever plays into Audivane Direct (bit-perfect) decides,
  and takes priority over the DSP sink.
- **Agreeing clients are followed.** Each client's native rate is mapped to
  an allowed rate; if all running DSP-sink clients map to the same one, the
  graph runs at it.
- **Disagreeing clients** follow `rate.conflict`: `keep_current` (default)
  keeps the current rate while any client still plays at it, otherwise the
  client that *started running first* wins (a per-client start sequence
  number, never map iteration order); `highest` takes the highest native
  rate; `default_rate` uses the default rate while they disagree.
- **Minimum dwell.** No switch within 2 s of the previous one. A switch that
  is still wanted when the dwell ends is made then (the PipeWire thread's
  100 ms timer re-checks), so it is delayed, not dropped.
- **Our own fallout is ignored.** A switch reopens ALSA and makes clients
  pause and resume. For 1.5 s after a switch (longer while the driver still
  reports `Switching`, at most 6 s), a client that pauses and resumes keeps
  its place in the start order, and no new switch is made while the driver
  is still switching.
- Each distinct decision is logged once at info level with its reason, e.g.
  `Rate: kept 48000: Brave still playing at 48000; others are resampled`.

Without these rules, two apps at 44.1 and 48 kHz flip the graph back and
forth forever: each switch pauses and resumes the clients, and whichever
the registry map yields last wins the next round
(`tests/daemon/engine_rate_conflict.sh` covers this on the loopback rig).

### EQ

- RBJ cookbook biquads in f64; transposed Direct Form II, or Direct Form I for
  low-frequency, high-Q filters.
- AutoEQ / Equalizer APO import.
- Layered sections: linked (both channels) plus optional left / right.
- Automatic preamp from the peak boost, plus a clip and inter-sample-over meter.

### Convolution

- Non-uniform partitioned FFT convolution: small first partitions for low
  latency, larger ones for long IRs.
- On each rate change, resample the IR offline at high quality and swap it in.

### Upsampling

- Kaiser-windowed sinc kernels designed in f64, linear or minimum phase.
- FFT-based (zero-stuff, then FFT-convolve at the output rate). Direct
  polyphase with 100k taps at 16× costs about 9 GMAC/s, which is too much CPU.
- Kernels up to 4M taps. Latency grows with length (1M linear-phase taps at
  705.6 kHz ≈ 0.7 s), which is why minimum phase is offered too. The app
  shows the engine's measured response for the chosen design.

### Dither

- Applied exactly once, at the final quantization to the DAC's integer format:
  TPDF by default, noise shaping optional.
- Never applied in bypass.

### A/B with A-weighted level matching

- Both chains run in parallel all the time, so switching is a ~10 ms crossfade
  with no gap.
- Level matching is static per comparison, never a moving AGC (which would
  pump).
- Default (analytic): integrate |H_A(f)|² and |H_B(f)|² weighted by
  A-weighting and a pink-noise reference spectrum; the difference is a fixed
  offset in dB.
- Alternative (measured): A-weighted RMS of both outputs over the current
  track. K-weighting (BS.1770 / LUFS) offered too.

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
| Packaging | Arch package + systemd user unit | Flatpak is a poor fit for direct ALSA access and a user service |

**Licensing (AGPL-3.0-only):** study CamillaDSP (GPL-3) but don't copy its code.
PipeWire libraries are MIT; alsa-lib is LGPL, fine to link dynamically.

### Build speed

- Keep `dsp-core` small and dependency-light; it changes most often.
- Always optimize DSP code, even in dev builds. Unoptimized Rust numeric loops
  are too slow for real-time audio:

  ```toml
  [profile.dev.package.audivane-dsp-core]
  opt-level = 3

  [profile.dev.package."*"]
  opt-level = 2   # dependencies compile once, so this is cheap
  ```

- Release: `lto = "fat"`, `codegen-units = 1`. `--profile fastrel` (thin LTO,
  incremental) is for local benchmarking.

## Repository layout

One Cargo workspace, separate from Audivane ABX. `dsp-core` has no
I/O, so all DSP can be tested offline against scipy/numpy reference output.

| Crate | Responsibility |
| --- | --- |
| `crates/dsp-core` | Pure DSP: biquads, convolver, resampler, dither, loudness / level matching |
| `crates/protocol` | serde types shared by the daemon and the UI |
| `crates/io-pw` | PipeWire sinks, driver node, stream routing, metadata |
| `crates/io-alsa` | Direct DAC output, clock source, `ReserveDevice1` |
| `crates/daemon` | Wires it together; systemd user service |
| `crates/cli` | Offline WAV in → WAV out for tests and null tests |
| `app/` | Tauri 2 + React UI |
