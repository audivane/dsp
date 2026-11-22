# Daemon control plane

This note records how the `audivane-dsp` daemon's control side works, what the
wire protocol does beyond `crates/protocol/src/ipc.rs`, and why a few choices
were made. The engine half (ALSA + PipeWire driver + DSP) is reached only
through `crates/daemon/src/api.rs::Engine`, so everything below is testable
with a mock engine.

```
avdsp ctl  ──unix socket──▶  control::ipc ──▶ control::service ──▶ Engine
                                  │                 │
                                  │                 ├── control::config   (persist, /proc devices)
                                  │                 ├── control::persist  (coalesced writer)
                                  │                 ├── control::presets  (preset files)
                                  │                 ├── control::graph    (PipeWire)
                                  │                 └── control::mpris    (D-Bus)
                                  └── Events: meters @30 Hz; status/streams/now_playing/config/presets/devices on change
```

## Socket

- Path: `--socket`, else `$AUDIVANE_DSP_SOCKET`, else
  `$XDG_RUNTIME_DIR/audivane-dsp.sock`, else `/tmp/audivane-dsp-<uid>.sock`.
  Paths longer than 107 bytes are rejected (`sun_path`).
- Mode `0600`. A stale socket file (nothing answers a `status` probe within
  300 ms) is removed; a live daemon makes the new one refuse to start. The
  daemon probes the socket first thing, before it reads the config or opens
  the device, so a second copy started by hand exits with "Audivane DSP is
  already running (systemctl --user status audivane-dsp)" instead of failing
  the device reservation against the running one.
- `status.version` is the engine's package version. The app compares it with
  its own and, after an upgrade, offers to restart whichever side is older;
  `avdsp upgrade` waits for it to equal the installed release.
- Newline-delimited JSON, one object per line, cap **1 MiB** per line. An
  over-long line gets a `PARSE` error and the connection is closed (there is no
  way to resynchronise).
- Errors: `PARSE` (-32700, unparseable JSON), `INVALID_REQUEST` (-32600,
  missing/unknown `method`, missing `id`), `INVALID_PARAMS` (-32602,
  validation), `INTERNAL` (-32603), `BAD_STATE` (1), `IO` (2),
  `STALE_INSTANCE` (3) and `STALE_REVISION` (4), both from
  `set_volume_guarded`.
- `subscribe` takes any subset of `meters`, `status`, `streams`,
  `now_playing`, `config`, `presets`, `spectrum`, `devices`. The server sends a `status`/`streams`/
  `now_playing`/`config`/`presets`/`devices` snapshot immediately after the reply, then
  meters at 30 Hz and the others only when they change. Each connection has a
  bounded outbound queue (256 lines); a client that stops reading is
  disconnected instead of stalling the daemon.
- `config` events carry the whole `EngineConfig` after any change from any
  client (including preset load/save, which update `active_preset`); `presets`
  events carry the sorted preset list after a create, delete, rename, import or
  save. Both are detected with generation counters (a new subscriber may see
  one duplicate event on the first pump tick).
- `status` subscribers also get an `overload` event (`OverloadReport`) each
  time the engine falls back after a DSP overload (see "DSP overload").
- `devices` events carry the playback device list (`DeviceInfo[]`, as
  `list_devices` returns it) when it changes: a sound card came or went (a
  USB DAC plugged in or out) or the configured device changed. The daemon
  reads `/proc/asound` once a second while someone subscribes.
- Requests are answered in order on their connection; clients may pipeline.
  `shutdown` replies first, waits ~20 ms and only then stops the daemon.

`avdsp ctl` builds exactly these lines. `avdsp ctl watch --json` prints event
lines verbatim, which is handy for scripting. Without `--json`, `watch` prints
human-readable text: meter lines update in place on a terminal and become one
newline-terminated line each when stdout is redirected; status, stream,
"now playing", config and preset events are always printed as their own lines.
`watch <topic>` accepts `meters`, `status`, `streams`, `now-playing`, `config`,
`presets`, `spectrum` and `devices`; without a topic it subscribes to all
but `spectrum` and `devices`.

## Config file

`$XDG_CONFIG_HOME/audivane-dsp/config.json` (override `--config`). Written
atomically: temp file in the same directory, `fsync`, `rename`, directory
`fsync` (best effort).

```json
{ "version": 1, "config": { "device": "hw:0,0", "...": "..." } }
```

- A bare `EngineConfig` object (no envelope) is accepted, so hand-written
  files keep working. Unknown fields are ignored at both levels. A version
  newer than the daemon understands is read with a warning.
- A file that is not valid JSON, or that fails validation, is moved to
  `config.json.corrupt-<unix-seconds>` and the defaults are used (logged);
  the old content is never silently discarded.
- Defaults: `quantum` 1024, `rate.default_rate` 48000 with the 44.1/48 kHz
  families in `rate.allowed_rates`, `rate.conflict` `keep_current`, formats
  `S32, S24, S24_3, S16`, `device_lost` `pause`.
- `device_lost`: what the music does while the output device is gone
  (see "Devices"): `"pause"` (default) pauses the players that are playing
  through MPRIS when the device goes, `"keep_playing"` lets them play on
  into silence.
- An empty `device` is filled with the first `/proc/asound/pcm` playback
  device that is not HDMI (`Loopback` cards are only used as a last resort).
  Nothing is ever opened just to pick a default.
- Validation (`control::config::validate`) is shared by every mutation:
  `hw:` device, non-empty formats, power-of-two quantum `32..=8192`
  (`MIN_QUANTUM..=MAX_QUANTUM`, shared with the engine), rates from
  the supported list with `default_rate` included, the upsampler rules below,
  bands ≤ 4096 per channel
  with sane frequency/Q/gain (applied to both left and optional `right` EQ),
  optional `dc_block` (`cutoff_hz` 1..=20 Hz, default 5 Hz),
  optional `crossfeed` (`cutoff_hz` 300..=2000 Hz, `feed_db` 1..=15 dB),
  optional `harmonics` (`h2_db`, `h3_db`, optional `h4_db`/`h5_db` in −120..=−20 dB),
  every enabled convolution IR file must exist (combined enabled length capped
  at 4 000 000 taps in the engine), A/B side B must have the same `target`,
  resolve to the same ratio at every allowed rate and use the same output
  format/dither as side A, manual level offsets within ±60 dB,
  `volume.volume_db` within −120..=0 dB, `volume.balance_db` within ±24 dB,
  `varispeed.speed` finite in `0.5..=2.0`, `varispeed.ramp_ms` finite in
  `0..=10000` (`varispeed.keep_pitch` is checked against the running rate
  by the engine, see [Varispeed](#varispeed)), and when `varispeed.enabled` is true the static varispeed +
  rational-chain MAC/s budget across `rate.allowed_rates` (small quanta stay
  allowed; the scheduler caps the speed range at run time).
- Upsampler `target` (`RateTarget`, JSON externally tagged):

  | JSON | Meaning |
  |---|---|
  | `"off"` | no upsampling |
  | `{"ratio": 4}` | integer ratio, 1..=64 |
  | `{"max_rate": 384000}` | highest power-of-two multiple of the graph rate at or below the cap (8000..=3072000, ≥ 44100; 3072000 gives ×64 from 44.1 and 48 kHz) |
  | `{"max_rate_any": 768000}` | stay in the source's rate family when possible: the highest integer multiple of the graph rate the DAC plays (in one of the configured `formats`) at or below the cap (up to ×64). Cross to the other family (a fractional ratio) only when no integer multiple above ×1 is playable, or when the other family's best rate is at least twice the same-family best: 44.1 kHz on a DAC with 176.4 and 192 kHz gives ×4; on a 48/96/192 kHz-only DAC it gives 44.1 → 192 kHz. Same range as `max_rate`; without a device probe it resolves like `max_rate` |
  | `{"rate": 48000}` | one explicit output rate, one of the engine rates (44.1k … 768k) or the high output rates 1411200, 1536000, 2822400 and 3072000 (`HIGH_OUTPUT_RATES`): fractional (44.1 → 48 kHz, L/M = 160/147), decimating (96 → 48 kHz), integer (44.1k → 2.8224 MHz is ×64) or ×1 at the graph rate itself |

  The ratio is `output / input = L/M`, reduced (`RateRatio { up, down }`).
  Both terms must stay ≤ 4096. Every pair of engine rates up to 768 kHz
  does; of the high rates, 44.1 kHz → 1.536 MHz (5120/147), 44.1 kHz →
  3.072 MHz (10240/147) and 88.2 kHz → 3.072 MHz (5120/147) do not, so a
  `{"rate": 1536000}` or `{"rate": 3072000}` target is refused while those
  source rates are in `rate.allowed_rates` (`at 44100 Hz the ratio 10240/147
  has a term above 4096`), and `max_rate_any` never offers those pairs.
  **Known limitation:** 44.1 kHz → 1.536 / 3.072 MHz and 88.2 kHz →
  3.072 MHz are refused for that reason; the same-family integer ratios
  work (44.1 kHz ×32 = 1.4112 MHz, ×64 = 2.8224 MHz; 48 kHz ×32 / ×64;
  88.2 kHz ×32 = 2.8224 MHz). To run a fixed 1.536 or 3.072 MHz, turn the
  44.1 kHz family's rates that cannot reach it off in `rate.allowed_rates`.
  When the device cannot play the output rate the open falls back to ×1
  (for a fractional ratio) or down the integer ladder.
- Output rates: integer ratios go up to ×64 (`MAX_INTEGER_RATIO`) and output
  rates up to 3.072 MHz (`MAX_OUTPUT_RATE`). Above 768 kHz the engine only
  opens 1.4112, 1.536, 2.8224 and 3.072 MHz (×32 and ×64 of 44.1 and
  48 kHz); other multiples (44.1 kHz ×24 = 1.0584 MHz) are no DAC rate and
  are skipped on the ladder (44.1 kHz ×48 lands at ×32). A DAC that does not
  report a rate is never opened at it.
- Upsampler wire shape (`UpsamplerConfig`) and validation (`validate_upsampler`,
  also applied to an explicit `upsampler_response` config):
  - `UpsamplerConfig` holds `{ target, preset, design }`:
    - `target: RateTarget` (`"off"`, `{"ratio": N}`, `{"max_rate": HZ}`,
      `{"max_rate_any": HZ}`, `{"rate": HZ}`).
    - The fixed headroom is `output.headroom_db` (see "Output headroom");
      the upsampler has none of its own.
    - `preset: Option<String>` (default `null`): a named catalogue entry — any
      of the 66 `FilterPreset` names below or `"recommended"`. Mutually
      exclusive with `design`. When both are `null`, the engine runs
      `"recommended"`.
    - `design: Option<FilterDesign>` (default `null`): a full filter design
      tagged by `"kind"`, with an optional informational `"based_on"` string.
      The schema below is the wire form; `crates/protocol/src/design.rs`
      documents each field.
      - `"windowed_sinc"`: `window`, `length`, `decimation`, `cutoff`,
        `phase`, `preserve_samples`, `rates`, `numerics`.
      - `"lanczos"`: `lobes` (taps per input sample are `2a + 1`), plus
        `decimation`, `phase`, `preserve_samples`, `rates`, `numerics`.
      - `"cascade"`: `{"stage": "fir" | "fir_minimum" | "fir_asymmetric" |
        "frequency_sampled" | "elliptic" | "inverse_chebyshev"}`.
      - `"interpolator"`: `{"method": "cosine" | "catmull_rom" | "table_sinc"}`.
      - `"block_fft"`: `{"block": {"input": B} | {"output": P}, "overlap":
        "half" | "quarter_average"}`.
      - `window` (tagged by `"type"`): `kaiser` (`beta`, or `null` to derive
        it from the band attenuation), `gaussian` (`alpha` or `stopband_db`, not
        both), `flattop` (`set`: `hft196d`, `hft223d`,
        `hft248d`), `chebyshev` (`sidelobe_ratio`, edited in dB),
        `nuttall`, `hann`, `tukey` (`taper` in (0, 1]; needs linear
        phase and a fraction cutoff). Every window takes `sampling`:
        `periodic`, `symmetric` (default) or `integer_centre`.
      - `length` (tagged by `"rule"`): `fixed {taps}` (output frames),
        `per_input {taps}` (taps per input sample), `output_span {taps}` (the
        `"ladder"` or an output-rate tap count), `band`, or `auto {budget}`.
        A `taps` or cutoff `value` may be a stepped value,
        `{"by_source_rate": [[50000, 167], [100000, 83], [null, 63]]}`.
      - `decimation`: `{"length": "scale", "odd": "down" | "up",
        "base_taps": N}` or `{"length": "keep"}`.
      - `cutoff` (tagged by `"mode"`): `fraction {value}` (the -6 dB point),
        `nyquist_stop`, or `band {passband_end, stopband_start,
        attenuation_db}`.
      - `phase` (tagged by `"type"`): `linear`, `minimum {method}` or
        `intermediate {mix, method}`, with `method` `reflect` (default) or
        `cepstral` and `mix` counting toward linear phase (0 minimum, 1
        linear).
      - `rates`: `accept` (`integer` | `upsampling` | `any`) and an optional
        `fallback_window {when: "not_multiple" | "decimating", window}`.
      - `numerics`: `path` (`standard`, `fir`, `fir_after`,
        `exact_pi_inverse_after`, `exact_over_ratio_before`,
        `exact_pi_inverse_pairwise`, `double_double`) and `grid_cutoff`
        (`scaled` or `design_ratio`). The path selects one of the kernel
        builders unchanged; it is not a set of independent switches.
      - Omitted fields take their defaults: `decimation` `keep`, `phase`
        `linear`, `preserve_samples` false, `numerics` `standard` with
        `design_ratio`, and `rates.accept` derived from the shape (`integer`
        for `output_span`, `upsampling` for a stepped length, `lanczos` or
        `preserve_samples`, `any` otherwise).
    - No config migration: older flat `config.json` files without `preset`/`design`
      fall back to default config on load (and `set_upsampler` expects the v2
      shape).
  - Integer lengths are checked at the largest
    integer ratio the target reaches; fractional and decimating ratios
    (`down > 1`) are checked at each rate of `rate.allowed_rates` (at the
    measured `source_rate` for `upsampler_response`, at none for a preset
    file, which is checked when it is loaded), since a design that is cheap
    from 96 kHz can be far too expensive from 768 kHz. The error then names
    the rate and suggests removing it from `allowed_rates`.
  - Length limits: `fixed` 1..=8 388 608 output-rate taps at the worst ratio
    the target reaches; `per_input` 1..=65 535 taps per input sample, with
    `taps × up` within the tap budget at every reachable ratio;
    `output_span` `"ladder"` or a power of two 2^12..=2^22; `lobes`
    1..=65 535 with `(2a + 1) × up` within the budget; an `auto` budget
    55..=200 dB or `"match_output"`; cutoff `fraction` in (0, 1], stepped
    values too.
  - A `band` cutoff takes `passband_end` 0.5..=1, `stopband_start` up to 1.5
    with passband end below it (fractions of the source Nyquist), an
    attenuation of 55..=225 dB, and a length the engine can build
    (`band_design`) of at most 8 388 608 taps. `nyquist_stop` must be
    feasible (`nyquist_stop_check`).
  - Building a design may use at most the machine's design workspace (a
    quarter of the total RAM, 1.5 to 4 GiB, read once at start-up;
    `status.limits.design_workspace_bytes`). Over it, validation refuses with `designing the
    <what> needs about <X> GB of memory, more than the <Y> GB
    design limit: use a shorter filter or a lower ratio`. A reflected
    minimum phase above 2 097 152 taps needs more than 1.5 GiB; at 4 GiB
    every allowed length fits.
  - Kaiser `beta` 0..=40; `phase` `minimum` and `intermediate` need beta
    2..=20 (band: attenuation ≤ 190.2 dB). `intermediate` with 0 < `mix` < 1
    needs a design of at most 2 097 152 taps: its kernel can grow to 4x the
    design length (about 1.45x at beta 14, 3.1x at beta 2). `mix` itself is
    0..=1. A Gaussian `alpha` is 0.05..=0.5; a Gaussian `stopband_db`
    55..=225; a Chebyshev `sidelobe_ratio` 10^-220..=10^-40; a Tukey `taper`
    in (0, 1].
  - An `auto` length measures a budget instead of taking one: `{"db": 140}`
    for an amplitude-error target, or `"match_output"` to derive it from the
    output bits and dither. It uses the band edges (`passband_end` /
    `stopband_start`), overrides any other length, and is Kaiser/Gaussian
    only, without `preserve_samples`. Targets 55..=200 dB. The automatic
    mode derives beta (including above 20 for high budgets); the manual
    beta limit does not apply. Designs are kernels N = 2 k L + 1 (L the
    interpolation numerator) with k ≤ 2048 and N ≤ 4 194 304; candidates
    must meet both passband error and image level after phase conversion.
    Validation covers every ratio the engine can build the target at (what
    it resolves to over `ALLOWED_RATES`, clamped to the engine's x64 limit,
    plus the lower ratios used when a device lacks a rate that land on a
    rate the driver opens: `{"ratio": 48}` designs x48, x32, x16, x8, x4
    and x2, never x47 .. x33) and, for `match_output`, every budget the
    output resolves to under an S16, S24 or S32 container. It checks the
    length estimate first and rejects designs past those limits at once
    ("… more than the N-tap automatic limit; widen the transition or lower
    the target"), then runs the designs themselves (about 1 to 1.5 s for the
    slowest configs), naming the failing ratio and budget. At a fractional
    or decimating ratio `L/M` the kernel lives on the intermediate grid like
    a manual design: its band edges are fractions of the lower Nyquist
    (`max(L, M)` grid samples per lower-rate sample, so N = 2 k max(L, M) +
    1) and its gain is `L`. Up to `max(L, M)` = 16 the search measures that
    grid; above (most fractional ratios, 160/147 for one) k comes from the
    x16 search. For linear phase that is the real grid's shortest k;
    minimum and intermediate phase designs are measured on the real grid
    and lengthened until they pass, or refused (`at 48000 Hz (147/160) … no
    minimum phase design met … on the 160x grid …`: ratios with one grid
    scale share the design and its refusal, which names the scale; the
    check in front names the ratio). Validation designs the rational ratio
    the target resolves to at each allowed rate and checks its cost like a
    manual design; for `max_rate_any` it also designs every fractional pair
    a device can offer, and a pair whose design fails or is over the
    rational limits is left off the ladder instead of failing the config. A
    graph rate outside `ALLOWED_RATES` (forced through PipeWire metadata,
    say) is not covered: a design that fails there is a chain error at
    stream start. Designs are cached per process, so the chain builds that
    follow reuse them. `upsampler_response` at a ratio validation did not
    design (x33 .. x63 other than the target's own) designs on demand and
    returns an error if that design fails. Failure is an error, never a
    silently weaker filter.
  - `match_output` uses `6.02 bits + 1.76` dB without dither, less
    `10 log10(1 + 12 σ²)` for dither of variance σ² LSB² (4.7712 dB for TPDF
    and every TPDF-based shaped mode, 1.76 for `feedback1`, 6.99 for
    `rectangular`, 3.61 for `gaussian`, 1.90 for `highpass_tpdf`; shaped
    modes use the unshaped broadband reference: a nominal figure, stricter
    than their total noise but possibly looser than their audible-band floor
    at high rates; use an explicit `{"db": N}` to match that), capped at the
    24-bit equivalent (146.24 dB, 141.47 dB with dither) since no DAC
    resolves more; an explicit `{"db": N}` may ask for more. The response
    call resolves it against chain A's output settings (A/B configs always
    share chain A's upsampler and output), capped to the negotiated
    container; an explicit `{"db": ...}` makes a response request
    independent of them. A design always sees a resolved budget: an
    unresolved `"match_output"` there is an error.
  - `preserve_samples` (a Nyquist filter that passes the input samples
    through) needs `phase` linear, a `fraction` cutoff of 1.0 and an output
    rate at or above the input rate at every checked rate.
  - Manual designs at a rational ratio live on the intermediate grid
    (rate `input × up`): `fraction` cutoffs are fractions of the lower of
    the input and output Nyquists (decimation scales the anti-alias cutoff
    by out/in), "stop at Nyquist" and band designs place their transition
    there, and a `fixed` length counts output frames (grid length
    `taps × down`) while `per_input` counts input frames (`taps × up`). The
    grid kernel must be ≤ 1 048 576 taps (8 MiB of coefficients; the cap is
    per kernel, so an A/B swap can hold four, up to 32 MiB), and its cost
    (output rate × taps per output frame) times the pipelines that can run
    at once (2: old and new chain during a swap or crossfade; 4 with A/B)
    at most 800 M multiply-adds per second per channel. The same limits
    apply to presets.
  - A `fixed` design of 65 536 output-rate taps fits no rational
    ratio: from 44.1 kHz it would need 9.6 M grid taps at 48 kHz. The
    refusal names the limits that bind at that rate: the size limit
    (`1 048 576 / up` grid taps) and the cost limit (`800 M / (output rate ×
    pipelines)`), the smaller winning.
  - `preset` rules: a preset is a fixed named
    recipe (`"recommended"` or one of the 66 `FilterPreset` names below);
    `target` still applies.
    Every integer ratio `> 1` that `target` resolves to over `ALLOWED_RATES` must
    be in `2..=64` and supported by the preset, and every fractional or
    decimating ratio at the checked rates (above) must be one the preset
    accepts, within the same grid length and cost limits:
    - FIR and short-interpolator presets: any integer ratio `2..=64`, as
      long as the kernel stays within 8 388 608 taps at the output rate;
      the longest, `lanczos65535` (131 071 taps per input sample), is
      8 388 544 taps at ×64. A longer one would be refused with `filter
      preset <name> does not run at x<r>: its kernel would have <N> taps,
      more than 8388608; use a lower ratio or a shorter preset`.
    - Fractional and decimating ratios: single-kernel presets only (not
      `cascade_*`, the interpolators or `block_fft*`). Integer only:
      `gaussian_huge`, `gaussian_huge_apod`, `flattop_xlong_std`,
      `flattop_huge`, `flattop_mega`, `lanczos32767`, `lanczos65535`.
      Fractional upsampling but no decimation: `gaussian_compact`,
      `flattop_compact`, `kaiser_compact`, `nuttall`, `lanczos23`,
      `lanczos23_mp`, `lanczos511`, `lanczos2047`, `lanczos8191`,
      `short_leaky`, `short_leaky_mp`, `halfband_*`, `hann_nyquist`. Any
      ratio: the rest. The rate rules
      (lengths, cutoffs, windows) are part of each preset's design
      (`dsp-core::design::catalogue`).
    - `cascade_*` presets: even ratios made of factors 2 and 3 (`2, 4, 6, 8,
      12, 16, 18, 24, 32, 36, 48, 54, 64`).
    - `block_fft*` presets: power-of-two ratios (`2, 4, 8, 16, 32, 64`).
    - Preset names (`FilterPreset`, snake_case):
      `gaussian_short`, `gaussian_medium`, `gaussian_long`, `gaussian_xlong`,
      `gaussian_xlong_apod`, `gaussian_early`, `gaussian_early_ip`,
      `gaussian_early_mp`, `gaussian_nyquist`, `gaussian_nyquist_short`,
      `gaussian_compact`, `gaussian_huge`, `gaussian_huge_apod`, `nuttall`,
      `flattop`, `flattop_short`, `flattop_medium`, `flattop_long`,
      `flattop_xlong`, `flattop_xlong_apod`, `flattop_xlong_std`,
      `flattop_early`, `flattop_early_ip`, `flattop_early_mp`,
      `flattop_compact`, `flattop_huge`, `flattop_mega`, `chebyshev`,
      `chebyshev_mp`, `chebyshev_short`, `chebyshev_short_mp`,
      `chebyshev_long`, `chebyshev_long_ip`, `chebyshev_long_mp`,
      `kaiser_steep`, `kaiser_steep_mp`, `kaiser_steep_short`,
      `kaiser_steep_short_mp`, `kaiser_compact`, `halfband_kaiser`,
      `halfband_large`, `halfband_medium`, `halfband_small`,
      `halfband_tiny`, `hann_nyquist`, `lanczos23`, `lanczos23_mp`,
      `lanczos511`, `lanczos2047`, `lanczos8191`, `lanczos32767`,
      `lanczos65535`, `short_leaky`, `short_leaky_mp`, `cascade_elliptic`,
      `cascade_inverse_chebyshev`, `cascade_fir`, `cascade_fir_asym`,
      `cascade_fir_mp`, `cascade_fft`, `cosine`, `catmull_rom`,
      `table_sinc`, `block_fft`, `block_fft_fast`, `block_fft_large`.
- Chain B is **materialised from chain A before validation** on every edit
  (`SetUpsampler`, `SetLimiter`, `SetConfig`, `SetDspEnabled`, `SetEq`, `SetConvolution`,
  `SetRoutingMode`, `SetVolume`, and when the file is loaded), so changing chain A never
  fails because of side B: a patch B is re-applied to the new A, and a full chain B
  has its upsampler target, limiter and output headroom realigned. `ab set` with a full chain
  that differs is realigned rather than rejected: the reply is an `AbStatus` plus a
  `"notes"` array explaining what followed chain A (`avdsp ctl` prints each note as
  `note: …`). A config file with a stale chain B is normalized (with a warning)
  instead of being treated as corrupt. CLI: `avdsp ctl ab stage <stage>` (that
  stage off in B, or on when A has it set up but off), `ab filter
  <preset|file>` and `ab output [--bits N] [--dither KIND]` send a patch;
  `ab set <profile>` sends a full chain.
- `active_preset: Option<String>` records the preset chain A was last
  saved to or loaded from. It is part of the config, not of a preset.
- **Targeted setters.** `set_config` replaces the whole config, so a
  client's copy read before another client's change writes the old value
  back (an app editing the output format right after `avdsp ctl volume
  --hardware-db -40` would put the old hardware level back). Every edit a
  client makes has its own call that changes only its field, validated and
  normalized like any edit: `set_output` (`OutputConfig`, chain A's
  `chain.output`; chain B follows), `set_formats` (the `formats` list),
  `set_rate` (`RatePolicy`), `set_device` (`{"device": "hw:..."}`, empty
  for the default playback device), next to `set_eq`, `set_upsampler`,
  `set_dsp_enabled` and the others. `set_config` stays for whole-file
  replacement (`avdsp ctl config set`), unconditional.

  ```json
  {"id":1,"method":"set_output","params":{"bits":24,"dither":"tpdf","headroom_db":0}}
  {"id":2,"method":"set_formats","params":["s32","s24"]}
  {"id":3,"method":"set_rate","params":{"follow_source":true,"allowed_rates":[44100,48000],"default_rate":48000,"conflict":"keep_current","bypass_resampler":"engine"}}
  {"id":4,"method":"set_device","params":{"device":"hw:CARD=DAC,DEV=0"}}
  ```

### Rate policy

```json
"rate": {
  "follow_source": true,
  "allowed_rates": [44100, 48000, 88200, 96000, 176400, 192000],
  "default_rate": 48000,
  "conflict": "keep_current",
  "bypass_resampler": "engine"
}
```

- `follow_source`: switch the graph to the playing stream's native rate
  (mapped to the nearest allowed rate at or above it, else the closest).
  Off: always `default_rate`.
- `allowed_rates`: the graph rates the engine may run at, any of 44.1k,
  48k, 88.2k, 96k, 176.4k, 192k, 352.8k, 384k, 705.6k and 768k; the default
  list stops at 192k (352.8k and up must be allowed explicitly, e.g. to let
  DXD reach the DAC unresampled; upsampling then falls back to a smaller
  ratio at those rates). Every config edit and the config file are
  normalized before validation (`control::config::normalize_rates`): the
  list is sorted without duplicates, and a `default_rate` that is not in it
  moves to the nearest allowed rate (same 44.1k/48k family first, then the
  closest, 48 kHz on a tie, like io-pw `nearest_rate`), so turning off the
  default never fails; an empty list is still rejected. The app's Device
  page toggles rates and sets the default (`set_rate`, or `set_config`);
  `avdsp ctl rates [--enable HZ…] [--disable HZ…] [--default HZ]` does the
  same from a shell (no arguments: print the list, `*` marks the default). What reaches PipeWire (`clock.allowed-rates`, the sinks'
  `audio.allowed-rates` and their format pods) is the **effective** set: these
  rates restricted to what the open device can play (see "Device
  capabilities" below). A stream at a rate outside it is resampled by
  PipeWire into the graph rate, e.g. 44.1 kHz apps into a 48 kHz graph on a
  48 kHz-only USB device.
- The effective default rate is `default_rate` when the device can play it,
  else the nearest effective rate (same 44.1k/48k family first, 48 kHz on a
  tie). It is used while idle, with `follow_source` off and for
  `conflict: default_rate`.
- `bypass_resampler`: `"engine"` (default) or `"pipewire"`. Who converts the
  rate for Audivane Direct and DSP off (the bit-perfect paths) when the DAC
  does not play the graph rate natively; see "The engine bypass resampler"
  below. A rate the DAC plays natively always stays ×1 and bit-perfect,
  whatever this says. A file without the field loads as `"engine"`.
  `avdsp ctl rates --bypass-resampler engine|pipewire` sets it; the app has
  it on the Device page.

#### Device capabilities

Each time the engine opens a device (start, device switch, re-acquire after
a release; not on rate switches) it probes it on the handle it is about to
configure (no second open, so a device it holds is never `EBUSY`): for the
engine's channel count and interleaved access, every (rate, format) pair over
every allowed graph rate times every integer ratio up to the maximum DSP
ratio (64), kept to the multiples up to 768 kHz and the high output rates
1.4112, 1.536, 2.8224 and 3.072 MHz (36 rates in all,
`io_pw::lifecycle::MASK_OUTPUT_RATES`), plus the significant bits ALSA reports per
pair (`snd_pcm_hw_params_get_sbits`, e.g. 24 for a 24-in-32 S32 DAC). On
HD-Audio cards those are capped at the widest sample size the codec lists
under "Default PCM" in `/proc/asound/cardN/codec#M`, because recent kernels
report HD-Audio S32 as using all 32 bits (`MSBITS_MAX`) even when the codec
converts 24. `sbits` is informational: the app and `avdsp ctl status` show
it and the app suggests a dither target from it (at most 24 bits). Formats
are probed per rate because USB devices often tie them together (24-bit only
up to 96 kHz). The probe leaves the device unconfigured.

- A graph rate `r` is playable when the device takes `r × k` with one of
  `formats` for some `k` on the ratio fallback ladder, from the wanted
  upsampling ratio down to 1 (halving powers of two, otherwise minus one).
  A fractional or decimating ratio's ladder is that ratio, then 1;
  `max_rate_any` tries the rule's choice first, then the rest of its
  preferred family, then the other family (highest first), then 1, then
  rates below the graph rate (for a device that cannot play it).
  Every ladder skips the ratios a configured chain cannot be built for
  (an integer-only preset at a fractional ratio, a design over the
  rational cost limit counted with the config's pipelines), on either A/B
  side, at every output rate the driver probes (the multiples ×1..16 of
  the engine rates up to 768 kHz and the four high output rates, which
  continuous-range devices report too), so a cross-family rate is offered only when both designs run
  there. A config change that changes these pairs (a preset change, A/B on
  or off) reopens the device when the running pair is no longer buildable
  or a fresh open would pick another ratio. So `{"rate": 48000}` makes 96 kHz and 44.1 kHz graphs
  playable on a 48 kHz-only DAC.
- With a device probe, `set_config` and every config edit reject a
  `{"rate": R}` target the open device does not play in one of the
  configured `formats` (empty: the driver's preference), the check the
  open plan makes (`device X does not play R Hz (it supports …)`, or `X
  plays R Hz only in S16, not in the configured formats (S32) …`); without
  one (no device yet, or another device configured) anything else valid
  passes.
- Effective rates = `allowed_rates` ∩ playable. If that is empty: every
  playable rate of the engine's full list (44.1k..768k); if still empty, the
  open fails with an error naming the parameter the device rejects and what
  it does take (unfiltered by the engine's channel count and rate list):
  `device 'X' needs 2 channels; device supports 1 (engine: channels 2;
  device: rate 48000 Hz; formats S16; channels 1)`, `device 'X' supports
  32000 Hz only, which the engine doesn't run at (device supports rate 32000
  Hz; formats S16; channels 2)`, or `takes none of the engine's sample
  formats`.
- **Audivane Direct never runs the DSP chain** (it is the bit-perfect path,
  ×1), so it only runs the graph at *Direct rates* (`Status.direct_rates`):
  the effective rates the device plays natively, and, with
  `bypass_resampler: "engine"`, the rates the engine converts to a native
  one (below). A Direct stream at another rate (e.g. 48 kHz on a 96 kHz-only
  DAC whose 48 kHz graph the DSP sink plays ×2, with `"pipewire"`) moves the
  graph to a Direct rate (the stream's rate's next higher Direct rate, else
  the closest; with `follow_source` off, the nearest Direct rate to
  `default_rate` when that is not one) and PipeWire resamples the stream:
  playback continues, and `passthrough` reports `resampled` (not
  bit-perfect). When no effective rate is native, Direct cannot play on the
  device: routing a stream to it puts the engine in `error` (`Audivane
  Direct cannot play on 'X': it does not upsample, and none of the allowed
  rates (…) is native to the device …`). DSP off plays ×1 bypass on the DSP
  sink and treats rates the same way; only the DSP sink with DSP on runs the
  configured chain (integer, fractional or decimating ratios).

#### The engine bypass resampler

With `rate.bypass_resampler: "engine"` (the default), Direct and DSP off at
a graph rate `G` the DAC does not play natively keep the graph at `G`, and
the engine converts it to the native rate `O` PipeWire's move would have
picked: the configured allowed rate the DAC plays, at or above `G` (the next
higher), else the closest (any native engine rate when no configured one is
native). The conversion is a fixed ratio `O/G` (e.g. 160/147 for 44.1 →
48 kHz, 1/2 for 192 → 96 kHz) on the same frame-exact rational path the DSP
chain uses.

- **One decision.** `io_pw::DesiredDspConfig::bypass_landing(G, rates)`
  decides it, and the rate arbiter (Direct's rates), the effective
  policies, the open ladder (`[O/G, ×1]` for Direct and DSP off) and the RT
  thread's reopen check all go through it, so they cannot disagree.
- **Added on top, never instead.** Each sink's rates start from what it
  plays without the engine leg (natively, or through the DSP chain;
  widened to the device's native rates when no configured one is, as
  before), and the rates the engine converts are added on top
  (`io_pw::rate_caps::RatePolicies`, `dsp_native`/`direct_native`). An app
  runs the graph at a converted rate only when it plays at exactly that
  rate; any other rate goes to the sink's native rates
  (`rate_arbiter::sink_rate`, in the arbiter, the open planner and the
  follow rate). So a native rate is never converted, and no stream is
  converted twice (PipeWire into a converted rate, then the engine): with
  allowed rates 44.1 and 88.2 kHz on a 48 kHz-only DAC and DSP off, 48 kHz
  plays bit-perfect, 44.1 and 88.2 kHz convert in the engine, and 96 kHz is
  resampled by PipeWire into 48 kHz.
- **The design** is the app's "Recommended" one (a Kaiser band design flat
  to 0.908, 20 kHz at 44.1 kHz, and stopped from 1.00 of the lower Nyquist,
  automatic length matched to the output's resolution, linear phase), fixed, one pipeline
  (no A/B). The chain is the upsampler alone, then the digital volume
  (gain, balance, mute) and dither to `chain.output` (capped at the
  format). EQ, convolution and the other stages never run there.
- **The bypass mask.** A pair `G → O` is used only when that design builds
  for it within the rational length and cost limits
  (`control::config::bypass_buildable_rate_pairs`, cached per output
  section). Otherwise, and without a device probe, Direct and DSP off
  behave exactly as with `"pipewire"`.
- **Built off the engine's locks.** Designing and building a pair's chain
  takes from tens of milliseconds to seconds (768 → 44.1 kHz), so it runs on
  the engine's design thread (`engine::bypass_design`), never on the
  driver's control thread or under the lock volume changes need. The thread
  keeps one built chain per pair ready, built ahead for the configured rates
  the device lacks; an open or an edit only publishes it.
- **Falling back.** When an open lands on a pair whose chain failed to
  build, or is still being built, the driver holds the pair back
  (`lifecycle::BypassHeld`: a failed pair until the device changes, a pair
  being built until the engine reports it done), republishes the rate
  policies without it and opens again at a native rate (PipeWire
  resamples) before anything resumes. Start-up, reacquire and reopen share
  this (`lifecycle::open_with_bypass_fallback`). A pair whose chain is then
  built comes back, and an app at that rate moves the graph to it again.
  It never plays silence for that; `Status.bypass_resampler.reason` says
  what happened (`the engine is still building its 44100 Hz to 48000 Hz
  resampler; PipeWire converts meanwhile`, or why it failed).
- **Readiness.** The resume after the open waits for the chain like the DSP
  path's (it is gated on its swap); until then the output is silence,
  reported as waiting (`reason`), not as an error.
- **Switches.** DSP on ↔ off at the same ratio swaps the chains without a
  reopen (an edit that leaves the chain's inputs alone keeps it): the
  outgoing chain keeps playing while the incoming one warms up (for its
  reported delay, the impulse peak for a minimum-phase design, plus a
  settling margin; a chain that reports no delay still warms up for its
  length), then they crossfade, or fade through silence when their delays
  differ. When the resample-only chain is still being built the DSP chain
  keeps playing until it is. A different ratio reopens, as does a switch
  between Direct and the DSP sink (the processor holds one chain, and the
  other sink's is built by the open).
- **DSP on.** The DSP sink's own rates are unchanged: PipeWire is offered
  the DSP sink's rates and Direct's together (`Status.effective_rates`), but
  the DSP sink's apps still map through the DSP sink's rates, so the DSP
  sink never runs at a rate only Direct can play. The arbiter hears of a
  change to either sink's rates even when the union stays the same (DSP
  off adds the converted rates to the DSP sink's, which Direct may have
  published already).
- **Bit-perfect.** A native rate is never converted: Direct and DSP off at
  ×1 are an exact copy, as before. The converted path reports
  `upsampling` or `rate_conversion` and `dsp_active` in `passthrough` (its
  chain resamples and dithers, at unity volume too), and a stream PipeWire
  still resamples (a second app at another rate) keeps `resampled`.

- If PipeWire does not follow a move (another driver or client fixes the
  graph rate), each reopen is another attempt; 3 s after the first attempt
  of a move that has not landed the open fails with a plain error (`…
  PipeWire keeps the graph at 44100 Hz instead of moving it to 48000 Hz
  (another client or driver fixes the rate) …`), the engine is in `error`
  and `passthrough` reports `no_output`. The DSP path keeps working, and a
  move that lands later (a graph-rate change) plays again.
- A failed reopen never sticks: when the route or the graph rate changes
  afterwards (e.g. the app goes back to Audivane DSP) the engine retries the
  open, so a playable path recovers without a config apply.
- On open the ratio is the first ladder step the device takes, with the
  first of `formats` it takes at that rate. There is no "nearest rate"
  fallback: the DAC always runs at exactly graph rate × ratio. At a
  rational ratio the ALSA period is `round(quantum × up / down)` frames;
  each period gets the fewest input frames that fill it, and the few
  frames one input frame can complete beyond a period (fewer than
  `ceil(up / down)`) are carried into the next one, so the DAC is fed at
  exactly the output rate (the prefill keeps those past the buffer until
  the device starts). If PipeWire
  still has the graph at a rate the device cannot play (e.g. right after a
  device switch), the engine opens at the effective rate it then forces.
- A config change alone (e.g. a new filter length) never reopens the device
  when the device already fell back to a smaller ratio for the same request.
  An edit of the upsampler target, DSP on/off or the rate pairs the chain
  can run reopens only when a fresh open would land on another ratio (a
  48 kHz-only DAC asked for ×4 then ×6 stays at ×1 without a gap); a new
  device, format list or quantum always reopens.
- ALSA errors say which parameter the device rejected and what it supports,
  e.g. `device 'hw:CARD=SPK,DEV=0' does not support rate 44100 Hz (device
  supports rates 48000 Hz; formats S16; channels 2)`.
- `conflict` (only while `follow_source` is on): what to do when the apps
  playing into Audivane DSP at the same time want different rates.
  `keep_current` (default) keeps the current rate while any of them still
  plays at it, else follows the app that started playing first; `highest`
  uses the highest native rate among them; `default_rate` uses
  `default_rate` while they disagree (a single app is still followed). The
  other apps are resampled by PipeWire. Apps on Audivane Direct are not
  affected: they always decide. A file without the field loads as
  `keep_current`. The arbitration rule and its timing are in
  [architecture.md](architecture.md#sample-rate-and-clock).

### Deferred writes

`config.json` is written by `control::persist` on a dedicated thread, not in
the request that changed the config:

- a change is queued and the reply is sent immediately (live EQ dragging at
  ~30 `set_eq`/s costs one small write, not thirty);
- at most one write per **300 ms**; a queued change is never held longer than
  **2 s** (measured from the first change of a burst);
- `shutdown` (IPC, SIGTERM/SIGINT and normal exit all end in
  `Daemon::shutdown`) writes whatever is still queued and waits for it. A crash
  can therefore lose at most the last couple of seconds of config edits.
- Writes stay atomic (temp file + fsync + rename + directory fsync).

A failing write cannot fail the
mutation that queued it (the reply is already gone). It is logged at error
level, remembered, and reported as `Status.error` when the engine has no error
of its own; the next successful write clears it. Preset files are *not*
deferred — saving a preset is an explicit user action and is written
synchronously before the reply.

## Presets

`control::presets` stores chain A's signal path (EQ, convolution, upsampler,
output) in `presets/` **next to `config.json`** — so `--config PATH` moves the
config and the presets together:

```text
$XDG_CONFIG_HOME/audivane-dsp/presets/hd-650.json
{ "version": 1, "name": "HD 650", "chain": { "eq": { "...": "..." }, "...": "..." } }
```

- One file per preset, written atomically (same temp file + fsync + rename as
  `config.json`) and synchronously.
- Names are trimmed, 1–64 characters, no control characters. The file name is
  a slug (lowercase ascii, digits, `-`); the `name` field is authoritative, so
  two names with the same slug (`Rock` / `rock`) get distinct files.
- A preset holds `chain` only. Device, formats, routing, rate policy, A/B and
  `active_preset` are machine settings.
- `Status.preset = { name, modified }` reports the active preset; `modified`
  is chain A ≠ the saved chain. The service caches the active chain, so status
  never reads the preset directory.

| call | reply | notes |
| --- | --- | --- |
| `list_presets` | `Vec<PresetInfo { name, modified_unix, bands, convolution }>` | sorted by name; unreadable files are logged and skipped |
| `get_preset { name }` | `Preset` | `INVALID_PARAMS` when unknown |
| `save_preset { name, overwrite }` | `PresetInfo` | saves chain A, then makes it active; `BAD_STATE` when it exists and `overwrite` is false |
| `load_preset { name }` | `null` | validates, applies to chain A through the normal edit path (chain B is still realigned) and makes it active |
| `delete_preset { name }` | `null` | clears `active_preset` when it was the active one |
| `rename_preset { from, to }` | `PresetInfo` | follows `active_preset`; `BAD_STATE` on a name collision |
| `import_apo { name, text }` | `PresetInfo` | Equalizer APO / AutoEQ text parsed by `dsp-core`'s `eq_apo`; the preset gets that EQ with flat defaults for the rest of the chain (no convolution, upsampling off, default output), and is **not** loaded (an existing name is replaced) |
| `list_devices` | `Vec<DeviceInfo>` | see below |

`avdsp ctl preset list|show|save [--overwrite]|load|delete|rename|import FILE
[--name N]` covers all of them.

## Devices

`list_devices` (and `avdsp ctl devices`) enumerate **playback** devices from
`/proc/asound/pcm`, with card ids from `/proc/asound/cards`; no device is ever
opened to enumerate. Each entry is
`DeviceInfo { id: "hw:CARD=<card-id>,DEV=<n>", card, name, hdmi, loopback,
active }`. `active` marks the configured device (`hw:0,0`, `hw:CARD=DAC`,
`hw:CARD=DAC,DEV=0` and the short forms are all matched); capture-only PCMs are
skipped.

Card numbers depend on boot order, so the daemon names devices by card id:
the device it picks on first run is `hw:CARD=<id>,DEV=<n>`, and a numeric
`hw:<card>,<dev>[,<sub>]` in the config is rewritten to the card-id form
(and saved) at start-up while that device exists.

**Device gone: the sinks stay.** When the configured device is not there
(unplugged while running, or missing at start-up), the engine keeps both
sinks in the graph and runs them on its own clock: streams keep their link
to Audivane and keep progressing, their audio is discarded, and PipeWire
never moves them to another output because of Audivane. `status.state` is
`error`, `status.waiting_for_device` is true and `status.error` says
`USB is not connected.` (the card name of a `hw:CARD=…` id, else the id;
the app shows it once, in its top banner).
What the music does is `device_lost`:

- `pause` (default): when the device goes while something plays through
  Audivane (the engine was `playing` up to 3 s before it started waiting),
  the daemon sends `Pause` to the MPRIS players that report `Playing` and
  play through Audivane: the player's process (`GetConnectionUnixProcessID`)
  is the process of a stream linked to Audivane's sinks, or one of its
  parents (a browser's audio service, an Electron helper); the stream's
  process is its PipeWire client's `pipewire.sec.pid`. A player whose
  processes do not tell (a Flatpak sandbox) still matches when its bus
  name's player part (`org.mpris.MediaPlayer2.<name>[.instance…]`) is the
  stream's `application.process.binary` or `application.name`. Players
  playing elsewhere keep playing. Without the graph watcher every playing
  player is paused. Nothing is resumed when the device comes back. A player
  without MPRIS keeps playing into silence. A start-up without the device
  pauses nothing.
- `keep_playing`: nothing is paused.

The engine looks for the device in `/proc/asound` twice a second (nothing
is opened) and, once it is there, opens it through the usual reopen: the
rates are probed again, the volume is written to the card again before
anything plays (the same failsafe as any open), and playback fades in. A
device that is there but fails to open after a loss is retried after 1 s,
then with a doubling pause up to 30 s; `status.error` then carries the open
error with `(retrying)`. `list_devices`, `get_config` and `set_config` work
while waiting, so another device can be picked; a device change opens the
new one at once.

How a loss is told from other failures: an output whose stream cannot be
restarted (`prepare` or `start` fails after an error) is handed back to the
engine's control thread for a reopen instead of ending the audio thread; a
reopen that finds the device gone (or that follows such a failure) waits.
ALSA reports nothing to an open device that is not playing, so while it
holds the device the control thread also looks for it in `/proc/asound`
twice a second; once a device it saw there is gone, the idle output is
handed back the same way (an unplug between tracks waits too, and frees
the stale handle and reservation).

`release_device` (or a ReserveDevice1 request) ends the wait: status no
longer says it waits, and `acquire_device` opens the device from scratch.
A successful acquire ends it as well.
`AUDIVANE_TEST_UNPLUG=<file>` makes the device count as unplugged while the
file holds `1`, for rig tests.

**Device present but failing at start-up.** When the device is there but
the engine cannot start on it, the daemon still starts and answers on its
socket with a stand-in engine: `status.state` is `error` and `status.error`
says `Cannot open output device ...: <ALSA error>`. `list_devices`,
`get_config` and `set_config` work; DSP calls that need the engine
(`selftest`, `ab_select`, `acquire_device`) fail with `BAD_STATE` and the
same text. The daemon replaces itself with a fresh copy (same arguments,
same PID) once `set_config` (or `set_device`) names another device that
exists; the failing device is not retried in a loop. The stand-in has no
sinks.

**Audio thread ended.** When the engine's audio thread stops for a reason
a reopen cannot handle, the daemon replaces itself with a fresh copy within
a second (after 5 s when the device is still present). Clients see the
socket close and reconnect.

### Chain wire shapes (EQ & convolution)

- **Per-channel EQ (`EqProfile`):** `EqProfile` has an optional
  `right: Option<ChannelEq>` (`{ "preamp_db": f64, "bands": [...] }`), omitted
  when `None`. `None` links all channels to the profile's top-level
  `preamp_db`/`bands`; `Some(right)` applies the top-level set to channel 0
  (and channels ≥ 2) and `right` to channel 1. Analytic A/B level matching
  uses the mean power response of left and right.
- **DC blocker (`ChainConfig.dc_block`, `set_dc_block`):**
  optional stage placed first at the source rate, before pitch and crossfeed
  (`null` or `enabled: false` = off; absent on `ChainConfig` by default).
  `DcBlockConfig { enabled: bool, cutoff_hz: f64 }` has field defaults
  `enabled: true` and `cutoff_hz: 5.0` (so `{}` enables it at 5 Hz);
  `cutoff_hz` must be finite and in `1.0..=20.0`. `set_dc_block` updates chain
  A, accepts `null` to clear it, and returns `null`. Runs a 2nd-order
  Butterworth high-pass (`Q = 1/sqrt(2)`, prewarped bilinear) per channel in
  Direct Form I `f64`, zero latency (at 5 Hz: −0.017 dB at 20 Hz, −0.0097 dB
  at 23 Hz). Worst-case bounded-input sample-peak gain (L1 norm) is `‖h‖₁ ≈
  2.4345` (+7.73 dB; at 48 kHz / 5 Hz: full-scale step from silence −0.004 dB,
  bass-rich multitone +0.066 dB, synthetic kick −0.169 dB, settled bipolar step
  +6.0 dB, 5 Hz square wave +7.3 dB); no automatic gain compensation is applied.
  Breaks passthrough when `enabled: true`; Direct and DSP-off paths skip it.
  CLI: `avdsp ctl dc-block [on|off] [--cutoff <HZ>]` (`off` keeps `cutoff_hz`
  with `enabled: false` so a later `on` restores it; no arguments inspects).
- **Headphone crossfeed (`ChainConfig.crossfeed`, `set_crossfeed`):**
  optional stereo-only stage before EQ (`null` or `enabled: false` = off).
  `mode` is one of `bs2b_default` (700 Hz, 4.5 dB), `bs2b_chu_moy` (700 Hz,
  6.0 dB), `bs2b_jan_meier` (650 Hz, 9.5 dB), `bs2b_custom` (`cutoff_hz`
  300..=2000 Hz, `feed_db` 1..=15 dB), or `linkwitz` (796.4 Hz, 6.0 dB,
  mono-flat, 0.3 ms low-frequency interaural delay). `cutoff_hz`/`feed_db`
  are only used by `bs2b_custom`. Requires 2 channels when enabled.
  `level` (`CrossfeedLevel`, default `matched`) sets the stage's constant
  gain, computed when the chain is built:
  - `matched`: the same A-weighted loudness with crossfeed on and off, for
    stereo pink noise with inter-channel correlation 0.5 (the A/B analytic
    level match's reference, A-weighted). The gain is within ±0.5 dB of unity (−0.48 to
    +0.21 dB); worst-case signals can then go up to +1.4..+5.6 dB over the
    input's peak (+4.3 dB for `bs2b_default`). **Such a signal clips at the
    output** unless the limiter is on (`limiter.mode` `limit` or
    `auto_headroom`) or at least that much attenuation is applied digitally
    before the quantizer (digital volume, EQ preamp). Hardware volume gives
    no headroom: it acts after the DAC input. Nothing else catches it; use
    `peak_safe` to rule clipping out.
  - `peak_safe`: the exact worst-case peak gain of the filters (−2.9 to
    −4.4 dB for the presets, −1.4 to −5.6 dB across `bs2b_custom`), so the
    stage cannot go over; music plays that much quieter.

  The EQ preamp is not adjusted for either. Analytic A/B level matching
  includes the stage: its stereo power response (level included) for the
  same correlation-0.5 reference. CLI: `avdsp ctl crossfeed level
  matched|peak-safe`; the preset commands keep the current level.
  Example: `{"enabled": true, "mode": "bs2b_default", "level": "matched"}`.
- **Harmonic distortion (`ChainConfig.harmonics`, `set_harmonics`):**
  optional stage after the upsampler (`null` or `enabled: false` = off).
  `character` is `triode`, `pentode`, or `custom` (`h2_db`, `h3_db`, optional
  `h4_db`/`h5_db` in −120..=−20 dB relative to fundamental at 0 dBFS;
  `dc_block` default `true`). `h4_db`/`h5_db` `null` or omitted = that term
  off; `h2_db`/`h3_db` are always applied (−120 dB is 1e-6, not off). Levels
  are exact for a full-scale sine. The stage scales its output so input
  within full scale stays within full scale, and changes level (shown by
  `avdsp ctl harmonics show`).
- **Convolution chain (`ChainConfig.convolution`, `set_convolution`):**
  `ChainConfig.convolution` is `Vec<ConvolutionConfig>`, where each item is
  `{ "name"?: String | null, "enabled": bool, "ir_path": String, "gain_db": f64 }`.
  Both `ChainConfig.convolution` and `Call::SetConvolution` accept:
  - `null` or missing → `[]` (convolution off)
  - a single `ConvolutionConfig` object → `[item]` (backward compatible)
  - an array of `ConvolutionConfig` objects → `[...]`
  On the worker thread, the engine resamples each enabled IR to the graph rate,
  scales by `gain_db`, and convolves them via FFT into one combined IR per
  channel (mono applies to all channels; stereo maps L→L, R→R; combined tap
  count capped at 4 000 000).
- **Pitch shift (`ChainConfig.pitch`, `set_pitch`):**
  `ChainConfig.pitch` is `PitchConfig { enabled: bool, cents: f64, semitones?: f64, reference_hz?: f64, target_hz?: f64, quality: "low" | "high" | "max" }`
  (defaults: `enabled: false, cents: 0.0, quality: "high"`). At most one of
  `cents` (non-zero, `−1200..=1200`), `semitones` (`−12..=12`), or the
  `reference_hz`/`target_hz` pair (both `> 0`, derived shift in `−1200..=1200`)
  may be set. Quality steps down at high rates to stay inside the callback
  deadline: `max` runs as `high` above 96 kHz, `high` as `low` above 192 kHz.
  When `enabled == false` or the effective shift is `0.0`, the stage
  is bypassed bit-for-bit. `set_pitch` (`Call::SetPitch(PitchConfig)`) updates,
  validates, persists, and applies chain A's pitch config. CLI: `avdsp ctl pitch
  [--cents C | --semitones S | --reference-hz R --target-hz T | --off]
  [--quality low|high|max]` (`--json` prints `PitchConfig` JSON).


## Volume, balance, and mute

- Persisted in `EngineConfig.volume` (`VolumeConfig`, every field
  `#[serde(default)]`):

  ```jsonc
  "volume": {
    "volume_db": -20.0, "balance_db": 0.0, "muted": false,
    "mode": "hybrid",          // auto (default) | software | hardware | hybrid
    "hardware_db": -10.0,      // fixed hardware level (software/hybrid), or null
    "max_volume_db": -6.0,     // hard ceiling, or null
    "sink_volume": "follow",   // ignore (default) | follow
    "guard": { "mode": "limit", "max_rise_db_per_s": 6.0, "stream_max_db": -12.0 }
  }
  ```

  Validation: `volume_db` `-120..=0`, `balance_db` `±24`, `hardware_db`
  `-120..=0` (in `software` the fixed level is not part of the volume
  shown, so a positive one would play louder than it), `max_volume_db` and `guard.stream_max_db` `-120..=0`,
  `guard.max_rise_db_per_s` `0..=1000` (default 6; guard `mode` defaults to
  `off`); `hybrid` requires `hardware_db`. A `volume_db` above
  `max_volume_db` (both in range) is stored as `max_volume_db`, from
  any edit and the config file.
- **The level belongs to `set_volume`.** `set_config` ignores the incoming
  `volume.volume_db`, `volume.balance_db` and `volume.muted` and keeps the
  live ones: a whole config is usually a client's copy edited for something
  else, and its level may have changed since (or be another daemon's). The
  other volume settings (`mode`, `hardware_db`, `max_volume_db`,
  `sink_volume`, `guard`) apply as usual, and a lower ceiling still lowers
  the effective level. The ignored fields are not validated either (the
  live ones replace them first, then the config is normalized and
  validated). `avdsp ctl config set <file>` therefore leaves the level
  alone; use `avdsp ctl volume`.
- `set_volume` (`Call::SetVolume { volume_db, balance_db, muted }`)
  changes only those three fields (the others are set with `set_config`),
  unconditionally (an absolute set, as the CLI does), validates, persists,
  and returns the `VolumeStatus` (with the new `revision`).
- `set_volume_guarded` (`Call::SetVolumeGuarded { volume_db?, balance_db?,
  muted?, if_instance, if_revision }`, both conditions required) is the
  same change as a **compare-and-set** on the level state the client based
  it on. Only the level fields present change; an absent one keeps its
  value at the guarded revision, so a client writes only what a gesture
  edits (a volume step never rewrites a mute, and the reverse). Without
  any level field the call only checks the guards and returns the current
  `VolumeStatus`, changing nothing (a desktop ramp goes on). The check
  is against: the `instance` and `revision` of the `VolumeStatus` it read, or of
  its own last accepted write. When `if_instance` is not this daemon
  process's `instance` the call fails with `STALE_INSTANCE` (3); when
  `if_revision` is not the current `revision`, with `STALE_REVISION` (4).
  Either way nothing changes, and the client reads the status again before
  it writes. A request delayed or replayed across a restart (it may reach
  the replacement on a fresh connection), or computed from a level that
  changed since (the desktop, the CLI, a guard step, a ceiling), so never
  applies an old level. The check and the write happen under the lock
  every level change takes. It is a separate method so that a daemon that
  predates it refuses it at dispatch (`INVALID_REQUEST`, unknown method)
  instead of applying it unconditionally; clients treat that like a stale
  refusal and do not fall back to `set_volume`. The app only writes the volume this way. In both methods a `volume_db` within range but
  above `max_volume_db` is capped (not an error) and logged as a blocked
  change. `Status.volume` (`#[serde(default)]`) reports the current
  `VolumeStatus`:

  | Field | Meaning |
  | --- | --- |
  | `volume_db` | Effective master volume (after the ceiling and hardware quantization) |
  | `balance_db`, `muted` | As configured |
  | `hardware` | The hardware mixer carries (part of) the master volume |
  | `min_db`, `max_db` | Range the volume can actually take (`max_db` includes the ceiling; `min_db` includes digital attenuation below a hardware mixer's floor, so it is the engine minimum whenever digital volume is available) |
  | `mode` | Configured mode |
  | `hardware_db` | Level the hardware mixer is at: set by the engine, or (`software` without a fixed level) where an earlier mode left it |
  | `software_db` | Digital master gain on the DSP, Direct and bypass paths (0 = unity) |
  | `hardware_unavailable` | `hardware`/`hybrid` asked for, card has no usable control: digital used |
  | `ceiling_db` | `max_volume_db` in force |
  | `sink_volume`, `guard` | Configured sink policy and guard mode |
  | `blocked` | Volume changes blocked or reduced since daemon start |
  | `last_blocked` | `BlockedVolumeChange { source, requested_db, applied_db, reason, at_ms }` |
  | `instance` | This daemon process: 128 bits from the OS's random source, hex, new on every start; omitted by daemons that predate it |
  | `revision` | Increases on every change of the level state (effective `volume_db` after the ceiling, `balance_db`, `muted`) from any source (`set_volume`, the desktop with `sink_volume: follow`, guard steps, a ceiling change), and on every change of what this status reports as applied without a request (a staged hardware raise landing, the mixer read back on start-up or a reopen, a mode or `hardware_db` change). The applied level is read under the same lock the `set_volume_guarded` check takes, so one revision goes with one reported level. Unchanged by a write of the same level. Known limitation: the check and the engine's apply are not one step, so an applied-level change landing between them (a few milliseconds) still lets a write derived from the interim level through. Absent from daemons that predate it; clients must not write the volume from a status without `instance` and `revision` (the app disables its controls then) |

  `source` is `{"kind":"sink","sink":"dsp"|"direct"}`,
  `{"kind":"stream","node":N,"app":"..."}` or `{"kind":"control"}`;
  `reason` is `locked`, `capped`, `slewed` or `direct_bit_perfect`.
- **Hardware mixer:** on the ALSA card of the configured `hw:` device, the
  builder thread looks for a simple mixer element with playback volume in dB
  (`Master`, `PCM`, `Headphone`, `Speaker`, then the sole playback-volume
  element), re-applied after every device reopen (rate switch, acquire) and
  device change. Mixer writes happen off the real-time thread.
- **Modes** (`engine/volume.rs`):
  - `auto`: the volume (and mute, if the element has a playback switch) in
    hardware when such an element exists, otherwise digital.
  - `software`: always digital. The mixer is left alone, or, with
    `hardware_db`, set to that level once per device open and after any
    other mode moved it (switch untouched, mute digital). Without
    `hardware_db` the ceiling bounds the digital part only.
  - `hardware`: like `auto`, but a card without a control reports
    `hardware_unavailable: true` (and a warning in the log); the volume then
    falls back to digital rather than playing at full level.
  - `auto`/`hardware` below the card's range: the card at its minimum, the
    rest digital. A residue within 0.02 dB is the mixer's rounding (it asks
    for 0.01 dB steps and reads back in 0.01 dB units, and rounds down to a
    card step) and stays unity, so the card may play up to 0.02 dB above
    the request rather than breaking Direct's bit-perfect path.
  - `hybrid`: the hardware stays at `hardware_db` (required) for every
    volume at or below it and the digital volume attenuates the rest
    (`software_db = volume - hardware`). Only a louder volume raises the
    hardware, with the digital part at unity. The digital part is never a
    boost.
  - **Order of writes:** when a change raises the hardware above the
    loudest digital gain that may still be buffered, the digital part
    applies at once and the hardware write waits one output latency (ALSA
    delay plus filter delay plus one period) plus 50 ms, so audio already
    rendered at a louder digital gain never plays at the new hardware
    level. The engine keeps that loudest gain (and until when it can still
    be buffered) across later changes: a volume nudge, mute, an EQ edit or
    a guard step inside the window restages the raise, never sooner than
    it was due. `Status.volume` shows the hardware's old level until then.
  - **Letting go of the DAC:** in `software` with `hardware_db` and in
    `hybrid`, and in `auto`/`hardware` when the volume is below the card's
    floor (the card at its minimum, the rest digital), the hardware is
    brought down to the effective volume before
    the device is handed over, once, on every path that lets it go: the
    driver's release hook runs after playback stopped and before the
    reservation is released, on `release_device`, a ReserveDevice1 release
    request, shutdown (the driver stopping), a failed reconnect, a device
    switch (for the old card, before its output closes and its reservation
    goes; the engine also hands it over as soon as the new device is
    configured, so nothing writes the new card before it is reserved and
    negotiated), a failed rate resolve or open (the card was still
    reserved), and a start-up that fails after the first device open (a
    PipeWire registration failure: the hook is installed before that open,
    which may already have written the mixer). When the card was already let go (an invalid device, a
    failed reservation) the hook only stops mixer writes. The hook names
    the card it lets go of, and the engine acts only on the card it holds.
    Likewise every device open reports the device it opened: an open that
    finishes after a config change moved the engine to another device (a
    rate reopen of card A, with the change to B queued behind it) is
    superseded. The engine writes no mixer for it (neither B, not yet
    reserved, nor A) and does not take the DAC; the driver closes it
    without resuming, and the queued change opens B (an acquire is redone
    with it). The hook waits
    for the engine's builder lock, which a chain build can hold for a
    while; `release_device` then gives up
    waiting after 5 s with an error that clears itself when the release
    completes. Below the card's range the card stops at its minimum and its
    playback switch, if it has one and it was on, is muted (a switch the
    user muted is left alone); the mute is recorded per card (by its ALSA
    name, not its index) in `$XDG_STATE_HOME/audivane-dsp/failsafe-muted`
    (default `~/.local/state`, kept across reboots, since ALSA restores a
    muted switch at boot; written atomically: temp file, fsync, rename;
    one line per card: card, device, level written, and every playback
    channel's level and switch as left) and
    undone on the engine's next hardware write to that card; the entry
    stays until a write actually turned the switch on, and other cards'
    entries stay. A mode that drives the switch (`auto`, `hardware`,
    `hybrid`) sets it from its mute anyway. `software` does not own the
    switch, so it undoes the mute only while the card is exactly as the
    hand-over left it (checked once per open: every playback channel's
    switch and level as recorded, so an unlinked control with one channel
    moved counts as touched); otherwise the card was touched since (another
    player turned it on, the user muted it again at another level), or its
    state is unknown (not recorded, unreadable), the entry is dropped, the
    switch is left as it is and the log says so. A card changed and put
    back to the very same state cannot be told from an untouched one.
    While the DAC is not held, or the driver reports an error
    (at the last apply, or now for a staged raise), the mixer is never
    written; the next device open writes the configured level again. A
    fatal error on the realtime thread reaches that state at the driver's
    next poll (up to 50 ms), so a staged raise falling due in that window
    can still be written; the card is still reserved then.
  - **Crash failsafe:** `audivane-dsp --volume-failsafe` does the same
    from the config file (read only: an invalid file is left alone and the
    failsafe does nothing; it reads the card's level, on its quietest
    playback channel since an unlinked control can hold them apart, and
    writes nothing when that is at or below the stored volume, since its
    write sets every channel and would raise that one, or when it cannot
    be read (logged); `auto`/`hardware` act only when it is above). The
    engine's own hand-over uses the same rule, from the card read then or
    else its last write's readback when no engine answers on the socket (the same
    socket the daemon would use: `--socket`, `$AUDIVANE_DSP_SOCKET`, the
    runtime directory); the systemd unit runs it as `ExecStopPost=` (after
    crashes and SIGKILL too). It does nothing while
    `$XDG_RUNTIME_DIR/audivane-dsp/dac-released` exists: the engine writes
    that file when it starts (it holds nothing until its first device
    negotiation) and whenever it lets go of the DAC (the card was lowered
    then, and another program may hold it), and removes it when it takes
    the DAC. With `$XDG_RUNTIME_DIR` unset the failsafe also does nothing
    and logs why: the marker must not outlive a reboot, so there is no
    persistent fallback, and without it the failsafe cannot tell. Whether
    another ReserveDevice1 owner holds the card is not checked.
- **Ceiling:** `max_volume_db` bounds the master volume in every mode,
  applied in the engine, so no path (IPC, desktop, a config file) exceeds
  it by more than the 0.02 dB mixer-rounding tolerance below.
- **Digital volume & balance:** applied in `f64` immediately before final
  quantization/dither, on the DSP path and also on the Direct and bypass
  paths (otherwise those would play louder than the volume in `software`
  and `hybrid`). Channel balance (`balance_db < 0` attenuates the right
  channel by `|b|`; `> 0` the left) is always digital. Mute uses the
  hardware switch when the mode uses it, otherwise a ramped digital mute.
  All digital gain/mute transitions ramp over ~10 ms with lock-free RT
  updates and no RT allocations. On Direct and bypass a non-unity digital
  part is quantized to the DAC's bit depth with TPDF dither when that is
  below 32 bits (the DSP path uses its own quantizer). Digital silence
  stays exact zeros, so a DAC's auto-mute still sees it: a channel at a
  settled gain of 0 (a digital mute) is never dithered, and each channel
  is dithered only from a non-zero input sample on until 100 ms after its
  last one (per channel, so a silent channel beside music stays silent,
  and sparse signals do not switch the noise on and off per block).
- The **Direct** (`audivane_direct`) path is bit-exact while the digital
  part is unity and unmuted (0 dB, or a mode where the card carries the
  volume). A non-unity digital part marks the engine `dsp_active = true`
  (`PassthroughBreak::DspActive`) on either sink; on the DSP sink, so does
  an active chain, and on either sink the engine bypass resampler while it
  converts (`bypass_resampler.active`), at unity volume too. Pure hardware
  volume with centred balance does not.

### Sink volume (`sink_volume`)

Our sinks are `pw_stream`s, so PipeWire's adapter (`audioconvert`) applies
the node's volume in `f32` before the engine receives a sample.

- `ignore` (default): that is left as it is. The desktop slider scales the
  samples in PipeWire and the engine does not know about it
  (`passthrough` reports it as a `volume` break).
- `follow`: the engine owns the sink volume.
  - Every external `Props` write on `audivane_dsp` (desktop slider, media
    keys, `wpctl`, pavucontrol) reaches the engine's PipeWire thread through
    the stream's `param_changed`. It answers at once with its own complete
    `Props` write: the requested `channelVolumes`/`mute` still shown,
    `softVolumes` 1, `softMute` false, `volume` 1 and
    `params: ["channelmix.max-volume", 1.0]`, so PipeWire passes the samples
    at unity, and never above unity in the cycle before that answer (the
    limit clamps `volume`, `channelVolumes` and `softVolumes` each). A
    write of only `softVolumes`/`softMute` is answered the same way. The
    request (loudest channel = volume, the channel difference = balance,
    `mute`) goes to the control plane, through the ceiling and the guard,
    into `volume` (persisted as from `set_volume`).
  - The desktop's 100 % is the ceiling (`max_volume_db`, 0 dB without
    one), both ways: the sink's `channelVolumes` carry the level relative
    to the ceiling, and a request's level is the written `channelVolumes`
    in dB plus the ceiling. Desktops show the cube root of the linear
    value, so below the ceiling the percent falls the same cubic way as
    below 0 dB (ceiling -3 dB: 100 % = -3 dB, 50 % = -21.06 dB). A desktop
    100 % lands exactly at the ceiling; the sink never shows above 100 %
    (`channelVolumes` at most 1). Silence is the engine's floor.
    `audivane_direct` stays at 100 % (0 dB) whatever the ceiling.
  - Changing the ceiling keeps the level in dB (lowering it below the
    level lowers the stored level to it, as before) and
    moves the desktop slider to the matching percent.
  - The engine shows its effective volume on the sink whenever it changes
    (`set_volume`, the ceiling, a guard step), so the desktop shows the truth.
    A request it does not take as asked (capped at the ceiling, locked,
    rising gradually) is answered with the level in force, also when that
    level does not change: a volume key up at the ceiling puts the sink
    back at the ceiling, so it never shows more than plays.
    The stream's own writes are not reported back to it, so there is no
    feedback loop.
  - `audivane_direct`: its volume is put back to 100 % at once (a blocked
    change, `direct_bit_perfect`); its mute is the engine mute.
  - Both sinks carry `state.restore-props = false`, so WirePlumber neither
    restores nor stores their volume; on connect the sinks show the
    engine's persisted volume.
  - Switching to `follow` lowers `volume_db` to the level of the sink being
    listened through (the one feeding the DAC, else the DSP sink), and
    mutes when that sink is muted, since PipeWire stops attenuating at that
    moment.
  - Switching back to `ignore` sets both sinks to plain 100 % once, with
    `channelmix.max-volume` back at the sink's own value from before (or
    PipeWire's default 10.0 when none was seen).
  - `set_stream_props` failures are logged at warn.

### Volume guard (`guard`)

Against an app (or the desktop) suddenly raising its stream volume or the
sink volume while the hardware is driven hot and Audivane is the preamp.

- `off`: nothing.
- `lock`: sink volume requests (`follow`) are refused and the sink shows the
  engine's volume again; mute passes. Each app stream on our sinks is held
  at the level it settled at (1 s after its node appeared, or when the
  guard was turned on); a later rise is written back to that level, a fall
  passes and becomes the new held level. Lock does not hold a stream up
  against the app turning it down: that would take channel volumes above
  1.0, and the app restoring its own master would then jump the stream
  over the held level. An app master `volume` of 0 (the app's own
  silence) is left alone in every mode. During the settle second a stream
  is still capped at `stream_max_db`.
- `limit`: falls pass at once; rises are capped (`max_volume_db` for the
  sink, `stream_max_db`, default 0 dB = 100 %, for streams) and raised at
  most `max_rise_db_per_s` (50 ms steps, at least 1 dB in `auto`/`hardware`
  with a hardware mixer; 0 = only the cap). A stream's starting level is
  not slowed down, only capped. A repeated request while rising moves the
  target without restarting the rise. `set_volume` is never slowed down
  and cancels a sink rise in progress.
- A stream is guarded from when it links to one of our sinks (or is moved
  onto one), using the time its node appeared; a stream moved away is
  forgotten. Guard writes to one stream are at most every 50 ms (the
  latest wins; a write held back goes out on the next 50 ms tick, so a
  revert can land up to about 100 ms after the app's write), and
  blocked-change log lines at most once a second per source. `off` forgets
  every stream. Turning the guard on, or changing its mode, holds every
  stream at its level as it is then, except a stream still in its settle
  second, which keeps it.
- Each blocked change is logged and recorded in `Status.volume.blocked` /
  `last_blocked`.
- App streams are guarded by the graph watcher. A stream plays its
  `channelVolumes` times its master `volume`; the guard judges that
  product and writes `channelVolumes` scaled so the loudest channel plays
  at the decided level, keeping the balance and the app's own master (its
  slider does not jump). The guard never writes the master and never
  writes a channel volume above 1.0: when holding the level would need
  more, the stream plays at the lower level the app asked for. A stream
  at master 0 is not judged or written, and keeps the level the guard
  last accepted, so turning it back up is no jump; one first seen at
  master 0 starts from silence, so its first rise is slowed as well.

- `set_stream_volume` sets one stream's level by hand; see below.

- CLI: `avdsp ctl volume [DB] [--balance DB] [--mute|--unmute]
  [--mode M] [--hardware-db DB|none] [--ceiling DB|none]
  [--sink-volume follow|ignore] [--guard off|lock|limit] [--rise DB_PER_S]
  [--stream-max DB|none]` queries `status`/`volume` when given no
  arguments; the settings flags go through `get_config` + `set_config`, the
  level flags through `set_volume`.

### Stream volume (`set_stream_volume`)

`Call::SetStreamVolume { node, volume_db?, muted? }` sets one app stream's
level and/or mute, by PipeWire node id (as `route` takes it). Both fields are
optional: only what is present changes, and with neither the call only
reports the stream.

```jsonc
→ {"id":7,"method":"set_stream_volume","params":{"node":83,"volume_db":-6.0}}
← {"id":7,"ok":{"node":83,"volume_db":-6.0,"muted":false,"max_db":0.0}}
→ {"id":8,"method":"set_stream_volume","params":{"node":83,"volume_db":0.0,"muted":false}}
← {"id":8,"ok":{"node":83,"volume_db":-12.0,"muted":false,"max_db":-12.0,"limited_by":"stream_max"}}
```

- The level is written the way the guard writes it: the stream's
  `channelVolumes` scaled so its loudest channel times the app's own master
  `volume` plays at the level, keeping the balance. The app's master is
  never written (its slider does not jump), and no channel goes above 1.0
  (100 %). So the app's own volume bounds the level: an app at 50 % reaches
  -6 dB at most. `muted` writes the stream's `mute`.
- The reply is a `StreamVolume`: `volume_db` (the level it plays at now,
  0.01 dB steps), `muted`, `max_db` (the loudest this call can set now:
  the lower of the stream cap and the app's own volume) and `limited_by`
  (absent when the request was met): `stream_max` (the cap) or
  `app_volume` (the app's own volume). `list_streams` and the `streams`
  event show the new level at once, without waiting for PipeWire's echo.
- **Guard interplay:**
  - The level is capped by `guard.stream_max_db` (0 dB without one) in
    every guard mode, `off` included. A capped request is not an error:
    it is applied at the cap, logged and recorded in `Status.volume`
    (`blocked`/`last_blocked`, reason `capped`, source `stream`).
  - `limit`: the call is not slowed down. It is the user's own action,
    like `set_volume` for the sink: the level applies at once and a guarded
    rise in progress on that stream is dropped.
  - `lock`: the new level, up or down, becomes the held level, so the
    guard does not revert it; a later rise by the app is reverted to it.
  - In both modes the stream counts as settled from then on (a stream in
    its first second is judged against the user's level, not its app's
    starting level), and a guard write held back by the 50 ms interval is
    dropped.
  - `off`: only the cap; the guard keeps no state.
  - **Late echoes.** PipeWire answers each `Props` write with the stream's
    new `Props`, and an answer can arrive after a newer write. The guard
    keeps the levels it and this call wrote on each stream (the last 8,
    for 1 s). An observed level equal to one of those that is older than the
    newest write is taken as a late echo and ignored. Without that, the echo
    of an earlier guard write would undo the user's level: under `lock` it
    would become a lower held level, and under `limit` it would restart a
    rise the call had cancelled. The echo of the newest write clears the
    older ones. An app that sets exactly a level written within the last
    second, while a newer write's echo is still on its way, is not seen
    until its next change.
- `list_streams` and the `streams` event follow the level that plays, also
  when only the app's master `volume` changes (the loudest channel, and so
  `volume`, can stay the same), and right after a guard write.
- **Removal.** A node PipeWire is removing (its proxy was told it is
  removed, or the node reported an error state) is refused with
  `BAD_STATE` before anything is written. The registry can still deliver a
  removal after the write was sent: the call then reports success for a
  stream that is already going. Its level is then gone with it; the guard
  forgets the node on the removal.
- **Timeout.** The call waits 2 s for the watcher thread. A request still
  queued then is dropped, never applied later, and the call fails with
  `IO` ("nothing was changed"). A request the watcher had already started
  is waited for, so its answer is the one reported.
- **Logging.** Each set is logged at debug; the level a stream settles at
  (no newer set for 0.5 s) is logged once at info. Capped requests are
  logged at info like other blocked changes (at most once a second per
  stream), refused ones at info each.
- **Errors:**
  - `INVALID_PARAMS`: `volume_db` outside `-120..=0` or not finite; no
    node with that id; the node is not an app playback stream
    (`Stream/Output/Audio`).
  - `BAD_STATE`: no PipeWire graph (or its watcher stopped); the stream is
    going away; the stream is not linked to one of our sinks (only streams on Audivane DSP or
    Audivane Direct are set; route it first); the stream has not reported
    its volume yet; a level was asked while the app's own master volume is
    0 (raise it in the app first; muting still works).
  - `IO`: the watcher did not take the request within 2 s (nothing was
    changed), or the node could not be written.
- CLI: `avdsp ctl stream volume NODE [DB] [--mute|--unmute]` (negative
  levels need no `--`); with only the node it prints the current level.
  `avdsp ctl streams` shows `volume_db` in its VOL column.

## PipeWire watcher and routing

`control::graph` runs a private PipeWire main loop on its own thread. It tracks:

- `Stream/Output/Audio` nodes: app name, binary, media name, negotiated
  `Format` (rate/channels/sample format), `Props` (volume/mute).
- Links by their registry properties (`link.output.node`/`link.input.node`),
  which is enough to know which sink a stream is feeding.
- Our two driver sinks by `node.name` (`audivane_dsp`, `audivane_direct`),
  including their `object.serial`.
- The `default` metadata: `default.audio.sink` (effective) and
  `default.configured.audio.sink` (the user's preference).

Facts are published as one comparable `Facts` value; the IPC event pump uses
it to decide when `status`/`streams` changed.

Routing modes:

- **System** (`RoutingMode::System`): write `default.audio.sink` *and*
  `default.configured.audio.sink` as `Spa:String:JSON`
  `{"name":"audivane_dsp"}`. WirePlumber treats the configured key as the
  user's preference and publishes the effective default. Before the first
  write, **both** keys are snapshotted independently (raw value, or "absent")
  and persisted to `$XDG_RUNTIME_DIR/audivane-dsp/default-sink-restore.json`
  (temp file + fsync + rename). On leaving system mode or on shutdown each
  key is restored only while it still points at one of our sinks — a
  configured preference that differed from the effective default (e.g. the
  preferred device is unplugged) is not overwritten with the effective value,
  and a user's later choice is left alone. If a previous run died before
  restoring, the next start repairs the keys that still point at our sinks
  and deletes the file; if the metadata is unavailable at that moment the file
  is kept for the next start. Releasing the device leaves system mode first,
  and while the engine has no device (idle/released) system routing is not
  asserted at all, so the session default never points at a sink that does not
  exist; acquiring re-applies it.
  A supervisor thread re-asserts the default if WirePlumber picks something
  else (e.g. a hot-plugged device), at most every 500 ms, but only while the
  `audivane_dsp` sink is in the graph. When the sink disappears without a
  release (the DAC is unplugged and the driver goes idle on its own), the
  supervisor hands the default back the same way as on leaving system mode,
  so the session falls back to another output instead of `auto_null`; it
  takes the default again when the sink returns.
- **Apps** (`RoutingMode::Apps { rules }`): move individual streams by writing
  the `default` metadata for the stream's subject:

  | key | type | value |
  | --- | --- | --- |
  | `target.object` | `Spa:Id` | the sink's **`object.serial`** |
  | `target.node` | `Spa:Id` | the sink's registry node id |

  `SinkChoice::Default` deletes both keys, which lets PipeWire fall back to
  the stream's own target/default. Rules match `application.name` or
  `application.process.binary` case-insensitively, exactly; they are applied
  to streams as they appear and to already-playing matching streams when the
  rule set changes. Replacing the rules first clears the keys the old rules
  wrote, so a stream whose rule disappeared is not pinned forever. A rule for
  a sink that is not registered yet does not touch the stream.

  WirePlumber 0.5/1.6 only relinks when `target.object` is the sink's
  `object.serial` with type `Spa:Id`; a JSON `{"name": ...}` value is ignored
  and the stream is sent to the default sink. The serial is read from the
  sink's node properties, and `target.node` is written alongside for
  implementations that use the pipewire-pulse form.

The watcher never touches nodes other than the streams it routes and its own
two sinks.

## Passthrough status

`control::passthrough::compute(snapshot, streams feeding the active sink, sink,
source_bits)` is pure. Breaks, in report order:

1. `NotPlaying` (no stream is feeding our sink)
2. `NoOutput` (streams are linked but the output is not writing them: no
   format negotiated, a reopen in flight, an error; the engine `state` is
   not `playing`. Nothing reaches the DAC, so never `intact`. After a
   reopen the engine reports `playing` only once the live graph rate was
   checked and the device was prefilled and started)
3. `DspActive`
4. `Upsampling { ratio }` (integer ratio > 1) or
   `RateConversion { graph_rate, output_rate }` (fractional or decimating;
   the DSP sink with DSP on, or Direct and DSP off through the engine bypass
   resampler at a rate the DAC does not play natively)
5. `Varispeed { speed }` (the variable-speed stage is active on the DSP path,
   including at `speed: 1.0`; carries the audio thread's applied speed, never
   the config target. Always accompanied by `DspActive`; Direct and DSP off
   never run the stage and stay bit-perfect)
6. `Volume { node, volume }` (sink first, then streams by node id; volume ≠ 1.0
   or muted; `volume` is linear, 0 when muted)
7. `Mixing { streams }`
8. `Resampled { stream_rate, graph_rate }`
9. `ChannelMix { stream_channels, sink_channels }`
10. `SourceOver24Bit` — only when the source precision is actually *known* to
    exceed 24 bits, i.e. the player reported it (MPRIS `native_bits`) for the
    single stream feeding the sink
11. `Truncated { source_bits, output_bits }` (DAC format narrower than the
    widest *known* source)

A negotiated PipeWire format is a container width, not a source precision:
`pw-play` opens a 24-bit WAV as `S32LE`, and an `F32LE`/`F64LE` bus says
nothing about the file behind it. Such streams get a **caveat** instead of a
break: `Passthrough::caveats` carries `UnknownSourcePrecision { format }`
(`"S32LE"`), `intact` stays true, and `avdsp ctl status` renders e.g.
`passthrough: bit-perfect (source precision unknown: S32LE container)`. `caveats`
is `#[serde(default)]`, so older clients keep working; they are not breaks and
never appear in `broken_because`.

`intact` is simply "no breaks". The output word length is the negotiated ALSA
format in the engine snapshot (e.g. `S32`); the dither target in
`chain.output.bits` is not part of the snapshot, so a 24-bit source dithered to
16 bits inside a 32-bit container is not reported as truncated.

## Output format and dither

`OutputConfig` is `{ bits, dither, headroom_db }` (`chain.output`, set with
`set_output`; chain B always follows chain A's).

### Output headroom

`output.headroom_db` (`f64`, `0..=12`, default 3, a −3 dB cut) is one
fixed cut at the end of the DSP chain, at every ratio (also with the
upsampler off):

```
chain (… → upsampler → harmonics) → output headroom → (A/B mix and its
shared headroom) → digital volume → limiter → quantizer, clip/over reporting
```

It leaves room for the peaks the upsampler (or, at low ratios, the DAC's
own filter) reconstructs between samples; loud masters reach about +3 dB.
Harmonics come before it, so their levels stay defined at 0 dBFS. The EQ
`preamp_db` is unchanged and stays the profile's. Without harmonics the
gain is folded into the upsampler's kernel (exact, free per sample);
otherwise, and at x1, it is one multiply per output sample. At 0 dB nothing
changes and the output is bit-identical. A non-zero value counts as
processing: an otherwise empty chain does not take the bit-exact path.
With the defaults (3 dB, limiter `auto_headroom`) DSP on always processes;
`ChainConfig::neutral()` (limiter off, 0 dB) is the chain that changes
nothing. `avdsp process` without `--config` starts from that neutral chain.
Bypass, Direct and the engine bypass resampler never apply it. A value
outside `0..=12` (or not finite) is refused as
`chain.output.headroom_db must be between 0 and 12 dB`. `ClipStats.chain_peak`
(the chain peak before volume) is measured after the cut.
`avdsp ctl output --headroom DB` sets it; `avdsp ctl output` prints the
section.

The quantizer dithers to `Status.dither_bits` = `min(chain.output.bits,
container bits of the negotiated format)` (S16 → 16, S24/S24_3 → 24, S32 →
32), for chain A and B, rebuilt whenever a reopen negotiates another format
(`dither_bits` reports chain A's; an output compare's B caps its own bits the
same way).
Format choice is not strict (the first of `formats` the device takes at the
rate), so "Dither to 24" on a device that only takes S16 dithers to 16 bits
instead of letting the S16 packer truncate the low 8 bits.

`chain.output.dither` accepts `none`, `tpdf`, `shaped`, `headroom_tpdf`,
`rectangular`, `gaussian`, `highpass_tpdf`, `feedback1`, `feedback5`, `resonant5`,
`resonant9`, and `integrator15`. The default remains `tpdf`.

Offline processing accepts the same spellings through `avdsp process
INPUT.wav OUTPUT.wav --dither <name>`, including `--dither highpass_tpdf`
and `--dither headroom_tpdf`.

The nine fixed modes enable the DSP quantizer even with EQ,
convolution and upsampling off. `none`, `tpdf` and `shaped` keep the rule
that an otherwise empty chain bypasses quantization.
The master DSP switch and Direct sink bypass the configured dither, with
one exception: when the engine bypass resampler converts their rate
(`rate.bypass_resampler: engine`, the DAC lacking the source rate), its
resample-only chain quantizes to the output section's bits with its
dither, as the DSP path does. Applied dither counts as active DSP. Configure through `avdsp ctl config set` (the complete
config), for example with this output section:

```json
{"chain":{"output":{"bits":24,"dither":"resonant5"}}}
```

All modes round to nearest, ties to even. `none` and `tpdf` use
scale `2^(bits-1)` and saturate the output. `tpdf` uses the shared xoshiro256++
generator, its gain and its clipping behaviour. The fixed modes (including
`headroom_tpdf`, with per-channel KISS noise) first clamp input to [-1, +1],
then scale by `2^(bits-1) - 1 - H`. Headroom H in output
LSBs is 1 for `headroom_tpdf`/`rectangular`/`gaussian`/`highpass_tpdf`, 2 for
`feedback5`, 4 for `feedback1`/`resonant5`/`resonant9`, and 16 for
`integrator15`. As a deliberate overload-safety measure, packing saturates
the final output to the signed word range: headroom alone cannot prevent every
overflow. The feedback recurrence keeps the raw, unclamped q; this final
packing clamp does not limit the feedback error or alter the linear NTF or
non-overloaded output. Input clamps and output saturations increment the clip
counters. NaN emits zero and clears that channel's filter state. Dither
generators have separate channel seeds in the fixed modes; resetting restarts
the seeded sequence and clears all state.
`shaped` keeps the rate-adaptive nine-tap error filter and output
saturation (plain `tpdf` below 44.1 kHz).

The fixed loops have coefficients independent of rate and word length;
their spectra move in proportion to the output rate. `highpass_tpdf` filters
only the noise, rather than the total quantization error.

## MPRIS

`control::mpris` polls the session bus every 500 ms with `zbus::block_on`
(no tokio). It lists `org.mpris.MediaPlayer2.*`, reads `PlaybackStatus` and the
`Metadata` property, and keeps the most recently *started* `Playing` player as
the current one. A paused current player stays current (the UI keeps the last
track); if it leaves the bus the next most recent player takes over, and with
no history any readable player is shown.

When the output device goes while playing and `device_lost` is `pause`,
`mpris::pause_playing` calls `Pause` on the players whose
`PlaybackStatus` is `Playing` and whose streams go through Audivane
(`mpris::plays_through`; its own bus connection; see "Devices").

Native format keys are not standard; we accept these (case, `-` and `_`
insensitive, integer or numeric string values):

| field | keys |
| --- | --- |
| rate | `xesam:audioSampleRate`, `audio-samplerate`, `audioSampleRate`, `samplerate`, `samplerateHz` |
| bits | `xesam:audioBitDepth`, `audio-bitdepth`, `audioBitDepth`, `bitsPerSample`, `audioBits` |

## systemd user unit

`packaging/audivane-dsp.service`: `Type=simple`, `Restart=on-failure`. The
daemon never forks, so `--foreground` is accepted for clarity but implied.

- Real-time priority comes from **rtkit** (`MakeThreadRealtime`), which raises
  `RLIMIT_RTTIME` itself; `LimitRTPRIO` is therefore not needed and is only
  shown commented out for rtkit-less sessions.
- `LimitMEMLOCK=64M` covers ALSA/PipeWire buffers.
- Hardening keeps ALSA (`/dev/snd`), the PipeWire socket and D-Bus working:
  `ProtectSystem=strict`, `ProtectHome=read-only` plus
  `ConfigurationDirectory=audivane-dsp` (systemd creates
  `$XDG_CONFIG_HOME/audivane-dsp` before the sandbox and mounts it writable —
  `ReadWritePaths=-…` alone skips a missing directory, so a fresh install
  could not save; the config and its atomic temp file live there) and `ReadWritePaths=%t` (`$XDG_RUNTIME_DIR`: the control socket and
  `audivane-dsp/default-sink-restore.json`); impulse responses and EQ profiles
  are read-only, `PrivateTmp`,
  `ProtectKernelTunables/Modules`, `ProtectControlGroups`, `ProtectClock`,
  `ProtectHostname`, `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`,
  `RestrictNamespaces`, `LockPersonality`, `NoNewPrivileges`.

## EQ layout

`EqProfile` is layered (`version: 2`): a global `preamp_db`, a linked
section (`bands`, switchable with `linked_enabled`) that applies to every
channel, and optional `left` / `right` sections (`ChannelEq`: `enabled`,
`preamp_db`, `bands`) that apply only to channel 0 / 1.

- Channel 0 = preamp + linked bands (if enabled) + `left` (if present and
  enabled), in series; channel 1 likewise with `right`; channels >= 2 get the
  preamp and linked section only.
- Profiles without `version` (or below 2) are read as the older layout and
  converted on load: `right: None` keeps its bands as the linked section;
  `right: Some(r)` (fully independent channels) becomes an empty linked
  section with `left` = the old bands and `right = r`. Writes always use
  version 2.
- Auto preamp and analytic level matching use each channel's combined
  response. Auto preamp searches it from 10 Hz (or 3 octaves below the lowest
  band or tilt section, if lower) to Nyquist, densely around every band
  centre and tilt corner. Validation covers every section; only enabled sections count as
  active.
- APO / AutoEQ import fills the linked section (`Channel:` directives warn).
- `avdsp ctl eq`: `--section linked|left|right` on band edits, and
  `avdsp ctl eq section <linked|left|right> on|off|clear`.
- Optional `tilt` (`TiltConfig`, linked to all channels):
  `enabled` (default `true`), `start_hz` (default `20.0`, at least 20 Hz),
  `end_hz` (default `20000.0`, at most 20 kHz, clamped to `0.45 * fs` at
  chain build time), `pivot` (`"start"`,
  `"end"`, `"center"` geometric mean, or `{"hz": <f64>}`), and **exactly one**
  of `slope_db_per_oct` (`|slope| <= 6` dB/oct) or `total_db`
  (`|total| <= 24` dB). Included in auto preamp and A/B level matching.
  The implied slope and total must both satisfy those limits. Total mode
  derives its slope from the requested band; clamping preserves that slope.
  Enabled bands must still span at least 1/6 octave after stream-rate
  clamping; otherwise chain build returns a clear error, including for zero
  slope. An explicit Hz pivot must lie inside the requested band. Up to the
  effective (clamped) end it stays at its requested frequency; above it the
  target is flat, so it acts exactly as the `end` pivot.
  The target is the ideal sharp-cornered response: 0 dB below `start_hz`,
  the slope from `start_hz` to the effective end, flat above, so `total_db`
  is the change from start to end and a `start` pivot leaves the bass below
  the start untouched. `start` puts DC (the flat bass) at exactly 0 dB, `end`
  puts Nyquist (the flat treble) at exactly 0 dB, `center` and Hz pivots put
  the actual response at that frequency at exactly 0 dB. Every design is
  checked from 20 Hz to min(20 kHz, 0.45 * fs): within ±0.1 dB of the target
  outside ±1/6 octave of each corner, within 0.1 + 0.01 * |slope| dB inside,
  and the delivered start-to-end change within 0.1 dB of the requested total;
  a design that misses this is a build error. `|total| < 0.001` dB is an
  identity filter. `set_eq` (and any config apply) designs the tilt at the
  stream rate and rejects one that cannot be built.
  CLI: `avdsp ctl eq tilt [--high] [--slope <db/oct> | --total <db>] [--start <hz>] [--end <hz>] [--pivot start|end|center|<hz>] [--enable|--disable|--off]`.
- Optional `tilt_high` (`TiltConfig`): the treble section of a two-section
  tilt, a bass tilt below X (`tilt`, `end_hz` = X) and a treble tilt above
  Y (`tilt_high`, `start_hz` = Y), each with its own slope or total and
  0 dB between X and Y (the target; the response follows it within the
  tilt tolerance below). Whenever both are present (on or off),
  config validation and chain build require `tilt.end_hz <= tilt_high.start_hz`,
  `tilt.pivot == "end"` and `tilt_high.pivot == "start"`. Each section has
  the single-section rules (errors name `tilt_high`) and its own design; the
  two run as one cascade, checked against the sum of the two targets with
  the same limits. Either may be off, but `tilt_high` needs `tilt`: a
  treble tilt alone is a single `tilt` with pivot `start`.
  Included in the auto preamp, the EQ response and A/B level matching. In an
  A/B patch, `{"eq": {"tilt_high": null}}` drops the treble section and
  `{"eq": {"tilt": null, "tilt_high": null}}` the whole tilt; `avdsp ctl ab
  stage tilt` switches both sections together (off sends that whole-tilt
  patch, so B stays valid whatever shape A takes next). CLI: `avdsp ctl eq
  tilt --high ...` edits `tilt_high` and needs a bass tilt (a new one pivots
  at its start, and the bass section's pivot is set to `end`);
  `--high --off` removes it, and `--off` on the bass section makes the
  treble section the single tilt.
- `Status.eq_tilt` and `Status.eq_tilt_b` describe the enabled tilt stage
  (both sections of a two-section tilt) in chains A and B at the current
  `graph_rate`; `null` means absent/disabled. Each is
  `{ sample_rate, effective_start_hz, effective_end_hz, clamp_note, error }`:
  the start of the first enabled section, the effective end of the last,
  one clamp note per clamped section (`; `-joined, naming `tilt` or
  `tilt_high`), and the first error (the pair rule, then each section).
  `clamp_note` is a human-readable note or `null`; `error` explains an unusable
  effective band/pivot or a design that fails at that rate (the design runs
  once per tilt config and rate, cached), or is `null`. These describe the configuration's
  interpretation, not a claim that DSP is currently running (Direct, DSP off,
  or an engine build failure may keep it out of the audio path). Recomputed
  on every status snapshot, including after rate changes; while no graph rate
  is known, the configured default rate is used. `set_eq` still returns
  `null`; read/subscribe to status for current bounds. For example, at 44.1 kHz:

  ```json
  {"eq_tilt":{"sample_rate":44100,"effective_start_hz":20.0,
    "effective_end_hz":19845.0,
    "clamp_note":"tilt end_hz clamped from 20000.000 to 19845.000 Hz at 44100 Hz (0.45 * fs)",
    "error":null},"eq_tilt_b":null}
  ```

## Post-volume limiter

`ChainConfig.limiter` defaults to `auto_headroom` (a config that stores a
mode keeps it; only a missing `limiter` or `mode` takes the default).
`set_limiter` replaces chain A's
configuration through the usual validated, persisted config edit; returns
`null`. Chain B's limiter follows chain A, like its output configuration:
one shared limiter runs after A/B mixing and level matching.

```json
{"id":1,"method":"set_limiter","params":{"mode":"auto_headroom","ceiling_dbfs":-0.5,"lookahead_ms":5.0,"release_ms":100.0}}
```

- `mode`: `off`, `limit`, or `auto_headroom` (default).
- `ceiling_dbfs`: finite -6..=0 dBFS, default -0.5.
- `lookahead_ms`: finite 5..=20 ms, default 5. Total delay is
  `ceil(output_rate * lookahead_ms / 1000)` frames; detector history is
  included in that delay.
- `release_ms`: finite 10..=2000 ms, default 100. Exponential release time
  constant; attack is a bounded linear gain ramp within the look-ahead.

The order is DC block → pitch → crossfeed → EQ (with tilt) → convolution →
upsampling → harmonics → A/B mix/level match → digital volume/balance → limiter
→ quantizer/dither. All channels share one gain.
Both enabled modes break passthrough; Direct and DSP-off bypass it. The
ceiling applies to f64 samples entering the quantizer, before dither and
rounding (the latter can add a few LSBs). Peak detection density follows the
output rate's family (44.1 or 48 kHz base), whether or not the chain
upsamples: 8 detection points per output sample at 44.1/48 kHz, 4 at
88.2/96 kHz, 2 at 176.4/192 kHz, and the output samples alone from
352.8 kHz up, with 0.2 dB detector reserve. The sample ceiling is enforced exactly; finite FIR
true-peak estimation is not a certificate for every possible reconstruction
filter.

`auto_headroom` learns what the material needs **before** the digital
volume/balance gain, per channel: 0.5 dB steps whenever a detected peak
would cross the reserved ceiling. The applied headroom is the largest
channel need minus that channel's volume/balance attenuation, floored at 0
and capped at 12 dB (beyond it the limiter does the rest), so lowering the
volume gives headroom back at once and raising it (or moving the balance
towards the loud channel) needs it again at once. The need itself is kept
as measured (only the applied headroom is capped), and decisions pause
while the gain ramps. The need never decreases within a stream (no pumping): it
is cleared by a stream stop/reset or a change of output rate, so a quiet
track after a loud one in the same stream keeps it. A chain rebuild with an
unchanged limiter config at the same output rate (an EQ edit, say) carries
it over. A chain that is still warming up in parallel makes no headroom
decisions and counts no events until it becomes the audible chain. A track metadata change alone does not reset
it. A fixed cut before volume is `output.headroom_db` ("Output headroom"),
which applies at every ratio.

`avdsp ctl limiter` inspects the config; `limiter limit --ceiling -0.5`,
`limiter auto-headroom`, and `limiter off` change it. `--lookahead` and
`--release` are in milliseconds. `avdsp ctl status` also prints current
headroom, gain reduction and cumulative events from `meters.limiter`.

## Meters

`meters` events (30 Hz) and the `meters` call carry the same `Meters`
snapshot. Per channel, over the window since the last report:

- `peak_db` / `rms_db`: the output just before the quantizer, after the
  master volume. On the DSP path this is the float signal, so it reads above
  0 dBFS when the quantizer clips; on Direct / bypass it is the integer output.
- `input_peak_db`: the chain input, before the EQ.
- `chain_peak_db`: the chain output before the master volume; empty when no
  float signal went through the chain (Direct, bypass, idle). The UI's
  "chain ±x dB" readout is `chain_peak_db - input_peak_db`.
- `limiter` (optional object): `{ active, headroom_db, gain_reduction_db, events }`.
  `headroom_db` is the applied automatic headroom after the latest block
  (the material's need minus the digital volume's attenuation).
  `gain_reduction_db` is the largest attenuation the stage applied during the
  report window (held headroom included), in positive dB; each read resets
  it. `events` is a running total since daemon start, across chain rebuilds
  (meter reads do not clear it): in `auto_headroom`, decisions that raise
  the applied headroom (steps within one look-ahead span count once; a need
  the volume already covers is not an event); in `limit`, limiting onsets
  (a gain decrease after the reduction had fallen below 0.01 dB). Direct/bypass blocks clear `active` and `headroom_db`, retaining
  events.
- `clipped` (total) and `clipped_ch` (per channel): samples clipped since the
  daemon started.

`Status.filter_latency_ms` is the filter-delay part of `latency_ms`
(upsampler + convolution + limiter in the active chain). It is 0 on Direct
or with DSP off at x1; while the engine bypass resampler converts there, it
is that chain's delay (for its minimum-phase design, the block latency plus
the impulse peak at an integer ratio such as x2, and the grid peak over M,
a few frames, at a rational ratio).

## Status: device and rate fields

All `#[serde(default)]`:

- `waiting_for_device: bool`: the configured device is not there (or
  keeps failing to open right after it went); the sinks run on the
  engine's own clock and the engine opens the device once it is back (see
  "Devices"). `state` is `error` meanwhile.
- `device_caps: DeviceCapabilities | null`: the last probe of the configured
  device, also when that device turned out unusable (then `rates` is empty
  and the other fields say why). `null` before an open, when the probe failed
  or the device could not be opened at all. `{ device, channels,
  channels_min, channels_max, rates: [{ rate, formats: [{ format, sbits }]
  }], rate_min, rate_max, formats }`: `rates` ascending, only rates the
  engine could ask for with a usable format at `channels`; `rate_min` /
  `rate_max` (equal for a fixed-rate device) and `formats` are what the
  device accepts at all, regardless of channel count and the engine's rate
  list.
- `effective_rates: number[]`: the graph rates offered to PipeWire (see
  "Rate policy"); empty before the first open and while the output is closed
  after a failed open.
- `direct_rates: number[]`: the rates Audivane Direct runs at: the effective
  rates the device plays natively, plus, with `bypass_resampler: "engine"`,
  the ones the engine converts to a native rate ("The engine bypass
  resampler"); empty when no device is open or none is native.
- `playable_rates: number[]`: every engine graph rate (allowed or not) the
  device plays with the current settings: natively, through the DSP
  chain's ratio ladder (decimation and fractional ratios included), or with
  the engine bypass resampler. The same rule as `effective_rates`, as if
  every engine rate were allowed: an allowed rate missing here is resampled
  by PipeWire on this device, and a rate here that is not allowed would be
  offered once allowed. Empty while no device is open, or without a probe.
- `bypass_resampler: { mode, active, reason } | null`: the engine bypass
  resampler. `mode` is the configured `"engine"` or `"pipewire"`; `active`
  is true while it converts (Direct or DSP off away from ×1, its chain
  playing); `reason` (a string or `null`) says why it does not convert
  where it could: waiting for its chain, a run-time build failure, or why
  PipeWire still resamples a stream feeding Direct or DSP off (rate
  following off, a rate outside the allowed rates, another stream deciding
  the graph rate, a rate it cannot convert). `null` while no device is
  open.
- `effective_default_rate: number | null` (`null` like `effective_rates`).
- `format: SampleFormat | null` and `dither_bits: number | null`: see "Output
  format and dither"; `null` while no format is negotiated, including in
  `error` after a failed open or switch: status never shows the previous
  device's negotiation.
- `limiter_held_db: number` (default 0): the limiter's applied automatic
  headroom after the latest block, dB `>= 0`, the same value as
  `meters.limiter.headroom_db`, so a client without the `meters` topic
  (the tray popup with the main window closed) can show it. 0 unless the
  engine is `playing` with DSP on, the limiter on and in the path (Direct
  and bypass blocks read 0). Read without resetting the meters; status is
  pushed when it changes (in steps of the held headroom, so rarely).
- `normal_output: string | null`: `node.description` of the session default
  sink when it is not one of Audivane's (where a stream routed to `default`
  plays in per-app routing). `null` in system-wide routing, where the default
  is Audivane DSP, and when there is no other output. The UI offers
  `default` only in per-app routing, as "Not through Audivane (<output>)".
- `ratio` / `output_rate` are what the device was opened at, so a device
  that cannot take the wanted ratio shows the fallback (e.g. `ratio: 1`).
- `rate_ratio: {up, down}`: output rate / graph rate, reduced
  (`{"up":160,"down":147}` for 44.1 → 48 kHz, `{"up":1,"down":2}` for
  96 → 48 kHz). `ratio` is the integer ratio, or `0` while a fractional or
  decimating ratio runs.
- `filter_latency_ms` at a rational ratio is the exact sum of the stages'
  delays in output frames, rounded once to the nearest frame (halves up).
- `rt_load_max: number`: peak RT processing load per period over the last
  one to two seconds (share of one period; `1.0` = all of real time).
- `varispeed: VarispeedStatus | null`: variable-speed stage state from the
  audio thread's applied state (see [Varispeed](#varispeed)). `null` from
  daemons that predate varispeed.
- `limits: EngineLimits | null`: the limits the daemon holds upsampler
  designs to on this machine, for clients that size their controls; `null`
  from daemons that predate it. Fixed for the daemon's lifetime:

  ```json
  "limits": {
    "max_kernel_taps": 8388608,
    "intermediate_max_taps": 2097152,
    "design_workspace_bytes": 4294967296,
    "cpu_share": 0.9
  }
  ```

  `max_kernel_taps`: the longest kernel at an integer ratio (output-rate
  taps); `intermediate_max_taps`: the longest intermediate-phase design
  with a `mix` strictly between 0 and 1; `design_workspace_bytes`: the
  most memory designing one filter may take, a quarter of the machine's
  total RAM clamped to 1.5 GiB ..= 4 GiB (1610612736 ..= 4294967296; the
  floor when the RAM cannot be read); `cpu_share`: the share of one core
  the CPU admission check lets the upsampler take (see "DSP overload").

`avdsp ctl status` prints them as `rates:`, `direct:` (only when it differs
from `rates:`), `device can:`, `dither:` and `varispeed:` (when enabled,
active, held, or in error), plus `(peak X.X%)` on the `engine:` line when
`rt_load_max > 0`.

## Varispeed

Variable-speed playback (tempo and pitch together) sits in the processor
before the chains, so one stage feeds both A/B sides. It runs on the DSP
sink with DSP on only; Direct and DSP off never run it and stay bit-perfect.

**Config** (top level of `EngineConfig`, `#[serde(default)]`, off by default):

```json
"varispeed": { "enabled": false, "speed": 1.0, "ramp_ms": 200.0, "keep_pitch": false }
```

- `keep_pitch` (default `false`): **Keep pitch.** The speed changes the
  tempo only: a key-locked time stretch takes the stage's place (same slot
  before the chains and the multi-device tee, same pull contract, envelope,
  cap, underfeed handling and holds). It adds about 135 ms of delay
  (reported in `latency_frames`), costs about three times varispeed's CPU,
  and keeps running at exactly `1.0` while on. Off: tempo and pitch move
  together, as on a tape machine. The user's own pitch shift
  (`chain.pitch`) works on top in both modes. Transients soften at large
  speed changes (beyond about ±25 %).

- `speed`: finite, `0.5..=2.0` (`1.0` = normal speed).
- `ramp_ms`: finite, `0..=10000` ms. Shorter ramps are raised to the
  envelope's curvature-bounded minimum `T_min` (`≥ 20 ms`); `0` selects
  `T_min`.
- **Quantum rule:** small quanta down to `32` stay allowed (`32..=8192` power
  of two, enforced identically by config validation and the engine). Instead
  of refusing a config or holding at unity, the driver caps the speed range to
  what the negotiated period, quantum, ring allocation and RT processing
  budget sustain, and publishes `speed_limit` and `cap_reason`.
- **Static budget:** while `enabled` is true, `control::config::validate`
  checks `check_varispeed_budget` across `rate.allowed_rates`—including every
  buildable fractional and decimating landing candidate in the
  `MaxRateAny` ladder (the stage's worst case at speed 2 plus the rational
  chains' concurrent pipelines must stay within 800 M MAC/s per channel;
  integer FFT upsampling is not counted). `set_varispeed` and same-rate chain
  edits also check the negotiated `(graph_rate, ratio, format)` and every
  in-flight swap chain still playing or crossfading. A full-config edit that
  disables `varispeed` alongside a heavier filter does not charge the
  incoming chain for varispeed; when that chain only fits without varispeed,
  its publish waits until the RT thread has finished fading out and retiring
  the stage so the two never overlap.
- **Keep pitch budget:** with `keep_pitch` the stretch's cost is counted in
  varispeed's place, at the running graph rate only (config validation
  across `rate.allowed_rates` still counts varispeed's stage, so a config
  allowing 192 kHz stays valid). The stretch alone exceeds the budget at
  176.4 kHz graph rates and above (it fits up to 96 kHz): `set_varispeed` (or a `set_config`) that
  enables Keep pitch there is refused. A negotiation that moves to such a
  rate while Keep pitch is on keeps the request, plays at 1× and reports
  the refusal in `status.varispeed.error`; a rate where it fits brings it
  back.

**Live call (`set_varispeed`):** changes `enabled`, `speed`, `ramp_ms` and/or
`keep_pitch` without rebuilding the DSP chains. All four parameters are optional; omitted
fields keep their current configured value, while explicit `null` values are
rejected. Every `set_varispeed` call—including an empty `{}`—follows the
normal request path (re-applying the configured settings, clearing any
underfeed hold, and ending an active varispeed overload fallback step). To
inspect without side effects, use `status` (`avdsp ctl varispeed` with no
arguments reads `status`).

```json
→ {"id":1,"method":"set_varispeed","params":{"enabled":true,"speed":1.02,"ramp_ms":500}}
← {"id":1,"ok":{"enabled":true,"keep_pitch":false,"active":true,"speed":1.0,"target":1.02,"requested":1.02,"speed_limit":2.0,"cap_reason":null,"ramping":true,"fading":false,"latency_frames":507.5,"latency_exact":false,"held":null,"overload_hold":"none","underfeed_events":0,"underfeed_frames":0,"short_cycles":0,"gap_fades":0,"error":null}}
→ {"id":2,"method":"set_varispeed","params":{"speed":1.1,"keep_pitch":true}}
```

- Toggling `enabled` installs or retires the stage under a 5 ms cross-fade
  without dropping audio (transient RT command-queue backpressure during
  rapid toggles is accepted and reconciled by the reaper thread so config and
  engine state stay consistent); changing `speed` starts a C¹ log-speed ramp
  from the current applied speed and slope.
- Toggling `keep_pitch` while the stage plays cannot cross-fade (the stretch's
  135 ms delay would repeat that much music): the playing stage fades out
  over 5 ms, the other kind starts with its source faded in, and its output
  arrives after its delay. One gap: about 100 ms into Keep pitch, 10 ms out
  of it. Multi-device followers re-sync once (the tee's stream restarts).
  Speed changes with Keep pitch on stay live through the same envelope.
- Persists to `config.json` through the deferred coalesced writer (`Persister`),
  just like volume, and emits `Event::Config` / `Event::Status`.
- Every `set_varispeed` request (or a `set_config` edit that changes
  `varispeed`) clears any underfeed hold and immediately ends an active
  varispeed RT overload fallback step (`varispeed_budget`, `varispeed_unity`,
  `varispeed_off`), releasing its hold; chain overload fallback steps
  (`ab_off`, `reverted`, `upsampler_off`, `bypass`, `light_upsampler`) stay in place
  and keep the stage retired.
- A `set_config` edit that turns varispeed off and installs a chain that only
  fits the static budget without it holds that chain until the stage has
  faded out and retired, at most 1 s (then it plays anyway, with a warning in
  the log). Known limitation (timing-only): the budget's view of which chains
  still run comes from the engine thread a moment late. A live enable or a
  retirement in the same few milliseconds as a chain swap can see a chain as
  gone that still crossfades (or a stage as retired that was just
  installed), so the stage and two chains can briefly run together over the
  800 M budget. The overload fallback covers such an overlap.

**Status (`Status.varispeed: VarispeedStatus | null`):** combines the
host-accepted request (`enabled`, `requested`) with the audio thread's
applied state (`active`, `speed`, `target`, telemetry):

- `enabled`: accepted enable state from the host/config side.
- `keep_pitch`: accepted Keep pitch state (the stage is the stretch; its
  delay is in `latency_frames`).
- `active`: the last block ran through the stage (DSP sink, DSP on, stage for
  the running rate; unity counts as active).
- `speed`: RT-applied speed at the next output (`1.0` while inactive).
- `target`: RT-applied target speed the stage is heading for (clamped to
  `speed_limit`, `1.0` while held or inactive).
- `requested`: accepted requested speed from the host/config side (preserved
  while idle, on Direct, with DSP off, or during an underfeed/overload hold).
- `speed_limit`: upper speed limit (`1.0..=2.0`) from the scheduler cap and
  overload hold.
- `cap_reason`: `null`, `"period_quantum"`, `"quantum_too_small"`, or
  `"ring_size"`.
- `ramping`: a speed ramp is in progress.
- `fading`: a cap-drop fade is in progress.
- `latency_frames`: delay of the stage that runs (varispeed's, including its
  5 ms source look-ahead, or the stretch's, about 6 700 frames at 48 kHz),
  in nominal frames (`0` while inactive).
- `latency_exact`: `true` when constant speed has held across the filter
  span; `false` during/right after a ramp (while `latency_frames` reports the
  constant-speed estimate).
- `held`: `null` or `"underfeed"` (three underfeed events within one second
  held the stage at unity until the next speed command).
- `overload_hold`: `"none"`, `"budget"`, `"unity"`, or `"off"`.
- `underfeed_events`, `underfeed_frames`, `short_cycles`, `gap_fades`:
  cumulative loss and fade counters.
- `error`: why the stage could not be built for the current rate (e.g. static
  budget exceeded after a rate change), or `null`.

**Errors (`INVALID_PARAMS`, code −32602):**

- `varispeed.speed must be between 0.5 and 2 (got <value>)`
- `varispeed.ramp_ms must be between 0 and 10000 ms (got <value>)`
- `varispeed at <rate> Hz needs <stage> M multiply-adds per second per channel at speed 2, and the chains' <ratio> resampling <chains> M with the pipelines that can run at once: <total> M, more than the 800 M budget; use a shorter filter or a lower output rate`
- `Keep pitch is not available at <rate> Hz: the time stretch alone needs <stage> M multiply-adds per second per channel at its slowest speed, more than the 800 M budget; play at a lower rate or turn Keep pitch off`
- `varispeed with Keep pitch at <rate> Hz needs …` (the stretch plus the chains' rational resampling over the budget; same shape as the varispeed message)

**CLI:**

```sh
avdsp ctl varispeed                       # inspect current varispeed status
avdsp ctl varispeed 1.02                  # enable at 1.02x (keeps saved ramp_ms)
avdsp ctl varispeed 1.02 --ramp-ms 500    # enable at 1.02x with 500 ms ramp
avdsp ctl varispeed off                   # disable (keeps saved speed and ramp_ms)
avdsp ctl varispeed on                    # re-enable at saved speed and ramp_ms
avdsp ctl varispeed 1.1 --keep-pitch      # 1.1x tempo at the original pitch
avdsp ctl varispeed --no-keep-pitch       # speed and pitch together again
```

`--keep-pitch` / `--no-keep-pitch` combine with a speed, `on` or nothing
(Keep pitch only, enabled state unchanged); they are kept until changed.
`--keep-pitch` with `off` is refused.

## Multi-device output

The engine's device (the **master**) plays as always; up to three
**followers** play the same source on other DACs, each on its own thread,
in sync with the master's DAC through a drift loop, each with its own
instance of the master's chain.

**Config** (top level of `EngineConfig`, `#[serde(default)]`, no followers
by default):

```json
"outputs": {
  "followers": [
    { "device": "hw:CARD=USB,DEV=0", "enabled": true, "trim_ms": 1.5, "volume_db": -3.0 }
  ]
}
```

- `device`: an ALSA `hw:` device, not the master's, each listed once; at
  most 3 followers.
- `enabled` (default `true`): heard. Disabled, it keeps playing in sync,
  muted, and keeps its device.
- `trim_ms` (default 0): its delay behind the master, `-50..=50` ms
  (negative plays it earlier): the DACs' own latencies, speaker distance
  (1 ms ≈ 34 cm).
- `volume_db` (default 0): its digital volume, `-120..=0` dB, after the
  chain's.
- While any follower is configured, an enabled varispeed's `speed` is at
  most **1.996** (each follower corrects its clock on top of it).
- **Budget across devices:** at every allowed rate, each follower's
  clock-correcting stage (its varispeed stage at speed 2 in the worst case)
  plus chain A's resampling at the master's ratio (twice, for a swap) must
  fit the 800 M MAC/s per channel of one thread, and the master and the
  followers together must fit that times the cores they can run on.
- Chain commands take no device: an edit of the master's chain reaches
  every follower (built for its device, swapped in under a crossfade). A/B
  stays on the master. Followers mute while the master plays Direct or with
  DSP off.
- An `outputs` edit never rebuilds or reopens the master.

**Live call (`set_follower`):** adds, edits or removes one follower through
the ordinary config edit (validated, applied, persisted). A `device` not
listed yet is added (omitted fields at their defaults); `remove: true`
removes it (an unlisted one is refused). Omitted fields keep their value;
explicit `null` values are rejected. → `OutputsStatus`

```json
→ {"id":1,"method":"set_follower","params":{"device":"hw:CARD=USB,DEV=0","trim_ms":1.5}}
← {"id":1,"ok":{"alignment_ms":0.0,"followers":[{"device":"hw:CARD=USB,DEV=0","enabled":true,"trim_ms":1.5,"volume_db":0.0,"state":"opening","offset_ms":0.0,"ppm":0.0,"phase_error_frames":0.0,"bandwidth_hz":0.0,"skips":0,"xruns":0,"latency_ms":0.0,"device_rate":0,"period":0,"error":null}]}}
→ {"id":2,"method":"set_follower","params":{"device":"hw:CARD=USB,DEV=0","remove":true}}
```

**Alignment.** A follower reads the source only once the master has, so the
master is delayed (`alignment_ms`) until the slowest follower has a little
source ahead. It grows by whole milliseconds, at most once a second, never
past **60 ms**, and never shrinks while a follower plays. A follower whose
path needs more (large periods) plays **unaligned**: later than the master
by `offset_ms` (grown the same way, at most 200 ms), locked and heard. A
smaller period on that follower brings it back. Each of these changes, a
trim edit, and switching varispeed on, is a step the followers take
exactly: muted for their 5 ms fade out and in, not counted in `skips`. An
unaligned follower cannot play earlier than it does: a trim that moves it
earlier adds to its `offset_ms` instead (a later trim is taken as is).

**Status (`Status.outputs: OutputsStatus`):**

- `alignment_ms`: the master's added delay (0 without followers).
- `followers[]`, in config order: `device`, `enabled`, `trim_ms`,
  `volume_db`, and
  - `state`: `"off"` (not running: the engine is stopped, or it could not
    start, see `error`), `"opening"`, `"locking"` (playing, not in sync yet,
    muted), `"locked"` (in sync, heard), `"muted"` (in sync, not heard:
    disabled, or the master plays Direct or with DSP off), `"unaligned"` (in
    sync and heard, `offset_ms` late), `"lost"` (its device is gone or
    failed; it reopens when it is back).
  - `offset_ms`: how much later than the master plus its trim it plays (0
    when aligned).
  - `ppm`: its clock against the master's (the drift loop's estimate);
    `phase_error_frames`: the loop's filtered phase error, source frames;
    `bandwidth_hz`: the loop's bandwidth (it narrows as it locks).
  - `skips` (phase jumps the loop corrected by skipping frames; the known
    steps above are not counted), `xruns`,
    `latency_ms` (its own path from source to DAC), `device_rate` and
    `period` (0 while closed), `error`.

**Errors (`INVALID_PARAMS`, code −32602):**

- `outputs.followers: at most 3 followers (got <n>)`
- `outputs.followers[<i>].device <d> is the main output device; a follower plays another one`
- `outputs.followers[<i>].device <d> is listed twice`
- `outputs.followers[<i>].trim_ms must be between -50 and 50 ms (got <value>)`
- `outputs.followers[<i>].volume_db must be between -120 and 0 dB (got <value>)`
- `varispeed.speed is at most 1.996 while followers play (got <value>): …`
- `a follower at <rate> Hz needs <n> M multiply-adds per second per channel …`
- `the main output and <n> follower(s) at <rate> Hz need <n> M multiply-adds per second per channel together …`
- `<device> is not a follower` (`remove` of an unlisted device)

**CLI:**

```sh
avdsp ctl outputs                                    # alignment and every follower
avdsp ctl follower add hw:CARD=USB,DEV=0 --trim-ms 1.5
avdsp ctl follower hw:CARD=USB,DEV=0 --volume-db -3  # edit a listed follower
avdsp ctl follower hw:CARD=USB,DEV=0 --enabled off   # mute it, in sync
avdsp ctl follower remove hw:CARD=USB,DEV=0
```

## DSP overload

Two parts keep a chain that costs more than real time from taking the
engine down (the audio thread's guard is in `architecture.md`, "Overload").

**Admission.** A client change that brings a new upsampler (`set_upsampler`,
`set_config`, `set_ab`, a preset load; also a rate policy, quantum, device,
format or DSP on/off change, which move where the current one runs) is
estimated where the driver would land it: at every graph rate the driver
would offer (the configured rates the device plays, widened to the engine's
rates the device plays when none does) and the ratio a fresh open picks
there (the same ladder, `MaxRateAny` and rational landings included),
chain A and B together. It is refused with `INVALID_PARAMS` when it needs
more than 90 % of a core (`status.limits.cpu_share`, 0.9):

```
Too heavy for this CPU at 768 kHz: about 31.2 G MAC/s needed (312% of one
core), this machine sustains about 5.0 G MAC/s in real time. Use fewer taps
or a lower output rate.
```

When chain A alone fits and A/B's side B (its own filter) tips it over,
the refusal names side B: `Too heavy for this CPU at 768 kHz with A/B:
side B's upsampler needs about … on top of chain A's, … Pick a lighter
filter for side B, or turn A/B off.` At a rational ratio, the engine's
per-side check names `ab.chain_b.upsampler`.

The figures are multiply-adds per second at this machine's in-cache dot
rate (the share of a core times that rate, both channels), measured by a
~0.2 s benchmark when the engine starts. A result under a conservative
floor (an absolute figure under what an older AVX2 laptop core at about
2 GHz sustains, so raising a result to it never claims more than a slow
machine has) is measured again, at most twice, and the best of each kernel kept; what is still under the floor
is raised to it (and the log line says so), so a busy start never refuses
ordinary designs. An edit that leaves those fields
alone (EQ, volume, ...) is never refused, and the config the daemon starts
with is not checked: an overload there is handled by the fallback.

**Fallback.** The daemon samples the engine every 100 ms, serialized with
config edits. While the DSP sink plays the chain in effect (its generation
active on the audio thread), a second in which 8 of 10 samples show
`dsp_load >= 1.0`, or in which at least 3 underruns hit 10 % or more of the
periods the audio thread processed (cumulative counts, across reopens), is
an overload; so is a demotion of the audio thread from real-time to
normal priority after `SIGXCPU`, which is latched through startup and a
step's settling until the chain can be judged. A signal that finds the
thread at normal priority already, or a demotion the kernel refuses, is
no overload (the second is logged): the driver counts signals, refused
demotions and real demotions apart. A demotion is published with the
generation of the chain that ran on the audio thread when it happened (the
active one, or one warming up next to it before it takes over, whose cost
is the thread's then), and counts against that chain only: one of the old
chain right before an edit's chain starts never rolls the edit back. The
engine then runs, one step at a time with 2 s for each to take
effect, only steps the device can play (a rate the driver would land at
for that config).

While varispeed's stage runs, its steps come first. They leave the config
alone (`varispeed` stays as configured): the engine takes them as a hold on
varispeed, and each one's attempt is the
chain already running (its generation) under that hold. The hold carries
a sequence the audio thread acknowledges once it applies it (for
`varispeed_off`, once the stage is gone): the step is `applied`, and what
runs is judged, only then, and a demotion counts for the step only when it
was published with the step's generation and hold sequence or later ones
(one from before the hold applied is not the step's):

- `varispeed_budget`: its processing budget pinned at half the period,
  only when the measured load had lowered it (the speed range narrows to
  what the period and quantum sustain then);
- `varispeed_unity`: its speed held at 1×, only when the request is
  faster;
- `varispeed_off`: the stage retired (the request is kept). Every later
  step keeps it off, and its message says so.

Then the chain's steps:

0. A/B off, keeping chain A, while A/B is on (`ab_off`; side B may carry
   its own heavy filter). Every later step keeps A/B off, and its message
   says so. A new side B (`ab.b` changed, not chain B following an edit of
   A) ends the fallback and tries A/B again;
1. the last upsampler setting that ran without overload in this daemon run
   (3 s calm with no fallback, DSP on, its chain active, the device at an
   upsampled rate; the count restarts when the chain generation, the
   output rate or the ratio changes); the config the daemon started with
   counts once it ran so, like an edit;
2. the upsampler off, when the device plays a rate at x1 (the driver may
   move the graph to one, PipeWire converting the sources);
3. the bypass path (DSP off): at x1 where the device plays the graph rate,
   else through the engine bypass resampler (resample-only: its
   recommended design, volume and dither, nothing else of the chain; the
   message then says so);
4. only when neither lands, on a device whose rates are all above the
   graph rates (say 1.536 MHz only; the bypass resampler converts to graph
   rates only) or with `bypass_resampler: pipewire` there: the bypass
   resampler's recommended design at the requested target and output
   rate, with the rest of the chain (`light_upsampler`).

A step is committed only when the engine accepts its config. Each
config the engine accepts for the step in effect (its commit, or a device,
format or rate-policy edit it survives), and each new DAC found behind the
same name, starts an *attempt* with its own readiness, timeout and
confirmation: it is ready (and `applied`) once its chain landed with the
engine neither switching nor in `Error`. Until then:

- an `Error` is the attempt's failure only when the device answered a
  probe during the attempt (the driver probes again after every failed
  open, and drops the capabilities of an earlier probe when the device
  does not answer); with the device unplugged or not probed again, the
  fallback waits;
- ten seconds with a client streaming to the DSP sink and the engine still
  in `Switching` (the audio thread waiting for the chain before it
  resumes) is a failure too; with nothing streaming, the step is judged
  anyway.

A refused step and a failed attempt move on; so does a step that no longer
lands, checked again whenever the device's probed capabilities or the
config the step runs change. The next step is the next one that plays
there, a lighter one first, else from the top of the list, each tried at
most once per device and policy. When none is left, the fallback ends and
the engine runs the request; if the engine refuses that, the end stays
pending and is asked again every 2 s (logged once). The saved config is unchanged
(`get_config`, `config.json`); the fallback holds until the upsampler, the
output or A/B changes (after a bypass: any DSP setting; after a varispeed
step also DSP on or off, or a new varispeed request, which ends it and
judges the request afresh), or the daemon restarts, which falls back the
same way. Other edits apply on top of it. An
edit the engine refuses leaves the fallback and its notice as they were.

Each step logs one line and sends an `overload` event when the engine
accepted it (`applied: false`), and another once the audio thread runs it
(`applied: true`; a step that reopens the device lands later; the pump may
send only the second; a step whose chain has not landed after ten seconds
is judged anyway). `Status.overload` carries it while it is in effect
(`null` otherwise):

```json
{"event":"overload","data":{"stage":"upsampler","output_rate":768000,
 "load":1.4,"action":"upsampler_off","episode":1,"applied":true,
 "message":"The engine switched the upsampler off: it needed more CPU than real time allows (load 140%). Your setting is kept; lower the taps or the output rate."}}
```

- `message`: names the cause: the load over real time (quoted, as
  above), underruns in many periods, or the audio thread moved off
  real-time priority; the load is quoted only when it was the cause.
- `stage`: `"varispeed"` for a varispeed step, `"ab"` for `ab_off`, else
  `"upsampler"`, or `"chain"` when the step ran without one.
- `action`: `varispeed_budget`, `varispeed_unity`, `varispeed_off`,
  `ab_off`, `reverted`, `upsampler_off`, `light_upsampler`, `bypass` (`none` is
  reserved).
- `load`: mean DSP load over the second that tripped it.
- `episode`: fallback steps since the daemon started.
- `applied`: the audio thread runs the step (false: accepted, landing).

`avdsp ctl status` prints it as `overload:`, `avdsp ctl watch` as an
`overload:` line.

## Spectrum

`spectrum` events arrive at about 20 Hz and are computed only while some
client is subscribed. The RT thread copies samples into preallocated
wait-free rings (dropping whole frames when full: no allocation, no lock); a
non-RT thread analyses them. A drop starts the tap afresh, so a spectrum is
never computed across a gap. A period larger than half the post ring (×64
at a 2048 quantum or more) is queued as the mean of each 2, 4 or 8 output
frames, so it still fits whole. `post` is an empty array when the post tap
has produced no analysis for 400 ms while `pre` has (its audio keeps being
cut by gaps): `pre` is then sent alone rather than held back. Each tap (`pre` = chain input, `post` = chain output before the
quantizer, at the output rate) runs an 8192-point Hann FFT (50% overlap) and,
for log bins narrower than four of its bins (below ~800 Hz at 48 kHz), a
32768-point FFT (87.5% overlap, same hop).

- Bin `k` is centred on `f_min_hz * 2^(k / bins_per_octave)` (20 Hz, 24 per
  octave) and covers up to min(20 kHz, Nyquist).
- A bin reads band power: the interpolated power density integrated over the
  bin, divided by the Hann ENBW, so a sine reads 0 dB and noise reads its
  power in the bin regardless of FFT size (white noise rises 3 dB/oct, pink is
  flat). Where a local maximum's parabolic vertex falls inside a bin, that bin
  reads the higher value. Values are channel-mean power in dB, rounded to
  0.1 dB.
- Queued samples carry a rate epoch; a rate change resets the analyser, so
  pre-switch samples are never analysed at the new rate.
- The UI applies 1/3 to 1/24-octave smoothing (default 1/12).

## Filter designs

Two calls let the app edit a design before it is applied. Neither changes
the config.

- `resolve_design { preset }` → the `FilterDesign` a catalogue preset (any
  `FilterPreset` name or `"recommended"`) expands to, with `based_on` set to
  the name: the starting point of "Edit as custom". An unknown name is
  `INVALID_PARAMS`.
- `check_design { design, target?, source_rate? }` → `{ errors, plans }`.
  It runs the per-field bounds and, at each source rate, the ratio rules
  and budgets (`feasibility`): no kernel is designed and no automatic-length
  search runs, so it answers in well under a millisecond and can be called
  as the user types. The context comes from the current config:
  - `target` defaults to chain A's (pass one to ask "what if");
  - `match_output` resolves against chain A's output, its dither bits
    capped at the negotiated format;
  - the rates are `rate.allowed_rates` (or only `source_rate`, which must
    be 8000..=3072000); integer ratios are also checked at the other engine
    rates, as the config validation does, but get no plan there; rates
    where the target resolves to x1 are skipped;
  - the rational pipeline count follows A/B (2, or 4 with A/B), and the
    block size is the engine's at each rate (`quantum`).

  `errors: [{ path, message }]` names the field each refusal is about:
  `design.window.beta` style for a bound, `target` or `source_rate` for
  the context, `design.rates` for a ratio the design does not run at,
  `design.length` (`design.lobes`, `design.block`) for a budget,
  `design.preserve_samples` at a decimating ratio, `design.cutoff`,
  `design.phase` or `design.window` for a band, phase or attenuation the
  design cannot keep, `design` otherwise. A refusal at a rate starts
  `at 44100 Hz (160/147), `; one refusal per field and message is kept (at
  the first rate). When a bound fails, no rate is checked.

  `plans: [{ source_rate, ratio, plan }]`, one per allowed rate where the
  design upsamples and passes. With a probe of the device, `ratio` is where
  the engine would open it for that source rate with this target (the
  driver's rate policy and ratio ladder, as `upsampler_response` resolves
  it), and a rate it does not offer there, or lands 1:1, has no plan; the
  refusals stay at the target's own ratio. A `plan` (`DesignPlan`) holds `grid_up` (L)
  and `grid_down` (M), `grid_taps` with `grid_taps_exact` (false for an
  upper bound, such as a reflected intermediate phase, or an automatic
  length's estimate before its search), `workspace_bytes` (peak memory
  while it is built), `resident_bytes` (what the running filter keeps,
  both channels), and `cpu_load` (the fraction of one core one pipeline
  takes on this machine, as the admission check prices it; omitted when it
  cannot be priced). `bounded_peak_db` (the largest output a full-scale
  input can produce) is only known once the kernel is designed, so only
  `upsampler_response` carries it.

A design refused by `set_upsampler`, `set_config` or `upsampler_response`
names the field the same way in its message
(`chain.upsampler.design.length: at 44100 Hz (x8), ...`).

```text
→ {"id":1,"method":"resolve_design","params":{"preset":"lanczos23"}}
← {"id":1,"ok":{"based_on":"lanczos23","kind":"lanczos",...}}
→ {"id":2,"method":"check_design","params":{"design":{...},"target":{"ratio":70}}}
← {"id":2,"ok":{"errors":[{"path":"target","message":"ratio must be between 1 and 64 (got 70)"}],"plans":[]}}
```

## Upsampler response

`upsampler_response` builds the real `dsp_core::resample::Upsampler` for a
design (default: chain A's upsampler at the current graph rate, else
44.1 kHz), pushes a full-scale impulse through it and reports the measured
response: magnitude on ~600 log points plus dense points around the source
Nyquist, the impulse around its peak, passband ripple, the stopband start
and attenuation, latency, and `crossings` (the first frequency at which the
response falls to each level from -0.0001 to -150 dB, bisected to < 0.001 Hz;
levels deeper than the stopband attenuation minus 3 dB are omitted).

- `window: { f_lo_hz, f_hi_hz, points }` additionally evaluates that range
  directly from the measured impulse (zoom). It must lie within
  0..output Nyquist with `f_lo < f_hi` (else `INVALID_PARAMS`); points are
  clamped to 2..=4096.
- Measurements are cached per (design, source rate): the newest always,
  older ones within 16 Mi samples, so zooming never re-measures.
- `taps` is the kernel length actually used (chosen by the engine for band
  designs, usually longer than configured for intermediate phase), and
  `effective_cutoff` / `effective_kaiser_beta` the design's cutoff and beta,
  `filter_window` the window. When `preset` is set (`Option<FilterPreset>`,
  omitted for manual designs), `taps` is the measured non-zero impulse span,
  `stopband_start_hz` uses a fixed -150 dB threshold instead of a Kaiser
  beta, `filter_window` / `effective_cutoff` / `effective_kaiser_beta` carry
  neutral placeholders, and time-varying `block_fft*` presets report the
  response for one block position (an impulse at the start of a block).
  The response is the filter's own: `output.headroom_db` is not applied to it.
  `sample_leak_db` is the largest impulse value at the input sample
  instants (peak ± whole input periods) relative to the peak: about -250 dB
  or lower means the filter passes the input samples through.
- `design` is the design that was measured, resolved (a preset expanded,
  `match_output` resolved for the output), with `based_on` naming the
  preset; `plan` is its `DesignPlan` once designed (an automatic length
  searched), with `bounded_peak_db` and `cpu_load` at the engine block for
  `source_rate`. Both are omitted at x1.
- With upsampling off the reply is flat (`ratio 1`, impulse `[1.0]`).
- With a probe of the device, the target is resolved the way the engine
  would open the device for `source_rate` (DSP on, the driver's rate
  policy and ratio ladder; a fresh open), and that ratio is measured.
  `device_landings: [{ source_rate, ratio, requested, converted_to?,
  best_for_dac? }]` lists it for every allowed rate and `source_rate`:
  `ratio` where the engine opens the device, `requested` the target's own
  ratio (`resolve_rate_ratio`), `converted_to` the graph rate PipeWire
  converts a stream at a rate the engine does not offer on this device
  into (`ratio` is 1 then, and so is the measurement), and `best_for_dac:
  { target, ratio }` the `max_rate_any` target (the same cap, or 768 kHz
  for a `ratio` target) and where it would open, when this target lands
  1:1 or converted there and that one would not (never for `off`, `rate`
  or `max_rate_any`). Empty without a probe: the target's own ratio is
  measured.
- `rate_ratio` is the measured ratio. At a fractional or decimating ratio
  `ratio` is `0`, the response is the kernel's on the intermediate grid
  (`source_rate × up`; every output phase samples it), so `freqs_hz` and the
  stopband run to that grid's Nyquist and the band edge is the lower of the
  two Nyquists; `latency_samples` is in output frames, and
  `sample_leak_db` is 0 for decimation (no input instants survive). A
  `window` must lie within the grid's Nyquist.
- It measures every ratio the engine runs, ×1 to ×64 (`source_rate`
  8000..=3072000): a ratio above ×64 or a source rate out of range is
  `INVALID_PARAMS` with a plain message (`upsampler.target ratio must be
  between 1 and 64 (got 65)`, `source_rate must be between 8000 and 3072000
  Hz`), which the app shows as text under the plot.
- It runs outside the control lock; very long kernels take a moment. The
  tap limit bounds it at every ratio: the longest designs (4 M taps) take
  about 10 to 15 s at ×64 (release build), the same order as at ×2.

## A/B, self-test and device release

- `set_ab` configures side B (or disables A/B with `null`). Both chains
  run all the time; `ab_select` switches with a crossfade (default 10 ms).
  The reply is `AbStatus` plus optional `notes`.
- `AbConfig`: `{ b, label?, chain_b, level, headroom }`. `level` and
  `headroom` default when absent:
  - `b` describes side B, either way:
    - `{"patch": <JSON merge patch>}` (RFC 7386) on chain A: B is chain A
      plus that change, and the daemon re-applies it on every config edit
      and load, so B follows every later edit to A outside the patched
      fields. One addition to RFC 7386: a patch that only removes fields
      from a stage A lacks leaves it absent (`{"eq": {"tilt": null}}` with
      no EQ in A is still no EQ). Arrays (`convolution`, `eq.bands`) are
      replaced whole; a patch that replaces one with a non-empty list keeps
      that list when A's changes, and `set_ab`'s `notes` say so.
    - `{"chain": <ChainConfig>}`, a full chain (a stored preset). It keeps
      its own stages and upsampler filter (`preset`, `design`) at A's
      ratio; its `upsampler.target`, `limiter` and whole `output` (bits,
      dither, headroom) are realigned with A (stored realigned, and
      `notes` say what changed), so a preset never becomes an output
      compare: only a patch sets B's bits and dither.
  - What must match A: the upsampler target (one ratio, one device open),
    the limiter (one after the mix) and `output.headroom_db` (taken before
    the mix). The device, its formats and the rate are the engine's. A
    patch that touches `upsampler.target`, `limiter`, an `output` field
    other than `bits` and `dither` (`output.headroom_db`, or anything else
    such as `output.format`), `device`, `formats`, `format` or `rate`, is
    not an object, or does not apply to chain A (a value of the wrong
    type) is `INVALID_PARAMS` naming the patch path
    (`ab.b.patch.upsampler.target may not differ from chain A ...`), in
    `set_ab` and `set_config`. The filter design may differ. A B that
    stops applying after a load (a hand-edited file) turns A/B off with a
    note rather than failing.
  - **Output compare.** A patch B may set its own `output.bits` and
    `output.dither` (the noise shaper is a dither kind):
    `{"b": {"patch": {"output": {"bits": 16, "dither": "shaped"}}}}`. Both
    are capped to the negotiated container as chain A's are. When they
    differ from A's after that cap, the pair runs a second quantizer with
    B's settings; the shared volume and limiter stay before both. B's
    stages are built with A's bits and dither, so an automatic
    (`match_output`) filter budget resolves the same on both sides and B's
    settings change its quantization only. Both
    quantizers quantize the shared mix on every block, so each one's
    dither and shaper state stay continuous and neither starts cold at a
    switch. Two quantized streams cannot be crossfaded without quantizing
    again, so a switch is a **hard cut** from one quantized stream to the
    other at the **middle of the crossfade**: an output frame is B's
    quantizer's when its crossfade B weight (as it leaves the limiter) is
    at least one half, A's otherwise; with `crossfade_ms` 0 it is the
    first frame after the switch. Both streams carry the same signal, so
    the step at the cut is the difference of the two quantized streams:
    their quantization noise (a few LSB of the coarser one for `none`,
    `tpdf` and `shaped`: about 4 LSB for 16-bit shaped against 24-bit
    TPDF; more for the louder shaped noise of the fixed-coefficient
    modes) plus, when a fixed-coefficient mode is involved, its reserved
    headroom: that mode scales the signal down by 1 to 16 LSB at full
    scale (`integrator15` reserves 16), so the step grows with the level
    (16-bit `none` against `integrator15` stepped 19 LSB on a 1 kHz tone,
    about −65 dBFS). With the same bits and dither there is one
    quantizer and the output is unchanged. The CPU admission check counts
    the second quantizer with side B, at the benchmarked rate of B's
    dither kind (at 768 kHz stereo about 2 to 7 % of a core, by kind),
    after capping both sides' bits to the format the device opens in, as
    the engine does: a B that only differs above the container shares A's
    quantizer and costs nothing.
  - Example, a stage compare:
    `{"b": {"patch": {"crossfeed": {"enabled": false}}}, "label":
    "Crossfeed off"}`. A filter compare: `{"b": {"patch": {"upsampler":
    {"preset": "gaussian_long", "design": null}}}, "label": "Filter:
    gaussian_long"}`.
  - `label`: free text naming B, set by the client (omitted when unset).
  - `chain_b`: chain B as the daemon materialised it. Read-only: any value
    sent in is overwritten.
  - `level`: `{"mode": "analytic", "weighting": "k" | "a"}` (default, `k`),
    `{"mode": "manual", "offset_db": -3.0}` (−60 to 60),
    `{"mode": "measured", "offset_db": -0.84, "seconds": 31.5}` (what
    `ab_measure` locks; −60 to 60, `seconds` ≥ 0) or `{"mode": "off"}`.
  - `headroom`: `{"mode": "auto"}` (default) or `{"mode": "fixed", "db": 3.0}`
    (0 to 12).
- **Never boost.** For B's offset `o` (dB that make B as loud as A) the
  engine plays A at `gain_a = min(0, −o)` and B at `gain_b = gain_a + o`:
  the louder side is cut and nothing is raised. Then the **shared headroom**
  `H` cuts both sides equally, so it never changes the comparison: `auto`
  is `max(0, gain_a + P_a, gain_b + P_b)`, capped at 12 dB, where `P` is the
  side's predicted sine worst-case peak gain (per channel, EQ `max |H|` with
  every preamp, negative when the EQ only cuts, plus convolution `max |H|`;
  the louder channel, times the crossfeed bound), minus `output.headroom_db`
  (cut before the mix, so peaks it already took off need no more). The
  comparison plays up to
  `|o| + H` quieter than chain A alone. Gains are static per comparison,
  never a moving AGC; a change ramps over 50 ms.
- **The analytic offset** integrates each side's power response over a
  20 Hz–20 kHz pink reference, K-weighted (BS.1770, as LUFS) or A-weighted:
  DC block, EQ (tilt and preamp included), crossfeed as a stereo stage at
  inter-channel correlation 0.5, the convolution IRs' magnitude (their FFT)
  when B's upsampler design differs from A's, both upsamplers'
  magnitude, and the harmonics stage's loudness change, metered in LUFS on
  4 s of loud pink noise (the anti-clip scale, the compression on loud
  passages and the added harmonics), each channel's EQ and IR taken
  together. A built pair computes
  it from its own stages and IRs. Until the engine has built one (at
  start-up, while idle or with DSP off, before a reopen), status and the
  `ab_select` reply estimate the gains from the configs (once per chain
  pair, level and rate), with convolution by its `gain_db` only. A pair
  with a manual offset or a fixed headroom is never reported bit-perfect.
- `AbStatus`: `{ enabled, current, offset_db, gain_a_db, gain_b_db,
  headroom_db, overs: {a, b}, level, measuring, measure_error, error }`.
  `level` is the config's level mode in force; `measuring` is `null` unless
  `ab_measure` runs, and `measure_error` says why the last one ended
  without a lock (below). `gain_*_db` are the never-boost
  gains (≤ 0, before the headroom), `headroom_db` is `H` (≥ 0). `overs`
  counts overs since the last `set_ab` or `ab_select`: each limiter event
  (a held-headroom step in `auto_headroom`, a limiting onset in `limit`,
  intersample detections included) and each output frame the quantizer
  clips (full scale included). Each counts to A and B in proportion to the
  crossfade position of the frame it belongs to: the frame that caused the
  limiter event, or for a clip the frame that entered the limiter its
  latency earlier. The counts can be fractional. In an output compare a
  clip counts whole to the side whose quantizer produced the frame. `error` (string or `null`) says why an analytic offset could
  not be computed, e.g. a chain B tilt that cannot be built at the stream
  rate; the gains are then 0.
- **Measured matching ("Match to this track").** `ab_measure` takes
  `{"max_seconds": 60}` (10 to 300, default 60) to start, `{"accept": true}`
  to lock the running value now, or `{"cancel": true}` to stop and keep the
  current level; it replies with `AbStatus`. `INVALID_PARAMS` without A/B
  or with `max_seconds` out of range; `accept` is `BAD_STATE` with no
  measurement or nothing measured yet. A start while one runs restarts it.
  - While it runs, playback continues at the current offset. The audio
    thread meters both sides after their chains (time-aligned, before the
    level gains): BS.1770 K-weighting, 100 ms hops of each side's mean
    square. Nothing runs without a measurement, and the output is
    unchanged. A NaN or an infinity voids its hop and restarts that side's
    filters.
  - The daemon makes BS.1770 400 ms blocks (75 % overlap) from the hops
    and gates them **on A**: a block counts when A passes the absolute
    gate (−70 LUFS) and the relative gate (10 LU below A's mean); the same
    blocks are used for B. The offset is A's gated loudness minus B's.
  - `measuring: { seconds, offset_db, stable, max_seconds }`: `seconds` is
    the audio that contributes to the offset, the blocks that pass both of
    A's gates (silence, pauses and quiet passages do not count, so the
    measurement waits; a rise of the relative gate can take some out
    again); `offset_db` is `null` until a block passes; `stable` says the
    lock rule holds.
  - **Lock rule.** Once at least 20 s contribute and the offset moved by
    less than 0.1 dB (max − min) over the last 10 s of contributing audio
    (when the count steps back, the 10 s start again),
    or at `max_seconds` of it, the daemon writes `level: {"mode":
    "measured", "offset_db", "seconds"}` (offset rounded to 0.01 dB) to the
    config. It never moves again. At `max_seconds` without an offset (B
    silent) the run ends with `measure_error: "no match: …"`.
  - **Without a rebuild.** An edit that changes only the A/B level to a
    fixed offset (`measured`, `manual`, `off`) reaches the running pair as
    a 50 ms gain ramp; the pair is not rebuilt (unless a chain built for
    the previous config is still on its way, which rebuilds as before).
  - **Cancelled** (`measure_error: "cancelled: …"`, the queued hops
    dropped): another track (the now-playing player and its
    `mpris:trackid`, reported as `now_playing.track_id`, or without one
    the player, title, artist and album; a pause is not a change), another
    graph or output rate, or a
    change to chain A or to the A/B config other than its level and label; A/B off
    ends it too. An `accept` after a track or rate change is `BAD_STATE`
    and locks nothing.
- `self_test` plays a known signal through the Direct path and compares an
  FNV-1a hash of the expected left-justified samples with the bytes handed to
  ALSA (`SelfTestResult`).
- `release_device` gives the DAC back to WirePlumber and idles the engine;
  `acquire_device` takes it again. Both complete only after the hand-off has
  actually happened (see [pipewire-driver.md](pipewire-driver.md)).
- `list_streams` returns the current playback streams (`StreamInfo`), the
  same data the `streams` event carries. `volume` is the loudest channel
  volume (linear, as PipeWire reports it); `volume_db` (absent until the
  stream reports its volume, and from daemons that predate it) is the level
  it plays at: loudest channel times the app's own master `volume`, in dB,
  0.01 dB steps. That is the level `set_stream_volume` sets and returns.

## Tests

```sh
cargo nextest run -p audivane-dsp-daemon             # unit + IPC/CLI/MPRIS tests
cargo nextest run --workspace                        # everything
# Real PipeWire (two temporary f3_it_* null sinks, a pw-play stream):
cargo nextest run -p audivane-dsp-daemon --run-ignored all -E 'test(graph_it)'
# ... including the session-default parts (both metadata keys, normal stop and
# a simulated kill -9; briefly changes the default and restores it exactly):
AUDIVANE_IT_DEFAULT_SINK=1 cargo nextest run -p audivane-dsp-daemon \
    --run-ignored all -E 'test(graph_it)'
```

The default-sink tests hold a file lock (`$TMPDIR/avdsp-f3-default-sink.lock`)
so concurrent nextest processes cannot interleave their changes, and the
original values are restored even if a test panics.

The IPC/CLI suite runs the real server and service with a `MockEngine` on a
temporary socket and drives them both with raw protocol lines and the real
`avdsp` binary (built on demand; override with `AVDSP_BIN`). It covers the
preset round trip (save/list/get/load/rename/delete, overwrite refusal,
`modified`, active-preset tracking across a restart), APO import, device
listing, `config`/`presets` events and the coalesced config writer (write
counts and flush on shutdown).

The ALSA device parser is unit-tested against canned `/proc/asound/pcm` and
`/proc/asound/cards` text, so it needs no sound card.

Upsampler CLI examples:

```sh
avdsp ctl upsample --preset recommended --max-rate 768000
avdsp ctl upsample --preset gaussian_long --ratio 4
avdsp ctl upsample --design my-filter.json --ratio 4
avdsp ctl upsample --manual --ratio 8 --auto-taps-match-output --passband-end 0.9 --stopband-start 1
avdsp ctl upsample --auto-taps-db 140 --phase minimum
avdsp ctl upsample --manual-taps --taps 4097 --beta 14 --cutoff 0.95
avdsp ctl design resolve gaussian_long > my-filter.json
avdsp ctl design check my-filter.json --ratio 8 [--rate 44100]
```

`design resolve` prints a preset's expanded design (a file for `--design`).
`design check` runs `check_design` on a file (`-` reads stdin), with
`--ratio`, `--max-rate`, `--max-rate-any` or `--out-rate` in place of chain
A's target; it prints each refusal and each rate's plan, and exits 1 when
the design is refused.

`--preset` (accepting `"recommended"` and all 66 `FilterPreset` names),
`--design <FILE>` (loading a `FilterDesign` JSON file), and `--manual` are
mutually exclusive. `--auto-taps-db`, `--auto-taps-match-output`, and
`--manual-taps` are mutually exclusive and edit a `standard` `windowed_sinc`
design.
