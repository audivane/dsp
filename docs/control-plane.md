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
  its own and, after an upgrade, offers to restart whichever side is older
  (the app also relaunches itself once it sees its binary was replaced);
  `avdsp upgrade` waits for it to equal the installed release.
- Newline-delimited JSON, one object per line, cap **1 MiB** per line. An
  over-long line gets a `PARSE` error and the connection is closed (there is no
  way to resynchronise).
- Errors: `PARSE` (-32700, unparseable JSON), `INVALID_REQUEST` (-32600,
  missing/unknown `method`, missing `id`), `INVALID_PARAMS` (-32602,
  validation), `INTERNAL` (-32603), `BAD_STATE` (1), `IO` (2),
  `STALE_INSTANCE` (3) and `STALE_REVISION` (4), both from
  `set_volume_guarded`, and `SUPERSEDED` (5, see below).
- `subscribe` takes any subset of `meters`, `status`, `streams`,
  `now_playing`, `config`, `presets`, `spectrum`, `devices`. The server sends a `status`/`streams`/
  `now_playing`/`config`/`presets`/`devices` snapshot immediately after the reply, then
  meters at 30 Hz and the others only when they change. Each connection has a
  bounded outbound queue (256 lines); a client that stops reading is
  disconnected instead of stalling the daemon.
- `config` events carry the whole `EngineConfig` after any change from any
  client (including preset load/save, which update `active_preset` and
  `active_part_presets`); `presets` events carry the whole preset list (global
  and per part, as `list_presets`) after a create, delete, rename, import or
  save. Both are detected with generation counters (a new subscriber may see
  one duplicate event on the first pump tick).
- `status` subscribers also get an `overload` event (`OverloadReport`) each
  time the engine falls back after a DSP overload (see "DSP overload").
- `devices` events carry the playback device list (`DeviceInfo[]`, as
  `list_devices` returns it) when it changes: a sound card came or went (a
  USB DAC plugged in or out) or the configured device changed. The daemon
  reads `/proc/asound` once a second while someone subscribes.
- Requests are answered in order on their connection, except the slow
  read-only calls below; clients may pipeline and must match replies by `id`.
  `shutdown` replies first, waits ~20 ms and only then stops the daemon.
- A change that designs a filter (`set_upsampler`, and `set_config`,
  `load_preset` or `set_ab` when they bring a new upsampler or a side B
  that needs one) is answered once the design is built and in place: a
  fraction of a second for most designs, but there is no build limit, so
  the longest kernels take tens of seconds (an 8,388,608-tap linear
  windowed sinc at 48 kHz x16 about 7–11 s on a desktop CPU), and what
  played before plays until then. Calls sent after it on the same
  connection wait for it, and edits on any connection wait for the build
  too (changes are applied one at a time). Two builds sent in a row are
  both built, in order; the last one stays. A client that must stay live
  sends design calls on a connection of their own and waits minutes, not
  seconds, before it calls one lost (the app: `set_upsampler` and
  `check_design` each on their own connection, 10 minutes).
- Slow read-only calls: `harmonics_readout` (tens of milliseconds, more with
  `sweep`), `harmonics_spectrum` (milliseconds; up to a few tenths of a
  second with tape layers at 768 kHz), `upsampler_response` (up to seconds for a design it has not
  measured yet) and `conv_response` (tens of milliseconds to about a second
  for long filters it has not seen at this rate) run on a worker per connection and method, not on the
  connection's reader, so calls sent after them (volume, mute, edits) are
  answered without waiting and may overtake their replies. Each sees at least
  the state left by every call sent before it. Per connection and method at most
  one runs and one waits: a newer call replaces the waiting one, which is
  answered at once with `SUPERSEDED` (5) instead of being computed; the
  running one finishes. Replies of one method keep their order. When the
  client closes its end the calls already sent are still answered; once the
  connection is gone, a waiting call is dropped unrun.

`avdsp ctl` builds exactly these lines. `avdsp ctl watch --json` prints event
lines verbatim, which scripts can read directly. Without `--json`, `watch` prints
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
- `convolution_safety` (default `{"max_boost_db": 12, "subsonic_hz": 20}`,
  left out of the file at its defaults): the convolution guards, see
  "Convolution guards" under "Chain wire shapes".
- `channel_workers` (default `true`): run the per-channel stages after the
  stereo image stage (EQ, convolution, upsampler, harmonics, output headroom)
  on one thread per channel, so a heavy chain gets about twice the CPU, with the
  same output bit for bit; `false` runs everything on the audio thread (to
  compare). A convolution filter matrix couples the channels: it and the EQ
  before it then stay on the audio thread, and the stages after it split.
  Only the main output's DSP chains use it (single and A/B); Direct, DSP
  off, the bypass resampler, mono and followers stay single-threaded, as
  does a machine with one core. A change rebuilds the chain. A missed
  deadline (two blocks) makes the chain play silence until the worker is
  back, then it resets and plays on. How the split, the handoff and the
  deadline work: [architecture.md](architecture.md#channel-workers).
- An empty `device` is filled with the first `/proc/asound/pcm` playback
  device that is not HDMI (`Loopback` cards are only used as a last resort).
  Nothing is ever opened just to pick a default.
- Validation (`control::config::validate`) is shared by every mutation:
  `hw:` device, non-empty formats, power-of-two quantum `32..=8192`
  (`MIN_QUANTUM..=MAX_QUANTUM`, shared with the engine), rates from
  the supported list with `default_rate` included, the upsampler rules below,
  bands ≤ 4096 per channel
  with valid frequency/Q/gain (applied to the linked, `left` and `right` EQ
  sections),
  `dc_block` (`cutoff_hz` 1..=20 Hz, default 5 Hz),
  `crossfeed` (`cutoff_hz` 300..=2000 Hz, `feed_db` 1..=15 dB,
  `width` 0..=2, `width_corner_hz` 200..=2000 Hz),
  `harmonics` (at most 8 layers; Chebyshev `levels_db` in −120..=−20 dB,
  `drive_db` −40..=0, `amount` 0..=1, tilt corners `low` 20..=2000 Hz and
  `high` 200..=20000 Hz, each `db` −24..=24 and `slope` (optional) 6, 12,
  18 or 24, `sag.depth_db` 0..=6,
  `bias.depth` −0.25..=0.25, envelope `attack_ms`/`release_ms`
  0.1..=10000; flux `corner_hz` 1..=5000, `max_boost_db`
  0..=40, `target_h3_db` −120..=−12, `calibration_hz` 5..=1000 and reachable;
  hysteresis `saturation` 0..=1, `width` 0..=1, `amount` 0..=1, `target_h3_db`
  −120..=−12, `calibration_hz` 20..=5000 and reachable, `bass_eq.hz`
  20..=500 and `bass_eq.db` 0..=12 (checked while switched off too),
  `asymmetry` −1..=1; a hysteresis layer is calibrated at every engine rate
  and refused where it cannot reach its target, where a full-scale jump
  would need more than 4 × 128 integration steps, or where its offset makes
  it expand),
  every enabled convolution IR file must exist (combined enabled length capped
  at 4 000 000 taps in the engine; a `matrix` item: no `ir_path` beside it,
  at least one entry, `in`/`out` below the chain's 2 channels, each pair
  once, finite gains, every entry's file present when enabled), A/B side B must have the same `target`,
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
    - `preset: Option<String>` (default `null`): a named catalogue entry, one
      of the 68 `FilterPreset` names below or `"recommended"`. Mutually
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
      - `"optimal"`: a linear-phase low-pass designed directly for its band,
        no window: `method` (`"equiripple"`: Parks-McClellan, the smallest
        largest error, the passband's weighted twice since the engine
        measures it against the DC gain; `"least_squares"`: the smallest
        summed squared error), `length` (`fixed`, `per_input` or `auto`),
        `passband_end` and `stopband_start` (fractions of the lower
        Nyquist, the bounds of a `band` cutoff), plus `decimation`, `phase`,
        `preserve_samples` (must be `false`: not a Nyquist filter), `rates`
        (no fallback window), `numerics` (default `fir` with `scaled`; the
        fir paths only). An even grid length is designed one tap shorter
        (odd, symmetric about its centre tap).
      - `"cascade"`: `{"stage": "fir" | "fir_minimum" | "fir_asymmetric" |
        "frequency_sampled" | "elliptic" | "inverse_chebyshev"}`.
      - `"interpolator"`: `{"method": "nos" | "cosine" | "catmull_rom" | "table_sinc"}`.
      - `"block_fft"`: `{"block": {"input": B} | {"output": P}, "overlap":
        "half" | "quarter_average"}`.
      - `window` (tagged by `"type"`): `kaiser` (`beta`, or `null` to derive
        it from the band attenuation), `gaussian` (`alpha` or `stopband_db`, not
        both), `flattop` (`set`: `hft196d`, `hft223d`,
        `hft248d`), `chebyshev` (`sidelobe_ratio`, edited in dB),
        `nuttall`, `hann`, `tukey` (`taper` in (0, 1]; needs linear
        phase and a fraction cutoff), `dpss` (discrete prolate spheroidal,
        `nw` the time-bandwidth product, or `null` to derive it from the band
        attenuation like a Kaiser beta; `symmetric` sampling only; the
        preset paths, not `standard`). Every window takes `sampling`:
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
        attenuation_db}`, an optional `passband_end_hz` (an absolute
        passband end converted to a fraction of the Nyquist at build time,
        so the band does not scale with the source rate: Recommended's and
        `craven_apod`'s 20 kHz pin, or the Custom editor's "Hz" unit; the
        band is of the lower Nyquist on every path, so it is converted at
        the output rate when decimating: `design::band_rate`). Where the frequency
        does not fit the band at a rate, `passband_end` stands in there
        (see the bounds below).
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
    - No upsampler migration: an older flat `config.json` without the v2
      `target`/`preset`/`design` fields loads with upsampling off (and
      `set_upsampler` expects the v2 shape).
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
    (`band_design`) of at most 8 388 608 taps. An optional
    `passband_end_hz` (above 0) replaces the fraction at build time
    (`design::band_passband_end`). Its converted fraction `f` fits when
    `0 < f <= 1`, `f < stopband_start` and `stopband_start - f <= 1` (the
    band length's calibrated transition); at a rate where it does not
    (20 kHz below 44.1 kHz, or a 30 kHz pin at 44.1 kHz), the design uses
    `passband_end` there instead: the rate still builds, and `check_design`
    says so in `notes`. Under a 0.5 passband end (only a resolved
    frequency gets there) a band length is designed 10 dB deeper, so its
    passband keeps the promise where the wide transition makes a Kaiser
    passband ripple more than its stopband (up to 2.67 times, measured over
    stopband starts 1.0-1.5 and 55-225 dB); above 215 dB, where that would
    pass the 225 dB limit, it is designed at 225 dB with its passband end
    raised to `stopband_start - 0.4`, at most 1 (a longer kernel, flat past the
    requested end). An automatic length measures both bands and takes no
    margin.
    `nyquist_stop` must be feasible (`nyquist_stop_check`).
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
    in (0, 1]; a DPSS `nw` 1..=10 (a reflected phase: up to 20 / pi, about
    6.37, Kaiser's beta limit at `beta = pi NW`; a band or automatic DPSS
    design derives its NW from the depth, up to 190 dB).
  - An `optimal` design takes at most 4 095 grid taps for `equiripple` and
    65 535 for `least_squares` (`an equiripple design is built (its cost
    grows with the square of the length) up to 4095 taps on the grid ...`):
    the exchange costs about 16 M^2 per iteration (about 0.2 s at 4 095
    taps; past 4.6k taps it fails at clusters of lengths from about 90 dB),
    the least-squares solve O(M log M) per iteration (1 s at 65 535 taps,
    12 s at 262 143) and floors at 137 to 153 dB before that. An `auto`
    budget is 55..=150 dB for `equiripple` and 55..=135 dB for
    `least_squares`; `match_output` resolves to at most that. A fixed or
    per-input length is checked at every ratio against Kaiser's depth
    estimate `7.95 + 14.36 df (N - 1)` (`df` the transition in cycles per
    sample on the grid): refused past 160 dB for `equiripple` (f64 no longer
    levels the ripple) and 200 dB for `least_squares` (the solve no longer
    holds the passband), and short of the depth its branches' DC gains
    need against the safety gate's tolerance `tol` (0.01 dB at linear
    phase, 0.1 dB transformed): an equiripple stopband is flat and adds up
    in each of the `L` polyphase branches, so `20 log10((L - 1) / tol)` dB
    (59 dB at x2, 76 dB at x8, 82 dB at x16, linear); a least-squares one
    falls away from its edge, `20 log10(1 / tol) + 6` dB (65 dB linear) at
    any ratio. An automatic length starts from that depth and its search
    measures the branches' DC too. A length the exchange does not level is retried 2 and
    4 taps shorter (zero-padded back); one that still fails is refused (`the
    equiripple exchange lost its alternation at N taps ...`), and an
    automatic search that reaches it at or past its estimate stops (`an
    equiripple design does not reach X dB over this band ...: lower the
    target or use least squares`). At a fractional ratio such as 44.1 to
    48 kHz (160 grid samples per input sample) the equiripple limit allows
    about 25 taps per input sample: use least squares there.
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
    recipe (`"recommended"` or one of the 68 `FilterPreset` names below);
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
      `halfband_tiny`, `hann_nyquist`, `craven_apod`, `lanczos23`,
      `lanczos23_mp`,
      `lanczos511`, `lanczos2047`, `lanczos8191`, `lanczos32767`,
      `lanczos65535`, `short_leaky`, `short_leaky_mp`, `cascade_elliptic`,
      `cascade_inverse_chebyshev`, `cascade_fir`, `cascade_fir_asym`,
      `cascade_fir_mp`, `cascade_fft`, `nos`, `cosine`, `catmull_rom`,
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
- `active_preset: Option<String>` records the global preset chain A was
  last saved to or loaded from, and `active_part_presets: { <part>: name }`
  (left out when empty) each page's preset. Both are part of the config,
  not of a preset.
- **Targeted setters.** `set_config` replaces the whole config, so a
  client's copy read before another client's change writes the old value
  back (an app editing the output format right after `avdsp ctl volume
  --hardware-db -40` would put the old hardware level back). Every edit a
  client makes has its own call that changes only its field, validated and
  normalized like any edit: `set_output` (`OutputConfig`, chain A's
  `chain.output`; chain B follows), `set_formats` (the `formats` list),
  `set_rate` (`RatePolicy`), `set_device` (`{"device": "hw:...",
  "output_channels"?: [l, r]}`, empty `device` for the default playback
  device; see "Output channels"), next to `set_eq`, `set_upsampler`,
  `set_dsp_enabled` and the others. `set_config` stays for whole-file
  replacement (`avdsp ctl config set`), unconditional.

  ```json
  {"id":1,"method":"set_output","params":{"bits":24,"dither":"tpdf","headroom_db":0}}
  {"id":2,"method":"set_formats","params":["s32","s24"]}
  {"id":3,"method":"set_rate","params":{"follow_source":true,"allowed_rates":[44100,48000],"default_rate":48000,"conflict":"keep_current","bypass_resampler":"engine"}}
  {"id":4,"method":"set_device","params":{"device":"hw:CARD=DAC,DEV=0"}}
  {"id":5,"method":"set_device","params":{"device":"hw:CARD=USB,DEV=0","output_channels":[2,3]}}
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
- `conflict` (while `follow_source` is on): what to do when apps playing
  into Audivane DSP at once want different rates. `keep_current` (default)
  keeps the
  current rate while any of them still plays at it, else follows the app
  that started first; `highest` takes the highest native rate;
  `default_rate` uses `default_rate` while they disagree (a single app is
  still followed). The others are resampled by PipeWire; apps on Audivane
  Direct always decide. The rule and its timing are in
  [architecture.md](architecture.md#sample-rate-and-clock).
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
  the native rates among them, widened to the device's native rates when
  none of them is native, and, with `bypass_resampler: "engine"`, the rates
  the engine converts to a native one (below). A Direct stream at another
  rate (e.g. 48 kHz on a 96 kHz-only DAC whose 48 kHz graph the DSP sink
  plays ×2, with `"pipewire"`) moves the graph to a Direct rate (the
  stream's rate's next higher Direct rate, else
  the closest; with `follow_source` off, the nearest Direct rate to
  `default_rate` when that is not one) and PipeWire resamples the stream:
  playback continues, and `passthrough` reports `resampled` (not
  bit-perfect). When no effective rate is native and none can be converted
  (`"pipewire"`, or a pair the engine cannot build), Direct cannot play on
  the device: routing a stream to it puts the engine in `error` (`Audivane
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
  widened to the device's native rates when no configured one is), and the
  rates the engine converts are added on top
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
  to 20 kHz at every rate, 0.908 of the Nyquist where 20 kHz does not fit,
  and stopped from 1.00 of the lower Nyquist, so 20 kHz is converted at
  the output rate when decimating; automatic length matched to the
  output's resolution, linear phase), fixed, one pipeline
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
  length; a convolution it does not take over warms up for its ring too:
  "Edits with a convolution loaded"), then they crossfade, or fade through silence when their delays
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
  ×1 are an exact copy. The converted path reports
  `upsampling` or `rate_conversion` and `dsp_active` in `passthrough` (its
  chain resamples and dithers, at unity volume too), and a stream PipeWire
  still resamples (a second app at another rate) keeps `resampled`.

#### Opens and rate moves

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
deferred: saving a preset is an explicit user action and is written
synchronously before the reply.

## Presets

`control::presets` stores parts of chain A's signal path in `presets/` **next
to `config.json`**, so `--config PATH` moves the config and the presets
together. Each page of the app has its own presets, holding that page's part
only; global presets hold any set of parts:

```text
$XDG_CONFIG_HOME/audivane-dsp/presets/hd-650.json        global, every part
{ "version": 1, "name": "HD 650", "chain": { "eq": { "...": "..." }, "...": "..." } }
$XDG_CONFIG_HOME/audivane-dsp/presets/desk.json          global, two parts
{ "version": 1, "name": "Desk", "parts": ["eq", "upsampling"], "chain": { "eq": {}, "upsampler": {} } }
$XDG_CONFIG_HOME/audivane-dsp/presets/eq/warm.json       the EQ page's
{ "version": 1, "name": "Warm", "part": "eq", "chain": { "eq": { "...": "..." } } }
```

Parts (`PresetPart`), one per page, except that the output-side pages
(DC block, pitch, limiter, output) share one:

| part | chain |
| --- | --- |
| `stereo` | Stereo image: `crossfeed` (crossfeed and width) |
| `eq` | `eq` (bands, L/R sections, preamp, tilt and tone) |
| `convolution` | `convolution` |
| `upsampling` | `upsampler` |
| `harmonics` | `harmonics` |
| `output` | `dc_block`, `pitch` (varispeed is a machine setting), `limiter` and `output` (headroom, bits, dither) |

- One file per preset, written atomically (same temp file + fsync + rename as
  `config.json`) and synchronously. A file holds only its parts' chain keys;
  the keys it leaves out read as defaults and are never applied.
- A global preset without `parts` holds every part, so every older file
  loads that way. Saving with every part
  ticked writes no `parts`. A page's preset has `part`; its directory
  (`presets/<part>/`) is authoritative. Unreadable files are logged and
  skipped.
- Names are trimmed, 1–64 characters, no control characters. The file name is
  a slug (lowercase ascii, digits, `-`); the `name` field is authoritative, so
  two names with the same slug (`Rock` / `rock`) get distinct files. Names
  are unique per directory: a global preset and an EQ preset may share one.
- Loading applies the preset's parts to chain A and leaves the others exactly
  as they are. Device, formats, routing, rate policy, varispeed, A/B and the
  active names are machine settings. The room calibration's convolution item
  stays through a preset load and is never saved into a preset ("Room
  calibration bundles").
- `Status.preset = { name, modified }` reports the active global preset and
  `Status.part_presets = [{ part, name, modified }]` (left out when empty)
  each page's; `modified` is chain A ≠ the saved preset **on the parts it
  holds**. The service caches the active presets, so status never reads the
  preset directory. Loading a global preset clears the pages' active names
  of the parts it holds (they no longer play); the other pages keep theirs.

Every preset call takes an optional `part`: absent = the global presets.

| call | reply | notes |
| --- | --- | --- |
| `list_presets` | `Vec<PresetInfo { name, part?, parts, modified_unix, bands, convolution }>` | every preset: globals first, then each part's in `PresetPart` order, each sorted by name; `bands`/`convolution` count only parts held |
| `get_preset { name, part? }` | `Preset` | `INVALID_PARAMS` when unknown |
| `save_preset { name, overwrite, part?, parts? }` | `PresetInfo` | saves chain A (with `part`, that part; else `parts`, default every part), then makes it active; `BAD_STATE` when it exists and `overwrite` is false; an empty `parts` is `INVALID_PARAMS` |
| `load_preset { name, part? }` | `null` | validates, applies its parts to chain A through the normal edit path (chain B is still realigned) and makes it active; a global one clears `active_part_presets` of the parts it holds |
| `blank_preset { part }` | `null` | resets that part of chain A to its defaults (the EQ to an empty profile, on: no bands, preamp 0, no tilt or tone) through the normal edit path (the room calibration's convolution item stays) and clears the part's active name; the saved presets are untouched (the app's "New (blank)") |
| `delete_preset { name, part? }` | `null` | clears the active name when it was the active one |
| `rename_preset { from, to, part? }` | `PresetInfo` | follows the active name; `BAD_STATE` on a name collision |
| `import_apo { name, text }` | `PresetInfo` | Equalizer APO / AutoEQ text parsed by `dsp-core`'s `eq_apo` into an **EQ preset** (`part: eq`), which touches nothing else when loaded; it is **not** loaded (an existing name is replaced) |
| `list_devices` | `Vec<DeviceInfo>` | see below |
| `assign_devices { assignments: [{output, device}] }` | `null` | says which card each output is when identical DACs moved ports (see [Identical DACs](#identical-dacs)); `INVALID_PARAMS` for a device that is not a connected USB card or an output that is not `master` or a follower |

`avdsp ctl preset [--part P] list|show|save [--overwrite] [--parts
P,P…]|load|blank|delete|rename|import FILE [--name N]` covers all of them
(`blank` needs `--part`; `list`
shows the global presets, or with `--part` that part's; `import` always
writes an EQ preset).

A/B: the app's side B from a preset is chain A with the preset's parts
applied (a full chain, `AbB::Chain`), so a partial preset compares only what
it holds.

## Devices

`list_devices` (and `avdsp ctl devices`) enumerate **playback** devices from
`/proc/asound/pcm`, with card ids from `/proc/asound/cards`; no device is ever
opened to enumerate. Each entry is
`DeviceInfo { id: "hw:CARD=<card-id>,DEV=<n>", card, name, hdmi, loopback,
active, channels, suffix?, identity? }`. `active` marks the configured device (`hw:0,0`, `hw:CARD=DAC`,
`hw:CARD=DAC,DEV=0` and the short forms are all matched); capture-only PCMs are
skipped. `channels` lists the playback channel counts the device offers,
ascending, where `/proc` says so without opening it (a USB audio device's
`/proc/asound/card<N>/stream<M>`, its `Playback:` section); empty when
unknown. The open device's own range is `status.device_caps`. `identity` is a USB card's [`DeviceIdentity`](#identical-dacs);
`suffix` tells a card apart from another of the same model (same USB
vendor:product): `serial <s>` when it has a serial number, else `USB port
<chain>` with the hubs the cards share left out (ports `1.4.3` and
`1.4.1.4` read `USB port 3` and `USB port 1.4`). Pickers show it after the
name: "USB Audio (USB port 3)".

### Output channels

The engine plays two channels. A playback device with more (an audio
interface that plays 8 channels only, an HDMI output) opens at the
**smallest channel count it takes that holds the output pair and plays**:
2 when it has a stereo mode and the pair is the first two, else the next
count it offers. Channel count comes before format and ratio: the probe
records every count the device takes (from 2 up), and an open tries the
narrowest count that holds the pair with the configured formats and the
whole ratio ladder before a wider one, also for a follower (which opens
without a probe). So a device with a 2-channel S16 mode and an 8-channel
S32 mode plays the default pair as stereo S16, as it always did, and pair
3/4 on 8 channels; with S32 alone configured, a 4-channel S16 mode is
skipped for an 8-channel S32 one. The engine's left and right land on the pair, every other channel
plays **exact zeros** (the samples are interleaved into the wider frame on
the audio thread, in a buffer sized at the open: no allocation). A device
that offers 2 channels with the default pair is opened and packed exactly
as a stereo device (bit for bit). A mono device is refused:
`device '<d>' is mono (1 channel): the engine plays 2 channels and needs a
device with at least 2 (...)`.

- `output_channels: [l, r]` (config, master; left out: `[0, 1]`): the
  device channels, from 0, the left and right play on, two different ones
  below 64. `[0, 1]` is stored as no setting. It belongs to the device:
  `set_device` with another device and no `output_channels` goes back to the
  first two; with the same device and no `output_channels` it is kept.
- `outputs.followers[].output_channels` (same shape): a follower's own pair,
  set with `set_follower` (`[0, 1]` resets it).
- Changing a pair reopens that device: a reopen of the master, of the
  follower only for a follower. An edit is checked against the count its
  own pair opens at (an explicit upsampler rate the stereo mode plays stays
  allowed when going back to the first two from a wider pair whose mode
  lacks it); `status.device_caps` shows the count and rates of the pair in
  use.
- **Refused:** a pair naming one channel twice or a channel ≥ 64, at any
  edit; a pair past the device's channels, at the edit when the device's
  count is known (`status.device_caps.channels_max` for the master, the
  follower's `device_channels_max` once it has opened:
  `output_channels [8, 9]: device '<d>' has 8 channels (0 to 7)`), else at
  the open, as an open error (`... does not support 10 or more channels
  (output channels [8, 9]) ...`, the master's `status.error`, the
  follower's `error`, state `lost`).
- **Reported:** `status.device_caps.channels` is the count the master opens
  at (`channels_min`/`channels_max` what it takes); a follower's status has
  `output_channels` (left out: the first two), `device_channels` (the count
  it opened at) and `device_channels_max` (0 while closed).
- **Measurement and calibration are in the engine's channels.** A
  `measure_play` route's `channel` is the engine's channel (0 left, 1
  right): it plays on `output_channels[channel]` of that device. A bundle's
  `matrix[].out` (and a convolution `MatrixEntry.out`) is the chain's output
  channel, so it lands on that device's pair the same way; nothing in a
  bundle names device channels.
- CLI: `avdsp ctl device [<device>] [--output-channels L,R]` shows or sets
  the master's device and pair; `avdsp ctl follower <device>
  --output-channels L,R` a follower's; `avdsp ctl devices` lists the
  channel counts.

Card numbers depend on boot order, so the daemon names devices by card id:
the device it picks on first run is `hw:CARD=<id>,DEV=<n>`, and a numeric
`hw:<card>,<dev>[,<sub>]` in the config is rewritten to the card-id form
(and saved) at start-up while that device exists.

### Identical DACs

Two DACs of one model without USB serial numbers get card ids in probe
order (`USB`, `USB_1`), and a replug or a reboot can swap them. So the
master and each follower also record the USB device they are:
`device_identity` next to `device`, and `identity` in each
`outputs.followers[]` entry (USB cards only; set by the daemon, a client's
value is ignored):

```json
"device": "hw:CARD=USB,DEV=0",
"device_identity": {"card": "USB", "usb_id": "2522:0007",
                    "port": "pci-0000:00:14.0-usb-0:1.4.3"}
```

`usb_id` is `/proc/asound/cardN/usbid`; `serial` the USB serial when there
is one; `port` the controller and the bus-relative port chain, as
`/dev/snd/by-path` names it without the interface (bus and device numbers
change on a replug; these do not). At start-up (before the engine opens
anything), whenever the cards change (looked at twice a second) and after
any edit of the outputs' devices, the daemon finds each identity among the
cards present:

- the only card of its model, for the only output of that model: that card,
  whatever its id (as it was before identities);
- otherwise by serial, else by port; one output left over with one card of
  its model left over: that card (it moved ports, its `port` is updated).

An output found on a card with another id is renamed to it
(`hw:CARD=USB,DEV=0` → `hw:CARD=USB_1,DEV=0`, device numbers kept), the
change logged, applied and saved; a loaded calibration's follower parts
follow their follower. An unplugged output whose id another card took
waits under the id ALSA gives the next card of its model. Each output's
card id is pinned to its device while the engine looks for it, so a name
that now belongs to the other DAC is never opened in between.

When identical DACs without serials both moved ports (more outputs or
cards left over than one each), nothing is guessed: those outputs stay
closed (waiting, as for an unplugged device) and
`status.unresolved_devices` lists them, `[{output, device, identity,
candidates}]` (`output`: `master` or the follower's device; `candidates`:
the cards of its model nobody claims). The app asks "Two identical DACs
changed USB ports: pick which is which"; `assign_devices` (or `avdsp ctl
devices assign master hw:CARD=USB_1,DEV=0 hw:CARD=USB_1,DEV=0
hw:CARD=USB,DEV=0`: the master is `USB_1`, the follower configured as
`USB_1` is `USB`) says it, all outputs at once so two can swap, each
taking the identity of the card it now names. Picking a device for an
output (`set_device`, `set_follower`, `set_config`) records that card's
identity the same way.

A config from before identities gains them at its first start, from the
cards its ids name then (logged). A calibration bundle's follower `device`
resolves to the follower configured under that id when it is loaded, and
follows that follower's renames after.

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

Each stage below (DC block, pitch, stereo image, EQ, convolution,
upsampling, harmonics, limiter) can be switched off or left out of the
chain. Volume, the output format and dither are always part of it.

- **Per-channel EQ (`EqProfile`):** the v2 layout is a linked section
  (`bands`, `linked_enabled`) plus optional `left` and `right`
  `ChannelEq` sections (`{ "enabled": bool, "preamp_db": f64, "bands": [...] }`),
  applied only to channel 0 and 1; channels ≥ 2 get the preamp and linked
  section only. See "EQ layout". Analytic A/B level matching uses the mean
  power response of left and right.
- **DC blocker (`ChainConfig.dc_block`, `set_dc_block`):**
  stage placed first at the source rate, before pitch and the stereo image
  stage (`null` or `enabled: false` = off; absent on `ChainConfig` by default).
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
- **Stereo image (`ChainConfig.crossfeed`, `set_crossfeed`):**
  stereo-only stage before EQ, with headphone crossfeed and a width control
  (`null` or `enabled: false` = off).
  `mode` is one of `bs2b_default` (700 Hz, 4.5 dB), `bs2b_chu_moy` (700 Hz,
  6.0 dB), `bs2b_jan_meier` (650 Hz, 9.5 dB), `bs2b_custom` (`cutoff_hz`
  300..=2000 Hz, `feed_db` 1..=15 dB), or `linkwitz` (796.4 Hz, 6.0 dB,
  mono-flat, 0.3 ms low-frequency interaural delay), or `none` (no
  crossfeed: the stage runs for its width alone). `cutoff_hz`/`feed_db`
  are only used by `bs2b_custom`. Requires 2 channels when active.
  **Stereo width:** `width` (default 1) is the side gain `k`, 0..=2, applied
  after the crossfeed sums: `d = L − R`, `L′ = L + g·d`, `R′ = R − g·d`,
  `g = (k − 1)/2`. The mid is untouched (mono passes bit-exact at every
  `k`); 0 is mono, 0.5 / 0.75 narrow, 1.25 / 1.5 wide (side +1.9 / +3.5 dB;
  a hard-panned source +1.0 / +1.9 dB, leaking into the other ear
  −18.1 / −12.0 dB inverted). `width_shape` (`StereoWidthShape`, default
  `flat`) is `flat` or `above_bass`: side gain 1 in the bass and `k` above
  `width_corner_hz` (default 700 Hz), a first-order high-pass on the side's
  change. The stage is active when `enabled` and (`mode` is not `none` or
  `width` ≠ 1); `none` with width 1 builds no stage, and width 1 with a
  crossfeed mode is the crossfeed alone, bit for bit. The width is never
  loudness-compensated: wider is louder (+0.6 dB at 1.25, +1.2 dB at 1.5 on
  correlation-0.5 pink noise; 0 dB on mono). Width changes rebuild the chain
  and crossfade like any other crossfeed edit.
  The stage's constant gain, computed when the chain is built, keeps the
  same A-weighted loudness with crossfeed on and off, for stereo pink noise
  with inter-channel correlation 0.5 (the A/B analytic level match's
  reference, A-weighted). The gain is within ±0.5 dB of unity (−0.48 to
  +0.21 dB); worst-case signals can then go up to +1.4..+5.6 dB over the
  input's peak (+4.3 dB for `bs2b_default`). Nothing cuts the stage for
  safety: such a peak is caught by the limiter (`limiter.mode` `limit` or
  `auto_headroom`) or by enough digital attenuation before the quantizer
  (digital volume, EQ preamp, `output.headroom_db`), else it clips at the
  output. Hardware volume gives no headroom: it acts after the DAC input.
  The match covers the crossfeed only (1 with `none`); the width's
  worst case comes on top: `max(1, k)` for flat width alone (+1.9 dB at
  1.25, +3.5 dB at 1.5), +3.4 / +5.8 dB for `above_bass`, and with
  `linkwitz` +6.2 / +7.8 dB (flat) at 48 kHz. An older config's crossfeed
  `level` key (`matched` or `peak_safe`) is dropped on load, since the
  stage always plays at its matched level (`peak_safe`'s cut is gone); the
  daemon logs it once and writes the config back.

  The EQ preamp is not adjusted for the stage. Analytic A/B level matching
  includes the stage: its stereo power response (level included) for the
  same correlation-0.5 reference, width included; the A/B shared headroom
  uses the stage's worst-case bound, width included. CLI: `avdsp ctl
  crossfeed width <K> [--shape flat|above-bass] [--corner-hz <HZ>]` (with
  the stage off it starts one with `none`), `avdsp ctl crossfeed none`;
  the preset commands keep the current width.
  Example: `{"enabled": true, "mode": "bs2b_default", "width": 1.25,
  "width_shape": "flat"}`.
- **Harmonic distortion (`ChainConfig.harmonics`, `set_harmonics`):**
  stage after the upsampler (`null` or `enabled: false` = off):
  `{ "enabled": bool, "layers": [layer…], "dc_block": bool, "routing"?:
  "parallel" | "series", "system"?: string }`. `dc_block` (default `true`)
  is a 5 Hz high-pass on the even orders' sum; `routing` (default
  `"parallel"`, omitted then) adds the layers together or runs them one
  after the other (see "Series" below); `system` is the system preset's id
  the stage was started from (omitted when built by hand; the engine
  ignores it, clients compare the layers against that system's). A layer
  with `enabled: false` is kept but not applied. Every layer, enabled or
  not, is validated; at most 8.
  Each layer is tagged by `kind` and carries `enabled` (default `true`) and
  an optional `preset`, the id of the preset it started from: compare the
  layer with that preset's `layer` (ignoring `enabled` and `preset`) to mark
  it modified, and copy the preset's layer back to reset it.
  - `"kind": "chebyshev"`: `levels_db` is H2..=H9 (8 entries, `null` = that
    order off; −120 dB is 1e-6, not off), relative to the fundamental for a
    sine at the reference point (drive 0 dB: a 0 dBFS sine). `signs` is one
    `"+"` or `"-"` per order, H2 first; default `["+", "-", "+", "+", "+",
    "+", "+", "+"]` (H3 negative: compressive). For odd orders the effect
    of the sign alternates: at low and moderate levels a negative H3 or H7
    compresses, while a negative H5 or H9 expands (the cubic terms of T5 and
    T9 have the opposite sign to those of T3 and T7). Flipping an even order
    flips which half-wave is flattened; `invert_even: true` (default false,
    omitted when false) flips every even order at once. `drive_db`
    (−40..=0, default 0) moves a 0 dBFS sine down the curve: the layer
    applies the curve's harmonics at that level relative to its fundamental
    there (H_k falls by about k − 1 dB per dB). `amount` (0..=1, default 1)
    scales the layer's terms. Levels are exact for a full-scale sine at
    drive 0 dB and amount 1, where each harmonic's sign is its phase
    relative to a cosine input; several Chebyshev layers add their signed
    amplitudes order by order.
    Optional `tilt` (omitted or `null`: flat) makes the levels change with
    frequency: `{ "low": { "hz", "db", "slope"? }, "high": { "hz", "db",
    "slope"? }, "low_even"?, "orders"? }`. Each order's distortion term
    goes through its own filter `G_k`: a low shelf (rising below `low.hz`
    until it reaches `low.db`) and a high shelf (rising above `high.hz`
    until `high.db`; negative `db` cuts), with the corners given as
    fundamental frequencies (order k's filter has them at `k × hz`) and the
    filter scaled to unity at `k × 1 kHz`. Each shelf's `slope` is 6, 12,
    18 or 24 dB/oct (a Butterworth-style shelf of order 1–4; omitted: 12
    for the low shelf, 6 for the high one; any other value is rejected). The slope is the
    asymptote: the order-n shelf is the 6 dB/oct curve squeezed n times
    toward its corner, so its steepest slope is `slope × (R − 1)/(R + 1)`
    with `R = 10^{|db|/20}` (0.88 × at ±24 dB, 0.60 × at ±12, 0.33 × at
    ±6): a small rise never gets that steep, it rises over fewer octaves.
    Overrides in `orders` carry their own `slope` the same way. So harmonic k of a 0 dBFS
    sine at f is `h_k + 20 log10 |G_k(k f)|` dBc relative to the input
    level, every order follows the same curve against the fundamental's
    frequency, and the levels hold at 1 kHz. The low shelf applies to odd
    orders only unless `low_even: true` (default false, omitted when
    false); the high shelf to every order. `orders` (8 entries, H2 first,
    `null` = the layer's shelves; omitted when all `null`) overrides both
    shelves of one order (its low shelf applies whatever its parity). The
    terms carry no linear part, so small signals keep a flat response; but
    an odd order's term also carries fundamental-frequency content (the
    curve's compression), which its filter shapes too, so a full-scale
    fundamental's level changes a little with frequency (`gain_db` in the
    readouts; the level change counts it). A tilt with every shelf at 0 dB
    is the same as none (and does not mark a preset layer modified).
    Optional `sag` and `bias` (omitted, `null` or depth 0: off; a depth of
    0 does not mark a preset layer modified) move the curve's operating
    point with the music. Both depths scale with the layer's `amount`
    (applied at `depth_db × amount` and `depth × amount`; amount 0 is off). `sag: { depth_db, attack_ms, release_ms }`
    (depth 0..=6 dB, times 0.1..=10000 ms): an envelope of `|x|` lowers the
    drive in front of the curve by up to `depth_db` at full scale, not
    renormalised, so loud passages are gently compressed (by up to the
    depth) and lean toward the low orders. `bias: { depth, attack_ms,
    release_ms }` (depth −0.25..=0.25 of full scale, the sign picks the
    side; times 0.1..=10000 ms, usually slower than the sag): an envelope
    moves the operating point to `depth × envelope` and the curve is read
    around it (its value and slope there removed), so sustained loud
    passages shift the even/odd balance and never the level. The bias
    reach is cut where it would raise the layer's peak bound more than
    1 dB over its static curve's (strong high-order curves keep very
    little; readouts list it in `bias_limited`). A layer with sag or bias
    runs on its own (its own envelopes and DC blocker per channel) instead
    of merging into the stage's static table; at depth 0 it is the static
    layer bit for bit. The stage's sag layers share one supply: each curve
    sees its own drive (its harmonic balance moves under load), but the dry
    signal and their linear parts are turned down once, by the deepest sag
    at that sample, never summed (stacked sag layers turn the level down as
    much as the deepest one alone; static layers' linear parts are not on
    the supply). A tilt on the same layer applies under load too: each
    order keeps its own filter (its corners, the low shelf on odd orders
    only unless `low_even`, its override), applied to that order's
    distortion around the moving operating point (its DC through its own
    5 Hz blocker); at rest the layer is the static tilted layer.
  - `"kind": "flux"` (transformer or tape core saturation): a low shelf
    (`corner_hz`, `max_boost_db`), an odd `tanh` saturator with first-order
    anti-aliasing, and the exact inverse shelf, so small signals pass flat
    and H3 rises about 12 dB/oct below the corner and 2 dB per dB of level.
    Calibrated from `target_h3_db`: H3 in dBc for a 0 dBFS sine at
    `calibration_hz`, solved at the output rate when the chain builds. No DC,
    no linear part.
  - `"kind": "hysteresis"` (tape magnetic hysteresis, Jiles-Atherton ODE
    integrated with RK4): distortion-only path `amount · P⁻¹ HPF₅Hz(u − z)`
    (`z = P x_c` the core's input, `u` its normalized magnetization) with
    `saturation` (0..=1, Rayleigh pinning vs Langevin knee), `width`
    (0..=1, magnetic loop width), `amount` (0..=1, default 1), `target_h3_db`
    (−120..=−12 dBc for a 0 dBFS sine at `calibration_hz` at `amount = 1`),
    and `calibration_hz` (20..=5000 Hz; above 0.16 × the output rate it is
    calibrated at 0.16 × the rate). Optional, absent = off:
    `bass_eq: {enabled, hz, db}`, a record/playback pair (a 12 dB/oct low
    shelf `P`, `db` of boost into the core below `hz`, unity at 1 kHz, and
    its exact inverse on the residual: only the distortion rises toward the
    bass); `asymmetry`, a DC offset of the core's field in units of its
    saturation knee, whatever the drive (the core starts at that rest
    point, so there is no DC step), which adds H2 and the other even
    orders. The drive is calibrated on the symmetric core, then the offset
    goes on. Small signals pass flat;
    the 5 Hz high-pass on the residual removes remanent and even-order DC in
    silence. Not band-limited: aliases into the audio band below 4×
    (176.4 kHz); readouts list it in `below_clean_rate` and `avdsp ctl
    harmonics show` warns.

  The stage plays at its natural gain: nothing scales it down to stay
  within full scale. Its certified peak bound `B` (the largest output for
  any input within full scale, every layer summed) is reported as
  `peak_gain_db` (`20·log10 B`) and counted by the A/B shared headroom;
  peaks over full scale are the limiter's and the output headroom's to
  catch. The bound is the Chebyshev part's
  certified bound (with its direct gain lowered by half of each flux
  layer's linear split `λ` and the hysteresis layers' direct-path splits
  `θ_i`) plus each flux layer's `λ/2` and certified residual and each
  hysteresis layer's bound (its path `p` and output `x + p`, probed through
  the whole layer when the chain builds, worst case included, and held
  there by the runtime for every input: the cheaper point of the segment
  between `|p| <= Y` and `|x + p| <= Q`); a sag/bias
  layer adds the cheaper point of a segment between its own certified
  bound (taking the dry signal) and its remainder with the direct gain
  left on the Chebyshev part, valid for every envelope value (a tilted
  sag/bias layer counts each filtered order's range over every reachable
  operating point through its filter, as a static filtered term does at
  rest; with sag alone its bound is the static tilted layer's per-term
  bound below). With tilt, the Chebyshev part (filtered terms, one per
  order and filter, plus the unfiltered ones) is bounded jointly, lag by
  lag: every term is `D_k(x)` through a linear filter (its tilt filter,
  then the DC blocker for an even term), so the output is a sum over lags
  `m` of polynomials `F_m(x[n − m])` whose coefficients are the impulse
  responses at that lag, and the worst case over all inputs within full
  scale is the sum of each `F_m`'s own maximum on `[−1, 1]` (certified on a
  grid with a curvature margin, the far lags by the certified L1 norms'
  rest). The per-term bound (each filtered term at `|a_k| ‖g_k‖₁ max|D_k|`
  on its own) is kept, and the smaller of the two counts; orders whose
  filter is unity stay in the certified grid, so without tilt the bound is
  unchanged. Strong tilts on loud levels raise it a lot (their true
  worst case). Input past full scale (inter-sample overs) passes at unity
  slope beyond the curve's ends.
  **Series** (`"routing": "series"`, two or more active layers:
  enabled, amount above 0; with one it is the parallel stage bit for bit):
  the active layers run in list order, each feeding the next, `y =
  L_n(…L_1(x))`, for heavier settings where the cross terms matter (a
  preamp into a tape machine). Each layer is its own device: calibrated as
  if alone at full-scale input, with its own bound `B_j`, its own sag/bias
  supply (nothing shared across layers) and its own DC blockers at its
  output. Each layer feeds the next at its natural level, so a layer after
  one that adds a lot sees more than full scale on worst-case inputs or
  loud passages: past full scale every layer shapes the clamped input and
  passes the excess at unity slope (unshaped), so for an input within `±R`
  (`R >= 1`) its output is within `B_j + R − 1`. The stage's peak bound is
  that range carried through the chain (`R_1 = B_1`, `R_j = B_j +
  max(R_{j−1}, 1) − 1`). A series config is valid wherever its layers are.
  Order matters only in series. Known limit: on music near 0 dBFS a
  series chain crosses full scale between layers on a small share of
  samples (0.02–0.1 % measured), which later layers pass unshaped and the
  limiter catches. The older routing `series_makeup` (series with output
  trims on the last layer) reads as `series` in every reader (daemon,
  `avdsp ctl config set`, `avdsp process --config`). A series stage's
  readouts are simulated through the whole stage (not separable, so editing
  any layer simulates it again); cross terms reach orders above H9, which
  the readouts and the aliasing warning do not list. The stage changes
  level (shown by `avdsp ctl harmonics show` and `harmonics_readout`); A/B
  level matching counts its loudness change. Order k of a Chebyshev layer
  folds back into the audio band when
  `k × 20 kHz > rate − 20 kHz` at the DAC rate (all orders at 1×, H4 and
  up at 96 kHz, H8/H9 at 176.4 kHz, H9 at 192 kHz). Such orders are still
  applied; `avdsp ctl harmonics show` prints a warning line naming them,
  and warns when a layer runs below its clean rate (`below_clean_rate`).
  Example: `{"enabled": true, "dc_block": true, "layers": [{"kind":
  "chebyshev", "preset": "triode", "levels_db": [-40, -58, -72, null, null,
  null, null, null], "signs": ["+", "-", "+", "+", "+", "+", "+", "+"],
  "drive_db": 0, "amount": 1}, {"kind": "flux", "corner_hz": 100,
  "max_boost_db": 12, "target_h3_db": -24, "calibration_hz": 20}]}`.
  A harmonics stage added without a config starts with one
  `se_triode_high_mu` layer.

  `harmonics_presets` → `Vec<HarmonicsPreset { id, name, origin,
  reference, source, layer }>`: the built-in presets (data in
  `crates/protocol/src/harmonics_presets.json`). `origin` is `hand_picked`
  (`triode`, `pentode`: the stage's original characters), `computed`
  (device equations at a stated operating point, from
  `examples/harmonics_presetgen.rs`) or `archetype`. `reference` says what
  0 dBFS at drive 0 dB stands for; `layer` has no `preset` field (set it
  to `id` when adding the layer).

  `harmonics_systems` → `Vec<HarmonicsSystem { id, name, gear, sound,
  routing?, layers }>`: the built-in system presets, whole stages that
  stand for a typical piece of gear (data in
  `crates/protocol/src/harmonics_systems.json`; generic archetypes, never
  products). `gear` says what the chain is, `sound` what it sounds like;
  `routing` is left out for parallel. Each layer is an ordinary layer with
  `preset` set to the layer preset it started from, some with settings
  changed (a lower drive, sag, a different H3 target). Loading one is
  `set_harmonics` with `{enabled, dc_block: true, layers, routing, system:
  id}`; the app asks before replacing a stage that has layers, compares each
  layer the system holds against the system's version of it (a freshly
  loaded system shows nothing modified, "Reset to system" restores it) and
  layers added by hand against their layer preset.

  `harmonics_readout { config?, sweep?, sweep_freq_hz?, sweep_level_dbfs? }` → `HarmonicsReadout { rate_hz,
  level_change_db, peak_gain_db, small_signal_gain_db, aliasing,
  below_clean_rate?, bias_limited?, points, sweep?, freq_sweep?,
  under_load? }`: readouts of `config` (validated; default chain A's stage,
  off when it has none) at the current output rate (else 96 kHz).
  `level_change_db` is the loudness change on music that A/B matching
  counts (simulated through the stage at 48 kHz, sag and bias envelopes
  settled on the music); `peak_gain_db` the certified worst-case peak gain
  for inputs within full scale (0 or more; nothing cuts it, see the bound
  above); `aliasing` is `{ tone_hz, level_db, worst_hz, worst_db, warn }`, the
  aliasing measured at the output rate: the stage's steady-state output for
  a 0 dBFS sine at `tone_hz` (just below 20 kHz, coherent with the
  measuring window), every partial landing in 20 Hz..=20 kHz other than
  the tone counted (the tone's own harmonics all lie above 20 kHz, so all
  of it is aliasing: every order, series cross terms, flux and tape layers
  included). `level_db` is their power sum and `worst_db` the loudest, both
  in dB relative to the tone's fundamental out (−200 = none), `worst_hz`
  where the loudest lands (0 when none), and `warn` is `level_db > −60` (the probe is far hotter than music at
  20 kHz; below that the level is reported without a warning).
  A parallel stage of Chebyshev layers only is computed in closed form
  (exact: its curve has degree 9 at most); any other stage is simulated
  at the output rate (tape layers and the sag/bias supply each on their own
  and cached, in parallel routing);
  `below_clean_rate` (omitted when empty) lists
  `{ layer, clean_rate_hz }` for each enabled, non-zero layer that is not
  band-limited and runs below the rate it needs (tape hysteresis: 176.4 kHz,
  4×; `layer` indexes `layers` from 0); `bias_limited` (omitted when empty)
  lists `{ layer, depth, reach }` for each layer whose bias reach the
  bound cut short of its depth (`depth` as applied, times the amount); `small_signal_gain_db` counts sag/bias
  layers with their envelopes at rest. `points`, `sweep` and `freq_sweep`
  are the curves at rest (sag and bias off: what a
  quiet passage or the onset of a loud one hears). `points` holds sines at 1 kHz and 40 Hz at 0
  and −18 dBFS, each `{ input_dbfs, freq_hz, thd_db, gedlee_gm,
  harmonics_db, aliased?, gain_db }` (`harmonics_db` is H2..=H15, 14
  entries, in dB relative to the fundamental, −200 = none; `thd_db` and
  `gedlee_gm` count the same orders; `gain_db` the fundamental out over
  in). Orders above Nyquist at the output rate are not harmonics of the
  tone once folded: their `harmonics_db` entry is −200, they are left out of
  `thd_db` and `gedlee_gm`, and `aliased` (omitted when empty) lists each
  one that is present as `{ order, freq_hz, level_db }`, `freq_hz` where it
  lands after folding. When a fold lands on another harmonic's frequency
  the closed form keeps the two apart; a simulated tone can mix them.
  Without a hysteresis layer on they are the stage's steady state in
  closed form at the output rate (each term's harmonics through its filter
  and the DC blocker, each flux layer's periodic response; it matches
  simulating the tone through the stage); with one, they are simulated
  through the stage in steady state at the output rate or 96 kHz,
  whichever is lower (gain and `aliasing` use the output
  rate; a harmonic above 48 kHz but below the output Nyquist is still
  measured at its order). With `sweep: true` (default false) the
  result also has `sweep`: the same points for 1 kHz and then 40 Hz, each at
  −40..=0 dBFS in 2 dB steps (42 points), for plotting the harmonics
  against level, and `freq_sweep`: 0 dBFS and then −18 dBFS, each at the
  31 third-octave frequencies 20 Hz..=20 kHz (62 points; with a hysteresis
  layer on, every third of them, octave steps, 22 points), for plotting
  them against frequency (both left out otherwise). With `sweep`,
  `sweep_freq_hz` (10..=20000) runs the level sweep (and `under_load`) at
  that one tone instead (21 points), and `sweep_level_dbfs` (−120..=0)
  `freq_sweep` at that one level (31 points, or the thinned set); `points`
  stay at the fixed tones, and the thinning is the default sweeps', so a
  moved sweep costs at most what the default one does (cached per tone:
  about 10 ms for Chebyshev layers, up to about 0.1 s for a tape layer or a
  series chain on a tone not seen yet). Both are ignored without `sweep`.
  With `sweep` and a
  sag/bias layer on, `under_load` (left out otherwise) is the level sweep
  again (same tones; every other level when the readout thins out for slow
  layers) simulated through the stage at the output rate or 96 kHz with
  each tone's envelopes settled on the steady sine (presettled to their
  periodic steady state, so 10 s envelopes cost nothing extra): the drive
  sag's compression in `gain_db` and its effect on the levels.
  In series every tone is simulated through the whole stage (at the output
  rate or 96 kHz, each layer at the output rate, its
  envelopes settled on its own input), every point counted as a simulated
  readout's (the sweeps thin out with the layers' cost, every static layer
  counted at 0.2 tape layers; the level sweep keeps its 2 dB steps, the
  tones shorten and the frequencies thin out instead), and `level_change_db` likewise runs the
  music through the whole stage.
  `gedlee_gm` is the GedLee metric
  `(∫₋₁¹ (cos²(πx/2) c''(x))² dx)^½` of the static curve `c` with the
  point's harmonic levels (exact for Chebyshev layers). Runs on the control
  thread without the mutation lock; nothing changes.

  `harmonics_spectrum { config?, freq_hz, level_dbfs, traces? }` →
  `HarmonicsSpectrum { rate_hz, freq_hz, level_dbfs, bin_hz, word, freqs_hz,
  lines }`: the distortion spectrum a measurement of the system would show
  for a sine at `freq_hz` (10 Hz to 0.45 × the rate) and `level_dbfs`
  (−120..=0) through `config` (validated; default chain A's stage, off when
  it has none), simulated in steady state at the current output rate (else
  96 kHz) with nothing band-limited, so every harmonic and alias lands where
  it lands. The tone is snapped to an odd bin of a power-of-two window
  (`freq_hz` in the reply is the one run; `bin_hz` the bin width, about a
  sixteenth of the tone, 1–12 Hz); every bin then holds exactly one order.
  `traces` (default `[{kind: "stage"}]`, at most 16) picks the lines, in
  order: `{kind: "stage"}` the whole stage, `{kind: "layer", layer}` one
  layer alone (applied even when disabled) fed the clean tone, `{kind: "after", layer}` in series routing only the chain up to and
  including that layer (an error in parallel). Each line is `{ trace,
  gain_db, thd_db, thd_n_db, floor_db, windowed?, levels_db, peaks, wave }`:
  `gain_db` the fundamental out over in; `thd_db` every harmonic below
  Nyquist (any order) in dB relative to the fundamental (−200 = none);
  `levels_db` the spectrum at `freqs_hz` (2048 points log spaced from 10 Hz
  to Nyquist, shared by the lines), peak held over each point's span, dB
  relative to the fundamental (0 dBr; −250 floor). It is unwindowed (exact:
  the steady state repeats over the window, so each order is one bin, on
  the one point whose span holds it, and points whose span holds no bin sit
  at the floor; DC stays in bin 0, off the plot) unless `windowed: true`
  (omitted when false): the window did not repeat closely enough (the
  bound on its leakage came within 10 dB of `floor_db`, e.g. an envelope
  still moving), and the spectrum is taken through a 7-term
  Blackman-Harris window instead, interpolated in dB between bins. Without
  a tape layer the stage settles 1.5 s longer than its own settle so its
  DC blockers and envelopes repeat;
  `peaks` the orders H2..=H64 above −200 dB as `{ order, freq_hz, level_db,
  aliased }` (`freq_hz` where it lands, folded when `aliased`); `wave` one
  cycle of the output (512 samples, the input being `10^(level_dbfs/20) ·
  cos(2π i / 512)`). `word` is `{ bits, dither, headroom_db }`, the output
  word the engine plays (`chain.output`, bits capped at the device format;
  volume at 0 dB assumed): `floor_db` is its rounding plus dither noise
  (unshaped variance, flat) per FFT bin of the plotted spectrum (unwindowed
  or windowed) relative to
  the fundamental after the headroom, and `thd_n_db` counts everything but
  the fundamental in 20 Hz..=20 kHz (harmonics and aliases) plus that noise
  over the band (an upper bound for the noise-shaped kinds). Each line is
  cached by its layers' parameters, the rate and the tone; runs on the
  control thread without the mutation lock, on up to four threads; nothing
  changes.

  `harmonics_live` → `HarmonicsLive { state, seq, rate, window_ms,
  interval_ms, f_min_hz, bins_per_octave, music, added, added_db,
  added_pct, added_peak_db, level_change_db, linear_gain_db, music_rms_dbfs,
  music_level_dbfs, music_peak_dbfs, centroid_hz, layers, split,
  split_note?, parts, analysis_ms }`: what the harmonics stage of the
  side playing (A, or B during an A/B comparison; when only the side not
  heard has a stage, that side's, since both sides run) does to the music
  playing now, the latest analysis frame (cheap, latest wins: clients poll it a few
  times a second; compare `seq` to see a new one). Each block taps one side
  only, and a side switch restarts the analysis window, so no frame mixes
  the two. Each call keeps the analysis on for 3 s
  (`HARMONICS_LIVE_HOLD_MS`): while nobody calls, the audio thread copies
  nothing and the analysis thread sleeps. The first call after a pause
  answers `state: "waiting"`; nothing queued before it is used (each
  queued block's header carries where its frames start in the sample
  stream, and the analyser resyncs on any mismatch). `state` is `waiting`
  (gathering the first window), `idle` (no audio reaches the stage:
  nothing playing, DSP off, bypass), `off` (neither side has an active
  stage), `silent` (music below −90 dBFS: levels only), `hidden` (a blind
  test runs, `ab_blind`: a live readout would give the side away, so the
  engine is not asked and nothing is analysed; setting or clearing the
  mask also drops what the analysis holds, its window, latest frame and
  `seq`, so the first call after a blind test starts afresh from
  `waiting` with `seq` 0) or `playing`. The tap is the
  stage alone, the mono sum (channel mean) of its input and output at the
  output rate, before the output headroom, volume and limiter, plus the
  input of channels 0 and 1 for the split. Every `interval_ms` (200 ms,
  doubled while one analysis takes over a quarter of it, up to 1.6 s; only
  the newest window counts, older audio is skipped) one Hann window of
  `window_ms` (the power of two at or below 1/8 s, 2048..=131072 samples)
  is analysed: the output `y` is split as `y = g·x + a`, `g` the
  least-squares gain of `y` on the input `x`, `a` the added content (its
  DC removed: a stage without its DC blocker passes the even terms'
  offset, which is not heard). The headline numbers cover about the last
  second of music, every frame of it (not only the windows analysed), so
  they move calmly and settle within about a second after the music
  changes: one fit of `g` over that second, from sums kept per stretch of
  1/8 window, `g` following the sag's drive gain (from the tap) and the
  music's level per window length, each when it changes. (A fit per window
  alone read the added content 0.4 to 1.1 dB low on noise-like music: too
  few bass cycles in one window to tell the H2 they make from a gain
  change.) `added_db`/`added_pct` are `a` against the
  music out (`g·x`), their power over that second; `added_peak_db` the
  loudest stretch of 1/8 window in it; `level_change_db` is `g` (its RMS
  over the music) against the stage's linear part (`linear_gain_db`: its
  small-signal gain and every order's slope; DC blockers
  and tilt filters act only on the distortion), so sag's slow turn-down
  and the curve's compression count there, not as added content (faster
  gain changes, as sag's attack and the envelope's ripple, stay in `a` as
  the sidebands they make). As power averages, a loud passage shows at
  once and counts for that second. `music` and `added` are
  spectra on a log grid (bin `k` at `f_min_hz · 2^(k / bins_per_octave)`,
  10 Hz to Nyquist, 24 per octave), band power in dB (0 dB = a full-scale
  sine; −160 floor), empty unless `playing`; a peak hold is the client's.
  Below 20 Hz a bin is narrower than the low band's FFT bins (about 1.5 Hz):
  it reads the interpolated band power, or a sine's peak level in the bin
  holding it.
  `music_rms_dbfs` (a full-scale sine reads −3.01), `music_level_dbfs`
  (RMS + 3.01: the sine level the readout sweeps use), `music_peak_dbfs`
  and `centroid_hz` (power-weighted, 20 Hz–20 kHz) describe the music.
  The spectra fit `g` over the window alone.
  `layers` is each sag/bias layer's state at the end of the window as
  `{ layer, sag_db, bias }` (the drive's turn-down in dB and the operating
  point, the deeper channel). `parts` splits `a` from the stage's static
  curve evaluated on the windows of that second split alike: each part's
  share of each window's added content, weighted by what the second's fit
  reads in that window, applied to `added_db` (its `peak_db` the window's
  peak, scaled as that fit reads the window). The curve is evaluated on
  each channel's input and averaged as the mono sum is
  (the stage acts per channel, so the split needs at most two channels,
  else `total` with a note), with the 5 Hz DC blocker on the even terms
  modelled. For `orders` the tap carries the stage's own blocker state
  at each block's end. A single even order starts from it (within about
  0.1 dB of what the stage makes, also right after a level change; the odd
  orders have no blocker and are exact). Several even orders share the
  stage's one blocker, whose state cannot be split among them (each
  order's transient after a level drop is opposite to the others' and
  cancels in the sum): each order starts in its own steady state and the
  stage's state goes to their total only, so just after a sudden level
  change in deep bass the lines are estimates while their total stays
  exact. When an even order would read more than 3 dB above that total
  over the windows (curves whose even terms cancel each other at the level
  playing), the frame shows `split: "even_odd"` instead, `even` the total and `odd` the
  odd orders' sum, with a `split_note`. For `even_odd`
  it starts in its steady state for the window repeated (the `f(−x)` half
  of the split never ran in the stage), so the split is an estimate that
  can read well off (over 10 dB) in the frames just after a sudden level
  change in deep bass, until the blocker settles (a fraction of a
  second). Each part's DC and what
  correlates with the music are removed, as for the total. Parts are
  `{ key, order?, level_db, peak_db }`: with `split: "orders"` (a parallel
  stage of static Chebyshev layers) one per order, `h2`..`h9`, each
  order's products on the music (its harmonics of every note and the
  intermodulation it makes); with `split: "even_odd"` (a series chain of
  such layers) `even` and `odd` (cross terms between layers mix the
  orders); `split: "total"` with `split_note` saying why otherwise (flux,
  tape, sag/bias and tilt have memory, so the curve alone cannot evaluate
  them).
  `analysis_ms` is the analysis thread's time for the frame. `BAD_STATE`
  without the audio engine.

  CLI: `avdsp ctl harmonics show | off | presets | preset <id> | systems |
  system <id> | triode |
  pentode | custom --h2-db -40 --h3-db -58 [--h4-db … --h9-db …]
  [--signs +-+-] [--invert-even] | flux --corner-hz 100 --boost-db 12
  --h3-db -24 --at-hz 20 | hysteresis --h3-db -28 [--saturation 0.5]
  [--width 0.35] [--amount 1] [--at-hz 1000] [--bass-eq HZ:DB]
  [--asymmetry A] | remove <n> | move <n> <to> | routing
  parallel|series`
  (`move` renumbers as `show` numbers, from 1); the Chebyshev
  commands take `[--drive-db D] [--amount A] [--tilt-low HZ:DB[:SLOPE]]
  [--tilt-high HZ:DB[:SLOPE]] [--tilt-even] [--sag DB[:ATTACK_MS:RELEASE_MS]]
  [--bias DEPTH[:ATTACK_MS:RELEASE_MS]] [--no-dc-block]` (a `--tilt-*`
  option replaces that shelf of a preset's tilt, `SLOPE` in dB/oct with
  the shelf's default when left out; `--sag` and `--bias`
  default to 20/200 ms and 300/2000 ms; `preset` also accepts `--amount`
  for hysteresis presets), `show` also notes a bias reach cut by the
  bound, and `--add` appends the
  layer instead of replacing the stage's layers. `--signs` runs from H2
  up; orders left out keep the default sign.
- **Convolution chain (`ChainConfig.convolution`, `set_convolution`):**
  `ChainConfig.convolution` is `Vec<ConvolutionConfig>`, where each item is
  `{ "name"?: String | null, "enabled": bool, "ir_path": String, "gain_db": f64,
  "matrix"?: [MatrixEntry], "calibration"?: bool }` (`calibration`: the
  loaded room calibration's item, set by the daemon and kept in place: "Room
  calibration bundles").
  Both `ChainConfig.convolution` and `Call::SetConvolution` accept:
  - `null` or missing → `[]` (convolution off)
  - a single `ConvolutionConfig` object → `[item]` (backward compatible)
  - an array of `ConvolutionConfig` objects → `[...]`
  On the worker thread, the engine resamples each enabled IR to the graph rate
  as a filter (its frequency response and DC gain kept: the taps are scaled
  by the file's rate over the graph rate, so a 48 kHz IR at 96 kHz is not
  6 dB louder), scales by `gain_db`, and convolves them via FFT into one combined IR per
  channel (mono applies to all channels; stereo maps L→L, R→R; combined tap
  count capped at 4 000 000).
- **FIR matrix (`ConvolutionConfig.matrix`):** an item can carry a filter
  matrix in place of `ir_path` (which is then `""` and may be left out):
  `out_j = Σ_i F_ji * in_i`, `i` and `j` over the chain's channels. Each
  `MatrixEntry` is `{ "in": usize, "out": usize, "ir_path": String,
  "channel"?: usize, "gain_db"?: f64 }`: the filter from input `in` to
  output `out`, channel `channel` of the WAV (0-based; left out, the file
  must be mono), scaled by its own `gain_db` (default 0) and the item's.
  A pair not listed is absent (zero); an output with no entry is silent.
  The key is left out of items without a matrix, so older configs and
  presets round-trip unchanged.

  ```json
  { "name": "Sub", "enabled": true, "gain_db": 0.0,
    "matrix": [ { "in": 0, "out": 0, "ir_path": "/irs/sub.wav", "channel": 0 },
                { "in": 1, "out": 0, "ir_path": "/irs/sub.wav", "channel": 1 } ] }
  ```

  Enabled items still compose in series (first item first): per-channel
  items alone take the path above bit for bit (a matrix with no cross term
  counts as per-channel, an absent diagonal entry as a zero filter); with
  any cross term the combined filter is the matrix product `M_k · … · M_1`
  (a per-channel item is the diagonal of its IRs), on the worker thread,
  each combined entry capped at 4 000 000 taps. It runs as one partitioned
  convolver per present entry, summed into each output. Every entry runs at
  the same base block, so the matrix has the one latency a convolution has
  (`status` and A/B alignment count it like any convolution). Routes that
  never reach an output (an entry whose input no earlier item feeds, or
  whose output no later item reads, or a diagonal channel some item has
  no entry for) are dropped before combining, in the check and the build
  alike. A/B loudness composes each output's response to each source
  channel (crossfeed direct and cross paths with the width, the EQ of
  each matrix input, the entry: `a_k = Σ_i F_ji EQ_i X_ik`) and takes it
  at the sources' matched correlation 0.5 (`Σ_k Σ_l ρ_kl a_k a_l*`): the
  entries' products `F_ji F_jm*` are interpolated between the bins of a
  grid of at most 256 Ki points (a longer filter wrapped into it, exact at
  the bins), the crossfeed and EQ evaluated at each frequency, so a long
  delay reads no null between bins. The shared headroom's peak bound sums
  a row on one grid and is certified between bins: `max_f Σ_i |EQ_i|max ·
  |F_ji(f)|` (every input a sine in the phase that adds up), each row's
  entries evaluated together at 64 points per tap of its longest entry (on
  at most 4 Mi points, about 80 MiB: fewer per tap past 64 Ki taps, 4 at
  1 Mi) and the grid maximum raised by `1 / sqrt(1 − (π d / N)² / 2)` (`d`
  the longest entry's length less one, `N` the grid: +0.005 dB at 64
  points per tap, 0.35 dB at 8, 1.6 dB at 4), which no peak between bins
  can pass (Bernstein's inequality on `|q|²`). Past 1 Mi taps each entry
  takes its L1 norm instead (a bound for any input, looser). With unequal
  EQ peaks per input, the smaller of the row's unit-weight bound times the
  largest EQ peak and the EQ-weighted sum of each entry's own bound. The
  taps are not kept for it. A per-channel convolution's `max |H|` uses the
  same bound. The
  config-only estimate (before a pair is built) takes an item's `gain_db`
  times its largest row's sum of entry gains. CPU admission prices a matrix
  with a cross term (see "DSP overload"). `avdsp process --config` checks
  an item's shape as the daemon does. CLI: `avdsp process --conv`
  and `avdsp ctl conv set|add` take `IN:OUT[:CH]=PATH` entries; a run of
  them is one matrix item (`avdsp ctl conv set 0:0:0=sub.wav 1:0:1=sub.wav`).
- **Edits with a convolution loaded:** an edit rebuilds the chain; how
  it swaps in depends on what it changed (master and followers alike).
  - *Before an unchanged convolution* (an EQ band dragged or committed,
    crossfeed, the DC block, a commit that changes nothing; the
    convolution and everything after it the same: IRs bit for bit,
    upsampler, harmonics, limiter, output; no pitch shift; a single chain,
    not an A/B pair): the new chain takes the playing one over in place
    and replaces it in the next period, with no warm-up. It moves in the
    playing chain's convolver, upsampler, harmonics, limiter and quantizer
    with their state (nothing copied), and the playing chain's DC block,
    crossfeed and EQ as the outgoing side of a fade at the convolution's
    input: for the fade both sets run on the same input and the convolver
    hears `(1 − g) · old + g · new`. `g` holds at exactly 0 while the
    sections that start fresh settle on the live input, unheard: 6.9 time
    constants of the slowest (its start-up down 60 dB; a Q 30 bell at
    20 Hz: 1.56 s; a 1 kHz bell of Q 2: under 5 ms); then it rises as a
    raised cosine to exactly 1 over 10 ms, or what the IR's fade bound
    asks (below; at most 500 ms). A hold over 5 s is refused (the edit
    takes the warm-up path below). The output plays on from the old
    chain's: without an upsampler or harmonics its first
    `hold + latency + 1` frames (the convolver's base block, at least one
    frame) are the old chain's next, sample for sample, at any block
    length; an upsampler's
    (or harmonics') filter reaches ahead of its reported latency, so the
    fade can show a few frames before it there (a 2x upsampler: from frame
    4134 of a 4218-frame latency). An unchanged
    stage, and each EQ channel's unchanged start (preamp, bands, tilt, in
    order), carries its state over; an edited section and the ones after
    it start fresh, settled in the hold and weighted in by the fade. A
    preamp change alone carries the states over scaled by the gain ratio
    (the sections are linear: exact), so it needs no hold. Another edit
    during a hold or fade
    waits for its end (the newest wins).
    - *The fade bound.* Each tap of the IR hears the fade at its own
      delay, so the fade is not exactly a crossfade of the two chains'
      outputs: while it passes through the IR, taps before it carry the
      new signal and taps after it the old. Where taps far apart cancel
      (a notch from a far reflection) the output can pass both settings'
      (two taps of ±1.9 20 ms apart, a 0.9 FS 1 kHz tone, the preamp 0 to
      −12 dB and a 10 ms fade: 1.28 FS where both play silence). For a
      steady tone it passes them by at most, summed over the inputs, how
      far the IR's spectrum under the fade strays from the segment between
      0 and its settled spectrum, times how much the edit changes that
      input there. The chain measures the first factor when it is built,
      off the audio thread (cached per IR set; about 130 ms on this
      machine's cores for a 2x2 matrix of 32768 taps), per output, input,
      fade length (10 ms to 500 ms) and frequency band (1024, log-spaced
      from DC): at fade positions over the whole IR, spaced so the
      second-order slack between two is at most 0.01, plus that slack;
      at FFT bins, each band taking the bins just outside it too. At the
      takeover it takes the second factor per band from both chains' EQ
      responses at 8192 points (computed at build): the largest change
      over the band's points and the two next to it (a changed DC block or
      crossfeed counts as twice full scale; an EQ section narrower than Q
      100 is not resolved there: refused). The shortest fade whose bound
      (times 1.1, for what the sampling leaves) stays within 0.125 FS
      (−18 dBFS) wins; none within 500 ms, an IR over 131072 taps, a fade
      not measured or any value not finite: refused, and the edit takes
      the warm-up path below. The bound is for one tone of up to full
      scale at the EQ's input; a sum of tones passes by at most the sum
      of theirs. Checked on a few hundred IRs (sparse taps, dense decays,
      cross routes) and edits (high-Q and low sharp bands, shelves, a steep
      high-pass, the preamp, on one input or both): every accepted takeover
      stayed within its bound, every refused one landed by warm-up. A room
      calibration is compact: a drag step keeps its band's own fade; a
      6 dB band step fades over about 90 ms, a −12 dB preamp change over
      500 ms. The two far taps above take a 0.2 dB change over in 10 ms and
      a −12 dB one only over 500 ms (within the bound); a mute there warms
      up.
  - *After the convolution, keeping its input* (the upsampler, harmonics,
    limiter or output): the incoming chain copies the convolver state as
    it starts warming up and warms up for its latency alone. The copy is
    budgeted per period (a quarter of it at a conservative 2 GB/s, about
    5 MB for a 512-frame period at 48 kHz; a 2x2 matrix of 32768 taps is
    about 3.4 MB); a larger state is not copied.
  - *Otherwise* (a new or changed IR, a state over the budget, a lane still
    busy on a worker, an edit before the convolution on an A/B pair): the
    convolution starts from silence and the new chain warms up for its
    ring too, past the reported latency (the IR's length less one, at most
    1 s), so it never takes over before its IR's peak or with its tail
    missing; and, with a convolution, at least for its fresh EQ, DC
    block and crossfeed to settle (6.9 time constants of the slowest, at
    most 5 s), so their start-up never reaches the crossfade.
  A follower keeps the newest chain that arrives during a swap (latest
  wins), so the last of several quick edits always lands. Its queue holds
  two built chains; the control side keeps the newest that does not fit
  (a newer one replaces it, the older dropped there, off the audio
  thread) and queues it on its next reap (each reaper tick, each config
  edit). The follower takes at most two a period.
  A chain that joins a running one off its rational frame grid resets to
  the grid's position on the audio thread; a chain that has not run yet
  leaves its (clean) convolution as it is, so the join clears nothing
  (a million-tap IR's state would take milliseconds). A reset while a
  channel worker is late leaves its lane's convolver to the reset when
  it is back, and the chain is not taken as clean meanwhile.
  An in-place takeover counts as a chain swap for a measurement
  (`measure_play`: an `active` run faults with `ChainSwap`), on the
  master and on a follower.
- **Convolution guards (`EngineConfig.convolution_safety`,
  `set_convolution_safety`):** room-correction filters can boost a lot, and
  a sub's driver has limited excursion below its tuning. This machine's
  setting (not part of presets; left out of `config.json` at its defaults):
  `{ "max_boost_db": 12, "subsonic_hz": 20 }`, `max_boost_db` in `0..=24`,
  `subsonic_hz` in `5..=60`. The engine checks each chain's combined
  convolution per output row (a per-channel IR is a row of one entry; a
  matrix output sums its entries) when it validates a config, off the
  audio thread, so every path that installs a convolution goes through it:
  `set_config`, `set_convolution`, a preset load (global or the
  convolution part), `set_ab` (side B) and the config the daemon starts
  with.
  - **Every build is guarded.** Besides the config check, each chain build
    holds the exact IRs it loaded, at the graph rate it builds for, to the
    boost cap: chain A and side B on the master (an edit, a rate change, a
    reopen), and each follower's own chain. A build over the cap (a file
    rewritten since, a rate the check did not see) is refused there: the
    master falls back to a chain without convolution (`status.error` says
    so), a follower stays off. A daemon waiting for its device validates
    configs as a running one does.
  - **Boost cap:** a row whose certified sine worst-case gain (the bound
    the A/B shared headroom uses, above) is more than `max_boost_db`
    (+0.01 dB slack) is refused (`INVALID_PARAMS`), and nothing changes:
    "the convolution boosts up to +13.0 dB on L, over the +12.0 dB cap
    (convolution_safety.max_boost_db)", prefixed `A/B side B: ` for chain
    B. Past 1 Mi taps the bound is the L1 norm (looser), so a very long
    filter can be refused below its real peak.
  - **Subsonic warning:** a row with more than 0.1 dB of gain
    (`Σ_i |F_i|`, every entry in phase) below `subsonic_hz` warns (a
    flat filter at 0 dB does not); not a refusal.
  - **Clipping risk:** with the limiter off (`chain.limiter.mode: off`), a
    row whose peak is more than the output headroom
    (`chain.output.headroom_db`) warns that loud passages can clip.
  - The warnings are in `Status.conv_warnings`: `[{ side: "a" | "b",
    device?, output, kind: "subsonic" | "clip_risk", gain_db, freq_hz?,
    limit_hz?, headroom_db?, message }]` (`message` e.g. "Below 20 Hz: +4.2
    dB on L (largest at 12.5 Hz)", "B: " in front for chain B; `device` and
    "follower <device>: " in front for a follower's own chain), and in the reply of
    the call that installed the config: `set_convolution`,
    `set_convolution_safety`, `set_config` and `load_preset` reply
    `{"notes": [message, ...]}` instead of `null` when there are any;
    `set_ab` adds them to its `notes`.
  - Followers run chain A as the master does, its limiter included (each
    follower builds its own copy from the same config), so the guards and
    the limiter cover them too; a follower with its own chain
    (`outputs.followers[].chain_patch`) is guarded on its own (its
    convolution, limiter and headroom).
  - CLI: `avdsp ctl conv list` prints the guards and the warnings,
    `conv set|add|remove|enable|disable` print the notes, and `avdsp ctl
    conv safety [--max-boost-db DB] [--subsonic-hz HZ]` shows or sets the
    guards (`config get|set` reach them too).
- **Convolution response (`conv_response`):** the frequency response of a
  chain's convolution, for the app's plot. Params `{ side?: "a" | "b", device?:
  string }` (`{}` for the defaults): `side: "b"` is A/B side B's chain (refused
  without A/B, or with a `device`), `device` a follower's own chain (its
  `chain_patch` applied, as the engine builds it; the master's device or
  none is the master); default chain A on the master. The IRs are the ones
  the engine plays: loaded through the engine's IR cache at the graph rate
  (else `rate.default_rate`), resampled, every entry and item `gain_db`
  applied, and combined as the engine combines them (`INVALID_PARAMS` for a
  file that does not load or a chain the engine would refuse). Read-only,
  off the mutation lock, a slow call (above); the curves are kept for the
  last four loaded IR sets and rates, so a redraw is cheap. Reply:
  ```json
  { "rate_hz": 48000, "side": "a",
    "freqs_hz": [20, 20.58, ..., 20000],
    "combined_taps": 65536,
    "items": [
      { "index": 0, "label": "Room calibration: Living room", "calibration": true,
        "taps": 65536,
        "curves": [ { "in": 0, "out": 0, "mag_db": [...], "group_delay_ms": [...] },
                    { "in": 1, "out": 1, "mag_db": [...], "group_delay_ms": [...] } ] },
      { "index": 2, "label": "Filter matrix", "taps": 8192,
        "curves": [ { "in": 0, "out": 0, ... }, { "in": 1, "out": 0, ... } ] }
    ],
    "outputs": [ { "out": 0, "mag_db": [...], "group_delay_ms": [...] },
                 { "out": 1, ... } ] }
  ```
  - `freqs_hz`: 241 log-spaced points, 20 Hz to 20 kHz (about 1/24 octave,
    the app's overlay grid), those above Nyquist left out; shared by every
    curve. Nothing is smoothed: each point reads the filter's DFT (bins at
    most 0.5 Hz apart) between its two nearest bins.
  - `items`: each enabled item in chain order, `index` its place in the
    chain's `convolution` list, `label` its `name` else its file's name
    (`"Filter matrix"` for an unnamed matrix), `calibration` on the loaded
    room calibration's item, `taps` its longest IR at `rate_hz`. A plain
    item has one curve per channel of its file (`in` = `out`), a mono file
    one curve without `in`/`out`; a matrix one curve per route.
  - `outputs`: the whole convolution stage per output, every enabled item in
    series; a matrix output sums its inputs coherently (the response to a
    signal common to every input; the boost cap bounds the in-phase worst
    case instead). An output with no entry reads −200 dB.
  - `combined_taps`: the combined convolution's longest entry at `rate_hz`,
    what the engine runs (0 without an enabled item).
  - `mag_db` in dB (−200 at the quietest, rounded to 0.001 dB);
    `group_delay_ms` in ms (0.1 µs steps), `null` where the magnitude is
    below −120 dB.
  - CLI: `avdsp ctl conv response [--device D] [--side a|b] [--json]`
    prints each curve at a few frequencies and its range.
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
    `hardware_db` the ceiling bounds the digital part only. The app and
    `avdsp ctl volume --mode software` set `hardware_db` to 0 when they
    switch into `software` with none set, so an earlier mode's hardware
    attenuation does not double; `--hardware-db none` keeps the mixer as it
    is.
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
- **Followers** (Multi-device output) with `follow_volume` move with this
  level: each master volume change by as much on top of the follower's own
  level, and the balance and mute, as digital gain on the follower
  (`outputs.followers[].follow_volume`).
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
    level lowers the stored level to it) and moves the desktop slider to
    the matching percent.
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
  key is restored only while it still points at one of our sinks. A
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

The calls: `route { target, sink }` (reply `null`) moves one stream
(`{"node": 83}`) or every stream of an app (`{"app": "name"}`) to `"dsp"`,
`"direct"` or `"default"` (unroute: the stream falls back to its own target
or the session default). `set_routing_mode` takes `"system"` (the default)
or `{"apps": {"rules": [{"app": "name", "sink": "dsp" | "direct" |
"default"}]}}`; a rule matches `application.name` or
`application.process.binary` case-insensitively and exactly.

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
10. `SourceOver24Bit`: only when the source precision is actually *known* to
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

`intact` means "no breaks". The output word length is the negotiated ALSA
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
no history any readable player is shown. `now_playing` returns it
(`Option<NowPlaying>`), the same value the event carries.

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
  `$XDG_CONFIG_HOME/audivane-dsp` before the sandbox and mounts it writable;
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
- Optional `tone` (`ToneConfig`, all channels): the bass and treble knobs,
  `bass_db` and `treble_db` (each default `0`, within ±6 dB; the app steps
  by 0.5 dB). Bass is an RBJ low shelf at 140 Hz, Q 0.48; treble an RBJ
  high shelf at 4.9 kHz, Q 0.49 (gentle, first-order-like shelves). A knob
  at 0 dB adds no filter, so `{"bass_db": 0, "treble_db": 0}` is the same as
  no `tone` (bit for bit). The shelves run after every channel's bands
  (whatever `linked_enabled` and the sections say) and are not part of any
  band list. Included in the auto preamp, the EQ response and A/B level
  matching. APO export writes them as the last two linked filters (`LS`
  140 Hz Q 0.48, `HS` 4900 Hz Q 0.49); an import whose linked filters end
  with exactly those shelves (a 0.5 dB step within ±6 dB) reads them back
  as the tone. `ab stage eq` (and the app's EQ compare) turns the tone off
  in B with the bands (`"tone": null`).
  CLI: `avdsp ctl eq tone [--bass <db>] [--treble <db>] [--off]`.
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

The order is DC block → pitch → stereo image → EQ (with tilt) → convolution →
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

`Status.conv_latency_ms` (default 0) is the convolution's own delay in the
active chain, in ms at the graph rate (the convolution runs there): the
IRs' bulk delay as the engine runs them (the index of the largest
magnitude, after resampling to the graph rate and combining; the latest
over channels or matrix entries) plus the convolver's partition block. The
block is part of `filter_latency_ms`; the bulk delay is the IRs' own (0 for
a minimum-phase IR) and is not. It does not change with the upsampler. It
is 0 without convolution, on Direct, or with DSP off. With a calibration
bundle it is the master's convolution only (each follower's latency is in
`outputs`); in A/B it is the playing side's. `avdsp ctl status` prints it
next to the latency.

## Status: device and rate fields

All `#[serde(default)]`:

- `waiting_for_device: bool`: the configured device is not there (or
  keeps failing to open right after it went); the sinks run on the
  engine's own clock and the engine opens the device once it is back (see
  "Devices"). `state` is `error` meanwhile.
- `unresolved_devices: UnresolvedDevice[]` (left out when empty): outputs
  held closed because identical DACs moved USB ports (see "Identical
  DACs").
- `device_caps: DeviceCapabilities | null`: the last probe of the configured
  device, also when that device turned out unusable (then `rates` is empty
  and the other fields say why). `null` before an open, when the probe failed
  or the device could not be opened at all. `{ device, channels,
  channels_min, channels_max, rates: [{ rate, formats: [{ format, sbits }]
  }], rate_min, rate_max, formats }`: `channels` is the count the engine
  opens the device at (see "Output channels"); `rates` ascending, only rates the
  engine could ask for with a usable format at `channels`; `rate_min` /
  `rate_max` (equal for a fixed-rate device) and `formats` are what the
  device accepts at all, regardless of channel count and the engine's rate
  list.
- `effective_rates: number[]`: the graph rates offered to PipeWire (see
  "Rate policy"); empty before the first open and while the output is closed
  after a failed open.
- `active_sink: "dsp" | "direct" | null`: which sink feeds the DAC now
  (`null` while neither plays).
- `direct_rates: number[]`: the rates Audivane Direct runs at: the native
  rates among the effective ones (widened to the device's native rates when
  none of them is native), plus, with `bypass_resampler: "engine"`, the rates
  the engine converts to a native one ("The engine bypass resampler"); empty
  when no device is open, or when Direct can run at no effective rate (none
  is native and none can be converted).
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
  checks `check_varispeed_budget` across `rate.allowed_rates`, including every
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
rejected. Every `set_varispeed` call, including an empty `{}`, follows the
normal request path. To inspect without side effects, use `status` (`avdsp
ctl varispeed` with no arguments reads `status`).

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
instance of the master's chain, or its **own chain** (a patch over it,
below).

**Config** (top level of `EngineConfig`, `#[serde(default)]`, no followers
by default):

```json
"outputs": {
  "followers": [
    { "device": "hw:CARD=USB,DEV=0", "enabled": true, "trim_ms": 1.5, "volume_db": -3.0,
      "follow_volume": true, "follow_ref_db": -20.0,
      "chain_patch": { "convolution": [ { "ir_path": "/home/me/sub.wav",
          "matrix": [ { "in": 0, "out": 0, "ir_path": "/home/me/sub_L.wav" },
                      { "in": 1, "out": 0, "ir_path": "/home/me/sub_R.wav" } ] } ] } }
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
- `volume_db` (default 0): its own digital volume, `-120..=0` dB, after
  the chain's. Different DACs feed different amplifiers, so it is the
  follower's own level, never tied to the master's absolute one.
- `follow_volume` (default `false`; **Follow main volume**): master volume
  changes move the follower by as much, on top of its own level, so the
  offset the user set between the devices (a subwoofer against the mains,
  a room calibration's) holds whatever the volume:
  - `follow_ref_db` (set by the daemon, left out when not following): the
    master's level as played (`Status.volume.volume_db`, the same level the
    follower tracks; the configured one before the engine reports it) when
    the follower's `volume_db` was last set or following was turned on. It
    plays at `volume_db + (master − follow_ref_db)`, at most 0 dB (a master
    raised further stops it there; its offset holds again on the way
    down). Turning following on keeps the level it plays at; a
    `set_follower` `volume_db` while following is the level it plays at
    now (against the master's level now); turning it off keeps the level
    it plays at as its `volume_db`. All three go by the master's played
    level, so a quantized hardware mixer (−31 dB asked, −36 dB on the card)
    moves nothing. A hand edit without `follow_ref_db` takes the master's
    configured level then. The engine reads the follower's own volume and
    its following as one word, so an untick is never heard for a block at
    the old level.
  - "The master's level" is what its DAC plays: the effective volume
    (`Status.volume.volume_db`, after the hardware's quantization, the
    hardware part included), so a master whose DAC mixer carries the
    volume gives the follower the equivalent digital change. It applies
    where the master's does: after the chain, before its limiter and
    quantizer, ramped over the same ~10 ms raised cosine. A staged
    hardware raise on the master (Order of writes) reaches it when the
    card is written, as the master plays it.
  - The master's balance on its first two **inputs** (the chain's input:
    a subwoofer's matrix summing both sides hears the balance as the mains
    do), ramped the same way; and the master's mute, digitally (whatever
    carries it on the master). Only while following.
  - A follower a calibration bundle adds or calibrates is turned on
    (Room calibration bundles), one added by hand is off; unloading the
    bundle leaves it as it is. A config from before this field follows
    exactly when its room calibration calibrates that follower (the file
    is written back in the new format on that start). While the loaded
    calibration is switched off (`bundle_enable`) each follower it
    calibrates is muted at runtime; its `enabled` is kept.
  Off, the follower's volume is independent: `volume_db` alone, no master
  volume, balance or mute. Either way its limiter and the volume guards
  apply.
- `output_channels` (optional, `[l, r]`; left out: the first two): the
  device channels its left and right play on, for a device with more than
  two ("Output channels"). A change reopens the follower only.
- `period_ms` (optional, `5..=50`; left out: **5 ms**): its device period,
  as the power of two of device frames nearest it at the rate it opens at
  (5 ms: 4096 frames at 768 kHz, 2048 at 384 kHz, 256 at 48 kHz; 5.3 to
  5.8 ms in time), within what the device offers; three periods queued. It
  is the follower's own, not the master's quantum × ratio: a short period
  needs little source ahead of the master, so the master's alignment stays
  small (Alignment). Its chain keeps the master's quantum (the same
  convolver blocks and latency): it runs more often on less, and a wakeup
  that ends a convolver block does that block's work (at most one block of
  the master's quantum, besides its period's resampling). A change reopens
  the follower only.
- `chain_patch` (optional, left out when unset): the follower's **own
  chain**, a JSON merge patch (RFC 7386) over chain A, applied as A/B side
  B's `patch` is (the same addition for stages A lacks, and the same array
  rule: `convolution` and `eq.bands` are replaced whole, so a patched list
  stays the follower's when A's changes). It is re-applied on every edit of
  chain A and after a preset load (a preset never carries it), so a follower
  that only swaps the convolution keeps following A's EQ, limiter and
  output edits, while a patched convolution stays its own. Any chain field
  may be patched, the upsampler's `target` included (the follower then
  opens its device at its own ratio: it reopens when the ratio it asks for
  changes, and again when the patch goes; a device that gave a lower ratio
  than asked keeps it through other edits). Without it the follower plays
  chain A exactly (it keeps the ratio it opened at when chain A's target
  changes).
- **The follower's chain is checked as chain A is**, under
  `outputs.followers[<i>].chain` in errors: the stages' ranges, its filter
  at every allowed rate, the CPU admission (its own thread, at every
  allowed rate: it gets none of the master's per-rate substitutes, so a
  patch over budget at any rate is refused, naming that rate), the
  multi-device budget below, and the convolution guards
  (`convolution_safety`): a boost over the cap is refused as
  `follower <device>: the convolution boosts ...`, and its subsonic and
  clip-risk warnings come in `conv_warnings` with `device` set and the
  message prefixed `follower <device>: `. The boost guard runs again on
  the IRs each time the follower's chain is built (an IR file rewritten
  after the edit): over the cap, the follower stays `"off"` (silent) with
  `error` set, and tries again every 2 s. The CLI's `follower` reply
  prints that follower's warnings.
- While any follower is configured, an enabled varispeed's `speed` is at
  most **1.996** (each follower corrects its clock on top of it).
- **Budget across devices:** at every allowed rate, each follower's
  clock-correcting stage (its varispeed stage at speed 2 in the worst case)
  plus its chain's resampling at its ratio (twice, for a swap) must
  fit the 800 M MAC/s per channel of one thread, and the master and the
  followers together must fit that times the cores they can run on.
- Chain commands take no device: an edit of the master's chain reaches
  every follower (built for its device, swapped in under a crossfade),
  except where a follower's `chain_patch` overrides it. A/B stays on the
  master. Followers mute while the master plays Direct or with DSP off.
- An `outputs` edit never rebuilds or reopens the master.

**Live call (`set_follower`):** adds, edits or removes one follower through
the ordinary config edit (validated, applied, persisted). A `device` not
listed yet is added (omitted fields at their defaults); `remove: true`
removes it (an unlisted one is refused). Omitted fields keep their value;
explicit `null` values are rejected, except `chain_patch`, where `null`
or `{}` clears the follower's own chain; `period_ms: 0` sets the default
period again. → `OutputsStatus`

```json
→ {"id":1,"method":"set_follower","params":{"device":"hw:CARD=USB,DEV=0","trim_ms":1.5}}
← {"id":1,"ok":{"alignment_ms":0.0,"master":{"rate_hz":0,...},"followers":[{"device":"hw:CARD=USB,DEV=0","enabled":true,"trim_ms":1.5,"volume_db":0.0,"follow_volume":false,"level_db":0.0,"state":"opening","offset_ms":0.0,"ppm":0.0,"phase_error_frames":0.0,"bandwidth_hz":0.0,"skips":0,"xruns":0,"latency_ms":0.0,"device_rate":0,"period":0,"error":null,"own_chain":[],"latency":{"rate_hz":0,...}}]}}
→ {"id":2,"method":"set_follower","params":{"device":"hw:CARD=USB,DEV=0",
     "chain_patch":{"convolution":[{"ir_path":"/home/me/sub.wav"}],"limiter":{"mode":"limit"}}}}
← {"id":2,"ok":{..."followers":[{..."own_chain":["convolution","limiter"],"chain":{...},
     "latency":{"rate_hz":48000,"chain_frames":1264.0,"chain_us":26333.3,
       "compensation_frames":0.0,"compensation_us":0.0,"alignment_frames":0.0,
       "alignment_us":0.0,"total_frames":1264.0,"total_us":26333.3}}]}}
→ {"id":3,"method":"set_follower","params":{"device":"hw:CARD=USB,DEV=0","chain_patch":{}}}
→ {"id":4,"method":"set_follower","params":{"device":"hw:CARD=USB,DEV=0","remove":true}}
```

**Alignment.** A follower reads the source only once the master has, so the
master is delayed (`alignment_ms`) until the slowest follower has a little
source ahead. What a follower needs is its own queue and a master period
less the master's queue: at the default 5 ms period (three queued, about
16 ms) against the master's three periods of the default quantum (64 ms
queued), nothing; a follower at the quantum's period (three of 21.3 ms at
768 kHz from 48 kHz) needs about 40 ms. Until a follower's
need is measured (a follower added, its period, the quantum or another
path changed) the alignment starts at what it is expected to need, so it is
placed with the source it needs and heard from its first second without a
gap: its queue and a wakeup (four of its periods), 13 ms (its fade, its
stage, the USB packets and the readings' spread), less the master's queue
just before it wakes and half a quantum for a late graph cycle (one and a
half quanta), plus 3 ms; only up (6 ms at the default period and quantum).
A follower expected to need more than the cap starts unaligned by the rest
(below). It grows by whole
milliseconds, at most once a second, never past **120 ms** (what a
follower at the default quantum's period needs, about 35–60 ms, plus the
whole negative trim range), and never shrinks while anything plays.

It comes back down in silence. Each follower keeps the least source it had
ahead at its worst since its last change (at any phase of the two clocks'
wakeups: the least over each second, less the shorter of its period and
the master's write, as the growth rule counts it; its `need_ms` is the
alignment less that, in source milliseconds: the slack, playback time,
times the speed it was measured at. Slack measured slower than 1× gives no
need: at 1× again the follower would play through its source faster than
it was measured). Once every follower that plays has measured it
(3 s after the last change), the master's need (`alignment_need_ms`, the
most of them) is kept as a high-water mark: the most it has been since the
followers' paths last changed (a follower added or removed, a chain, the
quantum, the master's device, a follower's device, period, chain patch or
output pair). A pause, an xrun, a speed or chain change starts a
follower's worst slack again, and the play after it never lowers the mark,
so a stall seen once still counts. Once the need has been measured over
**30 s** of play (pauses not counted) and the alignment is at least 1 ms
more than the need plus **3 ms**, a shrink to that
(`alignment_shrink_ms`) waits for silence: the RT thread applies it only
once the source (at the delay line's input, at most −80 dBFS) has been
silent for the longer of the old and new delays plus 150 ms (the master's
queue and the followers' read points behind it, so the followers' step
lands in silence too), and at once while nothing streams (a pause, the
start of a stream: the delay line's history is silence then). So it lands
at a pause, in a gap between tracks of a quarter of a second or more, or
as a stream starts, never while music plays (a gapless album keeps it
until the next pause), and never during a measurement. A growth drops a
shrink waiting; a need that grows past it drops it too; a follower added
or removed forgets the need. Not while a follower plays unaligned.

The master's own queue (three periods of the quantum, about 53 ms on
average at the default 1024 frames) is the floor: a follower then adds
nothing. It stays three periods: the wakeup's work (the graph cycle, up to
half a quantum when a client is late, then the chain, admitted up to 90 %
of a core) must finish within what is queued when the master wakes, which
two periods (one queued, 21.3 ms) do not leave at the admitted ceiling; a
shorter period under the same quantum moves that cushion into the input
ring (two quanta) instead of removing it.

A follower whose path needs more than the cap (large periods) plays
**unaligned**: later than the master by `offset_ms` (from the start when
its expected need is past the cap, else grown the same way; at most
200 ms), locked and heard. It steps back once it can spare the delay at
every phase of the two clocks' wakeups (a smaller period on it, a faster
master path): all the way to aligned, or by 30 ms or more (less is not
worth a fade, and the estimate it starts from can be up to about 20 ms
over); every (re)open, a rate change included, starts it aligned again
(or, until it is placed, late by its expected need past the cap). Each of these changes, a trim edit, and
switching varispeed on, is a step the followers take exactly: muted for
their 5 ms fade out and in, not counted in `skips`. A step of the master's
position lands at every DAC at the same moment, whatever each device
queues: the master publishes the source position its output carries at
its DAC where it jumps, and a follower takes the step only when its own
read point gets there (less its trim and `offset_ms`: it plays the master's
position that much later), shut from just before it. An alignment change
while followers play is made a command lead and two fades (10 ms of
playback: source frames at the fastest speed the stage plays until the
step) after it is asked for (rounded up to a block start, so it lands where
it is announced; under varispeed, within a block of it), and announced, so
a follower that queues more than the master (its read point runs further
ahead) shuts before the jump too and stays shut until it has taken it: it
is never heard out of place, only silent a little longer (about its extra
queue; a follower at the default period queues less than the master and is
silent for the fades only). A shrink in silence (above) is made at once. An unaligned follower
cannot play earlier than it does: a trim that moves it earlier adds to its
`offset_ms` instead, and a later trim takes `offset_ms` back first (down to
0: aligned again), only the rest moving it.

**Placement after a pause or a reopen.** A follower places itself rather
than acquiring whenever its rate state is good: it averages its first
readings (their interquartile mean), skips whole frames by it as soon as it
is then within the **placement floor** (0.05 ms) with confidence and the
readings' line stays within it over the follower's own delay (its queue
and a period) plus a tenth of a second, and is heard at once, the loop
correcting the residual while it plays. Until its usual lock condition has
held (2 s), a placed follower is muted again only when its error is
outside the floor (the newest readings beyond it, or their line beyond it
over that same horizon, so a drift is caught before it reaches the
device); it then re-places itself muted, and after a few failed placements
in a row it acquires from scratch. In the simulation it is heard 0.2 to
0.7 s after a resume, a rate change, a reopen or a start, within the floor.
- **What it places from.** The clock ratio the loop locked at, per pair of
  master device and follower device and per rate family (48 and 44.1 kHz):
  kept through a pause, a rate change, a reopen or an unplug, and on disk
  (`$XDG_STATE_HOME/audivane-dsp/follower-clocks.json`, written when it
  moved, at most every 30 s and when a follower stops), so a daemon
  restart, a reboot or an update places at once too. The master's
  alignment is kept there per master device as well. A file that does not
  parse is ignored (the loops learn again).
- **A pair never seen** (a fresh install, another DAC) is a **first
  start**: heard within a second whatever its clock, starting at the
  nominal rate (0 ppm). Until its rate is known and its error is within
  the placement floor and not drifting out of it (or its loop has
  locked), the placement and the heard guard allow a wider tolerance,
  0.25 ms (12 frames at 48 kHz), which keeps ±100 ppm heard while the loop
  pulls the rate in; then the floor rules as for any pair. Pairs with a
  saved rate never use the wider tolerance. The rate is looked at in the
  readings' slope (the loop's own correction added back, so it holds
  while the follower plays) once that slope is known (to about 12 ppm, or
  from a full set of readings over half a second at least) and taken when
  it stands clear of their noise; taken while heard, the phase error the
  old rate left is skipped out (a 5 ms fade out and in). In the simulation, at ±50 ppm on 128-frame
  periods and ±100 ppm on longer ones: heard after 0.3 s at 128 and 256
  frames (within 2 frames), 0.5 s at 512 and 1024 (within 2.2 and 3.3
  frames; within the floor from 0.5 and from 1.4 to 2.4 s), 0.8 s at 2048
  (readings 43 ms apart; 6.4 frames, within the floor from 3.1 s).
- **A master change** (another master device) never carries the last
  pair's rate: each follower starts from its new pair's.
- **A follower xrun** shuts the follower until it is placed again: its
  fade closes, its queue and the device's buffer are dropped (also when the
  recovery came inside a status or room query, before anything was
  written), and its output stays silent until everything its chain still
  holds has played out (the chain's whole tail: a convolution's filter
  length included). Nothing from before the xrun is heard after it, and
  its fade opens only once that silence ends (no step when it lifts).
- **A late master reading.** A point of the master's DAC position off its
  line by more than 1.5 ms, with no known step, new stream or ramp, is held
  back for one master period (the follower's readings wait); the next
  point that agrees with it makes it count. A late reading would otherwise
  span a whole master period of a short-period follower's readings and
  read as a jump.
- **Reading bias.** A DAC's reported delay can carry a bias that moves
  slowly: a USB pointer on 125 µs microframes or on 1 ms packets, read at
  a phase that sweeps against the device's clock. A fraction of a second
  of readings can carry most of that sweep. The engine follows each
  device's readings against its own clock (a line over a long window) and
  measures how far a quarter-second's mean strays from it: that RMS, kept
  on disk with the pair, bounds the placement (it waits for the floor less
  twice the RMS). Once a device's line has run 60 s, its deviations from it
  are taken out of the readings, so the loop holds the long-run mean, not
  a short window's. A device that played a minute before keeps its line's
  slope over a new stream and is corrected again from its third reading.
- **A device that reports in whole 1 ms packets** is not fit that way (a
  line through part of a slow sweep takes the sawtooth for the clock's
  rate). It is heard at once, like a fresh pair, and the gate, the skip
  threshold and the out-of-floor checks allow a packet's spread, so the
  loop averages the sweep instead of chasing it. Where the phase wraps
  (the readings jump a packet as they cross its edge), the readings on
  either side sit on the long-run line; once two wraps a whole sweep apart
  have been seen, the line through them is taken out of the readings, the
  follower is placed again from them (a brief mute) and holds the floor.
- **Known limits.** A bias that does not move at all (a reading always
  taken at the same phase of a coarse grid) is a constant, which no loop
  can see; it belongs in the trim. On a device that reports in whole 1 ms
  packets the follower may sit up to about one packet off until its first
  full sweep (minutes at most: a sweep lasts a packet over the device's
  clock against its bus's, 100 s at 10 ppm), then holds the floor. Through
  a pause it stays exact, and after its own xrun it is exact again from
  the next wrap (the line's slope is kept); the rate is saved per pair,
  but a reopen or a restart meets the grid at a new phase and learns the
  line again (up to about a packet off for a sweep).

**Latency compensation.** With different chains each device's chain
latency differs: the convolver's block (64 to 1024 frames, by the period),
the upsampler's filter delay, the limiter's look-ahead. The engine evens
every device's chain path out to the longest, so the acoustic offset
between the devices does not depend on which filters each one plays or on
the rate:
- a follower whose chain is longer than the master's delays the master: a
  **compensation** part of the master's alignment delay (whole graph
  frames, up to 1000 ms), on top of `alignment_ms`;
- a follower whose chain is shorter than the master's (with that
  compensation) **pads** its own output by the difference (whole device
  frames, up to 1000 ms, an exact delay after its chain).

Unlike the alignment, these are chain latencies to the drift loop, not
steps: the master's compensation and a follower's pad change only with a
chain (an edit, a swap, a reopen), and each device's path moves by the
same amount, so the loop never re-places a follower. A change is a fade on
the device it touches: the master's compensation cross-fades its delay line
(5 ms, as an alignment change); a follower's pad fades out, switches and
fades in (5 ms each). A target applies once it has held still (0.25 s for
the master's compensation, 0.5 s for a pad, a follower's own chain swap
not on its way): an edit that reaches both sides swaps them a little apart,
and the difference comes back by itself. Until then, and while the
master's compensation still falls short of a follower's longer chain, that
follower is shut (`muted`), rather than heard out of place. A follower
that is lost or stopped needs no compensation: the master's settles to
what the followers that play need (0 with none); while one is opening
(a reopen), the master's holds. A follower
that plays chain A at the master's ratio needs neither: its pad and the
compensation stay 0 and it plays bit for bit.

Only the processing latency is compensated, never what an IR does: a
convolution's own delay (a pure-delay IR, a linear-phase filter's group
delay) is the signal, and room-correction filters set the devices'
relative timing with it. A follower loading a pure delay of `d` frames
plays exactly `d` later; its convolver's block is compensated.

A need beyond 1000 ms is compensated up to 1000 ms; the rest shows in
the follower's `offset_ms` (state `unaligned`): positive when the
master's compensation falls short (it plays late), negative when its pad
does (it plays early).

Known limits:
- **Keep every DAC at one rate, or measure that offset.** The
  compensation covers the engine's chains, not the DACs: a DAC's own
  latency depends on its rate. On the test rig, a follower whose DAC ran
  at 48 kHz against a master's at 96 kHz (2× upsampled, the chains
  compensated exactly) played 965–981 µs late. With DACs at different rates, set that
  follower's `trim_ms` from a measurement.
- **Room correction runs at speed 1.** Under varispeed the master's
  compensation is source frames before its stage, so at a speed `s` it
  delays the master by its frames over `s` (at 2×, half of what a longer
  follower chain needs); the drift loop takes the difference (a skip after
  a large speed change). A follower with a longer chain is in place at
  speed 1.

**Status (`Status.outputs: OutputsStatus`):**

- `alignment_ms`: the master's added delay (0 without followers). It is in
  `Status.latency_ms`, not in `filter_latency_ms`.
- `alignment_need_ms` (left out until measured): what the followers need
  of it (the most of their `need_ms`), kept through a pause.
- `alignment_shrink_ms` (left out when none waits): the alignment it comes
  down to at the next silence.
- `master` (`PathLatency`): the master's path as it plays, in its output
  rate's frames and µs: `chain_frames`/`chain_us` (its chain's latency),
  `compensation_frames`/`compensation_us` (the compensation above),
  `alignment_frames`/`alignment_us` (`alignment_ms`), `total_frames`/
  `total_us` (the three), `rate_hz`. Zeros without followers.
- `followers[]`, in config order: `device`, `enabled`, `trim_ms`,
  `volume_db`, `follow_volume` (absent from daemons that predate it, read
  as `false`: they never followed), `level_db` (the level it plays at
  now: `volume_db`, moved by the master's changes when it follows; absent
  from daemons that predate it), and
  - `state`: `"off"` (not running: the engine is stopped, or it could not
    start, see `error`), `"opening"`, `"locking"` (playing, not in sync yet,
    muted), `"locked"` (in sync, heard), `"muted"` (in sync, not heard:
    disabled, or the master plays Direct or with DSP off), `"unaligned"` (in
    sync and heard, `offset_ms` late), `"lost"` (its device is gone or
    failed; it reopens when it is back). A follower placed after a pause
    or a reopen (above) shows `"locking"` while it gathers its readings and
    `"locked"` from the moment it is heard.
  - `offset_ms`: how much later than the master plus its trim it plays (0
    when aligned): its unaligned lateness plus what the latency
    compensation cannot cover (negative: earlier).
  - `ppm`: its clock against the master's (the drift loop's estimate);
    `phase_error_frames`: the loop's filtered phase error, source frames;
    `bandwidth_hz`: the loop's bandwidth (it narrows as it locks).
  - `skips` (phase jumps the loop corrected by skipping frames; the known
    steps above are not counted), `xruns`,
    `latency_ms` (its own path, from where it picks the source up to its
    DAC; not comparable with `Status.latency_ms`: in sync, its time from
    the player to its DAC is `Status.latency_ms + trim_ms + offset_ms`,
    what the app shows), `device_rate` and
    `period` (0 while closed), `period_ms` (its configured period, left out
    for the default), `need_ms` (what it needs of the alignment: the
    alignment less the least source it had ahead at its worst since its
    last change, in source ms; negative: none, by that much; left out
    until measured, while the master does not play, and while its slack was
    measured slower than 1×), `error`.
  - `own_chain`: the top-level chain fields where its chain differs from
    chain A (`convolution`, `eq`, `upsampler`, `limiter`, ...); empty when
    it plays chain A. `chain`: its effective chain (chain A with its
    patch), with a patch only.
  - `latency` (`PathLatency`, as `master`): its chain's latency, its pad as
    `compensation`, `alignment` 0; zeros while closed. Every device's
    `chain + compensation` is the same time, to within a frame.
- `settling` (`null` when settled): why the devices' timing may still
  move: `follower <device> is opening` (or `locking`), `the master's
  alignment changed <s> s ago` (for 3 s after a change, while the
  followers re-acquire), `a follower is short of source: the alignment
  grows to <ms> ms`, `the master's chain compensation is moving`. Any
  follower counts, disabled ones too (their slack and chain still move the
  master). `measure_play` waits for it (Measurement).

**Errors (`INVALID_PARAMS`, code −32602):**

- `outputs.followers: at most 3 followers (got <n>)`
- `outputs.followers[<i>].device <d> is the main output device; a follower plays another one`
- `outputs.followers[<i>].device <d> is listed twice`
- `outputs.followers[<i>].trim_ms must be between -50 and 50 ms (got <value>)`
- `outputs.followers[<i>].volume_db must be between -120 and 0 dB (got <value>)`
- `outputs.followers[<i>].period_ms must be between 5 and 50 ms (got <value>)`
- `outputs.followers[<i>].chain_patch must be a JSON object (a merge patch on chain A)`,
  `outputs.followers[<i>].chain_patch.<key>: <why>` (it does not apply to
  chain A), `outputs.followers[<i>].chain.<field> ...` (its chain is out of
  range), `follower <device>: <refusal>` (its convolution guards, its CPU
  admission)
- `varispeed.speed is at most 1.996 while followers play (got <value>): …`
- `a follower at <rate> Hz needs <n> M multiply-adds per second per channel …`
- `the main output and <n> follower(s) at <rate> Hz need <n> M multiply-adds per second per channel together …`
- `<device> is not a follower` (`remove` of an unlisted device)

**CLI:**

```sh
avdsp ctl outputs                                    # alignment and every follower
avdsp ctl follower add hw:CARD=USB,DEV=0 --trim-ms 1.5
avdsp ctl follower hw:CARD=USB,DEV=0 --volume-db -3  # edit a listed follower
avdsp ctl follower hw:CARD=USB,DEV=0 --follow-volume on   # main volume changes move it too
avdsp ctl follower hw:CARD=USB,DEV=0 --enabled off   # mute it, in sync
avdsp ctl follower hw:CARD=USB,DEV=0 --period-ms 10  # its device period (0: the default, 5 ms)
avdsp ctl follower hw:CARD=USB,DEV=0 --chain-patch '{"convolution":[{"ir_path":"/abs/sub.wav"}]}'
avdsp ctl follower hw:CARD=USB,DEV=0 --chain-patch sub-patch.json   # the patch from a file
avdsp ctl follower hw:CARD=USB,DEV=0 --no-chain-patch                # chain A again
avdsp ctl follower remove hw:CARD=USB,DEV=0
```

## Room calibration bundles

A **bundle** installs a room calibration across the devices in one step:
per device, a filter matrix, a gain and (on a follower) a trim, from one
manifest that a measurement tool writes. It is loaded atomically and
removed exactly.

```json
{ "bundle_version": 1, "label": "desk 2026-12 mixed-phase", "design_rate_hz": 48000,
  "devices": [
    { "role": "master", "gain_db": 0.0,
      "matrix": [ { "in": 0, "out": 0, "ir": "mains_LL.wav" }, { "in": 1, "out": 1, "ir": "mains_RR.wav" } ] },
    { "role": "follower", "device": "hw:CARD=USB_1,DEV=0", "gain_db": 0.0, "trim_ms": 0.35,
      "matrix": [ { "in": 0, "out": 0, "ir": "sub.wav", "channel": 0 },
                  { "in": 1, "out": 0, "ir": "sub.wav", "channel": 1 } ] } ],
  "notes": "target, positions, tool version" }
```

**Manifest (`bundle_version` 1):**

- `label` (1–128 characters), `design_rate_hz` (the rate the filters were
  designed at), `notes` (optional, free text); unknown keys are ignored.
- `devices[]`, at most one `master` part and any number of `follower`
  parts:
  - `role`: `"master"` or `"follower"`.
  - `device`: a follower's ALSA device (required; not the main device, each
    once). On the master it is optional: when it names another device than
    the main one, the load notes it and the part plays on the main device.
  - `gain_db` (default 0, `-60..=24`): the part's convolution item gain; the
    boost cap counts it.
  - `trim_ms` (default 0): a follower's trim (`-50..=50` ms). The master is
    the reference: its `trim_ms` must be 0 (put relative delays in the
    followers' trims or in the IRs).
  - `matrix[]` (at least one): `{ "in", "out", "ir", "channel"?,
    "gain_db"? }`, as a convolution `MatrixEntry` (`out_j = Σ_i F_ji *
    in_i`, channels from 0, each pair once): `ir` is a WAV file **relative to
    the manifest** (or absolute), `channel` the WAV's channel (left out, the
    file must be mono). Polarity and delays live inside the IRs.

**What it maps onto:**

- the master part is **one convolution matrix item in chain A**, named
  `Room calibration: <label>` and tagged `"calibration": true`
  (`ConvolutionConfig.calibration`, left out when false). It goes where the
  previous calibration's item was, or after the other items;
- a follower part is that follower's **whole `chain_patch.convolution`**
  (one tagged item; its other patch keys stay) and its **`trim_ms`**. A
  follower the config does not list is added (enabled, volume 0 dB). A
  calibrated follower **follows the main volume** (`follow_volume` turned
  on from the level it has, if it was off), so volume changes keep its
  level to the mains. Chain A's other convolution items do not reach a
  calibrated follower; its other stages follow chain A as they did.
- The installed state is `EngineConfig.calibration` (`CalibrationConfig`:
  `path`, `label`, `design_rate_hz`, `notes`, the `master` item and
  `master_manifest_gain_db`, and per follower its `item`, `trim_ms`,
  `manifest_gain_db` and `restore`: its trim and its patch's
  `convolution` before, and whether the bundle added it). Loading a bundle
  over another (a replace, or the same one again) unloads the old one
  first, except a follower the old one added that the new one calibrates
  too: it stays exactly as it is (its entry, level, following and
  reference; only its convolution and trim change) and is the new
  bundle's to remove. Left out without
  one. The daemon sets it; a hand edit is taken as it is.
- **The calibration stays in place on every edit.** The daemon keeps chain
  A's tagged item and each calibrated follower's patch convolution equal to
  the installed parts on any config edit: `set_convolution` (a list without
  the item gets it back, so the app sends only the user's items),
  `set_follower` (a new patch keeps its convolution the calibration's), a
  preset load, `set_config`. `bundle_enable` (below) switches it off without
  unloading it; A/B compares without it. Its **level** is set per part with
  `bundle_gain` (below).
- **The master's item stays on the master.** The chain A a follower inherits
  leaves the tagged item out (before the follower's own patch), so a
  master-only calibration never reaches another output; a calibrated
  follower plays its own part.
- **The calibration is the daemon's.** A whole config (`set_config`) keeps
  the loaded calibration and its record whatever its `calibration` field
  says; only `bundle_load` and `bundle_unload` change it.
- **The record follows the followers' edits.** A calibrated follower that is
  removed drops out of the calibration (a follower added again by hand is
  the user's: unload leaves it). One whose patch `convolution` an edit sets
  to something other than the calibration's part keeps playing the part,
  and unload puts back what the edit set (the key left out: none).

**Level.** `bundle_gain { device?, gain_db }` sets one part's level: its
item's `gain_db` (`-60..=24`; `null`: back to the manifest's). `device`
names a follower part; left out (or `"master"`, or the main device), the
master's. It is a chain edit like any convolution gain: the master's item
in chain A (and a side B that keeps A's calibration), a follower's part in
that follower's patch only, the boost cap counting it (over the cap, or a
device that is not a part: an error, `bundle_gain: nothing changed: ...`,
and nothing changes). While a part's gain differs from the manifest's, the
record keeps the manifest's beside it (`master_manifest_gain_db`,
`manifest_gain_db`; left out otherwise); it stays through every edit,
preset load and `set_config` as the calibration does, and a new
`bundle_load` (the same bundle again too) or an unload drops it. The reply
is the `CalibrationStatus`, the convolution guards' warnings in `notes`.

**Switch.** `bundle_enable { on }` switches the loaded calibration on or
off as one config edit. Off, chain A's tagged item (and a side B that
keeps A's calibration) is placed disabled, the parts the record holds
staying as they were: the main speakers play the user's own chain, without
the calibration's crossover. Each calibrated follower is muted at runtime
instead (its own `enabled` is kept, so turning the calibration back on
restores it); its patch item keeps playing, so its chain never changes and
a sub never plays without its low-pass, not even while a chain is rebuilt.
On, everything plays again. A side B with its own bundle
(`calibration_b`) is not affected. The guards hold the edit: a level raised
while off is over the cap on, and then nothing changes
(`bundle_enable: nothing changed: ...`); off always applies. A new
`bundle_load` (the same bundle again too) starts it on, an unload drops it
with the record, and a preset load or `set_config` keep it as they keep the
calibration. The reply is the `CalibrationStatus`, the guards' warnings in
`notes`; without a calibration `BAD_STATE` (`bundle_enable: nothing changed:
no room calibration is loaded`).

**Atomic.** `bundle_load` first reads the manifest and checks its shape and
every IR file (present, a WAV, the channel it names); then it installs the
bundle on a copy of the config (removing the loaded calibration first) and
applies that as **one config edit**: validated, the IRs loaded and held to
the convolution guards per device (the boost cap on the master's rows and,
as `follower <device>: the convolution boosts ...`, on each follower's),
CPU-admitted per device (the master's per-rate plan, each follower's own
thread at every allowed rate), then applied and persisted. On any error
nothing changes (`INVALID_PARAMS`, `bundle_load: nothing changed: <why>`, or
`bundle_load: <file problem>`).

**Unload.** `bundle_unload` removes exactly what the calibration installed:
chain A's tagged item; on each follower its trim and its patch's
`convolution` key as they were before the load (a patch left empty is
cleared), and a follower the bundle added is removed. A follower's volume
and following are left as they are: one that follows keeps following at
its level, with the master's mute and balance (an unload never moves,
raises or unmutes it). Edits made since to anything else stay. What is no longer valid to put back is skipped, with a
note each: a convolution item whose IR is missing or unreadable (when none
is left, the patch's `convolution` key goes), a trim out of range. The reply
is `null`, or `{"notes": [...]}` with those. `BAD_STATE` without one.

**At start-up**, a loaded calibration whose IR files are not all readable is
dropped on its own, as an unload does (the followers' settings from before it
put back where valid), and the rest of the config loads as saved; the log says
which file was missing (`the room calibration "<label>" is not loaded: ...`).

**Rates.** The filters are designed at `design_rate_hz`; the engine resamples
them to the graph rate as any convolution. Status warns when the graph rate
differs from it, and when a calibrated follower's DAC runs at another rate
than the main device's (a DAC's own delay depends on its rate, and the latency
compensation covers the engine's chains only: "Multi-device output", known
limits); while nothing plays, it warns when a follower's own chain asks for
another upsampling target than chain A.

**Presets.** A preset load **keeps** the calibration: it belongs to the room,
not to an EQ or convolution preset. Loading a convolution part replaces the
user's items and leaves the tagged item (at the end of the list); a saved
preset never holds the tagged item, and a preset's `modified` flag ignores
it.

**A/B.** Every side B keeps chain A's calibration unless it carries its own:
`AbConfig.calibration_b` (`AbCalibration { path?, label?, master? }`)
replaces chain A's tagged item on side B with another bundle's master part
(`master` left out: B plays without a calibration). `bundle_ab { path }`
sets A/B to that comparison: B is chain A (an empty patch, so it follows
every edit of A) with the bundle at `path` in its place, labelled
`Calibration: <label>`; `bundle_ab { path: null }` compares against no
calibration (`No calibration`). Level matching and the shared headroom are
the defaults; the bundle's master part is checked as side B (the boost cap,
`A/B side B: ...`; the CPU admission of the pair). **Known limit: A/B switches
the master only.** The followers keep playing the loaded calibration on both
sides (`notes` says so when the compared bundle has follower parts), so a
comparison of two sub-plus-mains calibrations compares their mains only.
Switching a follower's patch with `ab_select` would take a pair chain on each
follower, crossfaded at the same source frame as the master's (the
followers replay the master's commands at source positions, as they do for
varispeed), which is new audio-thread work.

**Calls:**

| call | reply |
| --- | --- |
| `bundle_load { path }` | `CalibrationStatus`, its `warnings` holding the rate warnings, then the load's notes (a master `device` that is not the main one) and the convolution guards' warnings (`notes` stays the manifest's text) |
| `bundle_unload` | `null`, or `{"notes": [...]}` (what it could not put back) |
| `bundle_show` | `CalibrationStatus` or `null` |
| `bundle_ab { path \| null }` | `AbStatus` plus `notes` (as `set_ab`) |
| `bundle_gain { device?, gain_db \| null }` | `CalibrationStatus` plus `notes` (the guards' warnings) |
| `bundle_enable { on }` | `CalibrationStatus` plus `notes` (the guards' warnings) |

**Status (`Status.calibration: CalibrationStatus | null`):** `label`,
`path`, `design_rate_hz`, `enabled` (whether it plays), `notes`,
`devices[]` (the master first: `role`, `device`, `entries` (filters),
`gain_db` (as it plays now), `manifest_gain_db` (the manifest's), `trim_ms`
(a follower's as it is now), `installed` (`false`: the follower was removed
since)), and `warnings[]` (the rates, above, while the engine plays).

**CLI:**

```sh
avdsp ctl bundle load desk/bundle.json    # prints the devices and warnings
avdsp ctl bundle show
avdsp ctl bundle unload
avdsp ctl bundle enable off               # off: mains without it, its followers muted
avdsp ctl bundle enable on
avdsp ctl bundle ab other/bundle.json     # A/B: B with that calibration (master only)
avdsp ctl bundle ab --none                # A/B: B without a calibration
avdsp ctl bundle gain -2                  # the master part's level, dB
avdsp ctl bundle gain --device hw:CARD=USB_1,DEV=0 3   # a follower part's
avdsp ctl bundle gain --device hw:CARD=USB_1,DEV=0 --reset
```

**App:** the Convolution page shows a "Room calibration" line (the loaded
bundle's label, its devices and warnings) with Load (a file picker) and
Unload, and a level slider per part ("Main", then each follower by its
device name) with a reset to the manifest's gain; the tagged item is not in
the page's list of impulse responses.

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
chain A and B together. With `channel_workers` on (and two cores or more)
each channel's upsampler runs on its own thread, so the share counted is
one channel's (a `channel_workers` change is admitted like an upsampler
change). It counts both channels on one thread instead when the workers
could not start, and while a follower is enabled (a follower runs chain A
whole on its own thread), so enabling a follower is admitted like an
upsampler change too. It is refused with `INVALID_PARAMS` when it needs more than 90 %
of a core (`status.limits.cpu_share`, 0.9) on the busiest thread **at the
landing playing now**: the graph rate the DSP sink plays (else the last
one it played since the daemon started) and the ratio the change would
open there. With nothing played yet, or a rate the change would not offer,
it is the heaviest landing (the worst case). The refusal names the source
rate:

```
Too heavy for this CPU at 768 kHz from a 192 kHz source: about 31.2 G MAC/s
needed (312% of one core), this machine sustains about 5.0 G MAC/s in real
time. Use fewer taps or a lower output rate.
```

The other landings are priced too: where an admitted design is over the
share, that source rate falls back per rate (see "Per-rate fallback" below)
instead of refusing the change.

With channel workers the share reads `(156% of a core on each channel's
thread)` and the sustained figure counts both threads.

A convolution filter matrix with a cross term runs whole on the audio
thread with the EQ before it, so it counts there in full beside that
thread's share of the upsamplers: one partitioned convolver per entry of
the combined product (entry lengths from the files' headers at the source
rate), priced like the FFT polyphase engine at one branch on the
convolver's own partition schedule, and the EQ's sections on both
channels. A convolution change (`set_convolution`, or any edit of either
side's list) is admitted too. Where a matrix counts in the config before
or after, no change that costs no more than the config it replaces is
refused, whatever else it touches (removing, disabling or shortening a
matrix, a preset that does that and moves the headroom, A/B off); an
upsampler alone over the share is still refused, lighter or not. Chain A's convolution, and its EQ while a matrix is enabled, are
part of the per-rate plan's key, so the plan is redone for them. When the matrix is what tips it over,
the refusal says so: `… the convolution filter matrix (one convolver per
filter, with the EQ before it, on the audio thread) needs about 140% of a
core, 152% in all with the rest of the chain. Use shorter filters or fewer
matrix entries.` Per-channel convolution runs in the channel lanes and is
not priced.

When chain A alone fits and A/B's side B (its own filter) tips it over,
the refusal names side B: `Too heavy for this CPU at 768 kHz from a 48 kHz source with A/B:
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

**Per-rate fallback.** For the requested chain A upsampler (DSP on), the
daemon prices every landing and keeps a per-rate plan in the engine: at a
source rate where the design is over the share by the estimate (`cause:
estimate`), or overloaded there earlier in this run (`cause: overload`, see
below), the engine builds another upsampler in its place, at the requested
target so the device lands where it would: the last upsampler setting
that ran without overload (as step 1 below) when it fits there, else the
engine bypass resampler's recommended design (`light_upsampler`; also when
neither fits by the estimate, the reactive fallback being the net). The
plan acts before the rate opens: the chain the engine builds for the new
rate's open is the plan's, so the heavy design is never built there; a
plan change at the rate playing goes through the chain swap (warm-up and
crossfade). A rate the design fits runs it: going back to 48 kHz after a
192 kHz track runs the requested design again. The plan is made again when
the request, the device probe, the last setting that ran or the overloaded
rates change, and only applies while the engine runs the request's chain A
upsampler and output section (not under a reactive step that replaced
them). A calm under the plan's design does not make the request the "last
setting that ran".

**A custom design that builds at some rates only.** Chain A's custom design
(a `design`, not a preset) is applied when it builds at one of the landings
the ladder picks for `rate.allowed_rates` at least (each checked on its own
with the same rules as the whole design, the searched length included).
Where it does not build (too short, too long, over a design limit), the
per-rate plan builds Recommended at the same target instead (`action:
recommended`, `cause: design`, with a plain `reason`). Where Recommended is
over the CPU share there, the first lighter form of it that builds there
and fits plays (`action: light_upsampler`: Recommended with a 120, 96 or
72 dB stopband, shorter at a rational ratio; at an integer ratio the FFT
polyphase engine costs the same for those lengths, so Recommended stays).
The ratio ladders follow the design's own buildable pairs, so a `max_rate`
target lands a rate on the highest rung the design builds at (44.1 kHz at
x8 when x16 does not build); from a rate where it builds at no rung the
target can pick, the ladder follows Recommended's. One selection decides
what plays at each landing: config acceptance, `check_design`, the per-rate
plan, admission and the cost table (which price the fallback that plays,
not the design that does not build), the varispeed budget (the incoming
and the outgoing chain as they play) and the followers. A follower asks
its device for the top of the master's ladder, and where its device opens
at a ratio the design does not build at anyway, it plays the fallback the
master would play there. A design that builds at none of the landings is
refused, in plain words
(`chain.upsampler.design.length: 1,279 taps is too short at 44.1k ×16
(needs about 1,700) and ... Use automatic length, ...`). Presets and chain
B (A/B) are still checked at every rate.

`Status.upsampler_rates` lists the landings (empty with DSP or the upsampler
off; before the CPU is measured, only the rates where the design does not
build, priced at 0):

```json
"upsampler_rates": [
  {"source_rate": 48000, "output_rate": 768000, "load": 0.43},
  {"source_rate": 192000, "output_rate": 768000, "load": 1.71,
   "fallback": {"action": "light_upsampler", "cause": "estimate", "load": 0.02}}
]
```

- `load`: the requested design's estimated share of a core on the busiest
  thread (A/B included), as admission prices it.
- `fallback`: absent where the request runs; else `action` (`reverted`:
  the last setting that ran, `light_upsampler`, or `recommended`), `cause`
  (`estimate`, `overload`, or `design`: the custom design does not build
  there), its own estimated `load`, and for `design` a `reason` in a few
  plain words (`too long for an equiripple design here`).

`Status.upsampler_fallback_now` is the row of the source rate playing now on
the DSP chain when it plays a fallback there (absent otherwise), so a client
can say so without matching rates.

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

The chain's steps are per source rate: once the DSP sink plays another
graph rate than the one the step was made for (`source_rate` in the report;
the engine neither switching nor in `Error`), the fallback ends, the engine
runs the request at the new rate (judged there afresh), and the rate left
counts as overloaded for the request (`cause: overload` in
`Status.upsampler_rates`), so the per-rate plan builds a lighter design
there the next time it opens. Those rates are kept until the upsampler,
the output or A/B changes, or another DAC turns up behind the name. A step
made while the per-rate plan's own design ran (the estimate and its
fallback both failed there) holds across rates.

Each step logs one line and sends an `overload` event when the engine
accepted it (`applied: false`), and another once the audio thread runs it
(`applied: true`; a step that reopens the device lands later; the pump may
send only the second; a step whose chain has not landed after ten seconds
is judged anyway). `Status.overload` carries it while it is in effect
(`null` otherwise):

```json
{"event":"overload","data":{"stage":"upsampler","output_rate":768000,
 "load":1.4,"action":"upsampler_off","episode":1,"applied":true,
 "source_rate":48000,
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
- `source_rate`: the graph rate it was made for (0: unknown); a chain step
  ends when playback moves to another.

`avdsp ctl status` prints it as `overload:`, `avdsp ctl watch` as an
`overload:` line.

## Spectrum

`spectrum` events arrive at about 10 to 20 Hz (one per analysis hop) and are
computed only while some client is subscribed. The RT thread copies samples
into preallocated wait-free rings (dropping whole frames when full: no
allocation, no lock); a non-RT thread analyses them. A drop starts the tap afresh, so a spectrum is
never computed across a gap. A period larger than half the post ring (×64
at a 2048 quantum or more) is queued as the mean of each 2, 4 or 8 output
frames, so it still fits whole. `post` is an empty array when the post tap
has produced no analysis for 400 ms while `pre` has (its audio keeps being
cut by gaps): `pre` is then sent alone rather than held back. Each tap (`pre` = chain input, `post` = chain output before the
quantizer, at the output rate) runs an 8192-point Hann FFT (50% overlap) and,
for log bins narrower than four of its bins (below ~800 Hz at 48 kHz), a
32768-point FFT (87.5% overlap, same hop). At 88.2 kHz and above both run on
the signal decimated to 44.1/48 kHz and serve the bins up to 20 kHz; the bins
above come from a third Hann FFT (no overlap, bins of at most 48 Hz) at the
tap's rate, or at 352.8/384 kHz for higher rates (half-band decimated, flat
and alias-free to 100 kHz).

- Bin `k` is centred on `f_min_hz * 2^(k / bins_per_octave)` (10 Hz, 24 per
  octave). Each tap's bins run up to its own Nyquist less two FFT bins of the
  FFT serving the top (the Hann FFT's last bins hold a mirror image), or
  100 kHz (the graph's widest range), whichever is lower; no bin straddles
  that top. So `pre` (graph rate) and `post` (output rate) can differ in
  length: 267 bins at 44.1 kHz (to 22 kHz), 269 at 48 kHz, 293 at 96 kHz,
  317 at 192 kHz, 319 (to 100 kHz) from 352.8 kHz up. A folded post period
  (above) is analysed at its folded rate.
- A bin reads band power: the interpolated power density integrated over the
  bin, divided by the Hann ENBW, so a sine reads 0 dB and noise reads its
  power in the bin regardless of FFT size (white noise rises 3 dB/oct, pink is
  flat). Below 20 Hz a bin is a fifth to two fifths of a 32768-point bin
  (1.46 Hz at 48 kHz), so a sine there shows as a hump about ±3 Hz wide
  around its bin. Where a local maximum's parabolic vertex falls inside a bin, that bin
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
- `check_design { design, target?, source_rate? }` → `{ errors, plans,
  notes, advice? }`.
  It runs the per-field bounds and, at each source rate, the ratio rules
  and budgets (`feasibility`): no kernel is designed, so a refusal comes
  back in well under a millisecond and it can be called as the user types.
  A design that passes is planned where the probed device lands each rate,
  and the driver's ladder keeps only the ratios the design builds at: for
  an automatic length that is a search per ratio (milliseconds for most
  designs, a second or two for a long automatic Optimal one; its
  prototypes are cached for the build that follows). The context comes
  from the current config:
  - `target` defaults to chain A's (pass one to ask "what if");
  - `match_output` resolves against chain A's output, its dither bits
    capped at the negotiated format;
  - the rates are `rate.allowed_rates` (or only `source_rate`, which must
    be 8000..=3072000); rates where the target resolves to x1 are skipped;
  - the rational pipeline count follows A/B (2, or 4 with A/B), and the
    block size is the engine's at each rate (`quantum`).

  `errors: [{ path, message }]` names the field each refusal is about:
  `design.window.beta` style for a bound, `target` or `source_rate` for
  the context, `design.rates` for a ratio the design does not run at,
  `design.length` (`design.lobes`, `design.block`) for a budget,
  `design.preserve_samples` at a decimating ratio, `design.cutoff`,
  `design.phase` or `design.window` for a band, phase or attenuation the
  design cannot keep, `design` otherwise. A design that builds at none of
  the landings the ladder picks (as the config validation decides; the
  target's own ratios where the ladder upsamples nowhere) has one refusal, in plain words, under the field of its first rate's
  refusal: what is wrong with the rates grouped by reason, then what to do
  (`1,279 taps is too short at 44.1k ×16 and 48k ×16 (needs about 1,700)
  and too long for an equiripple design at 176.4k ×4 and 192k ×4. Use
  automatic length, set 104 taps per input sample, or change the rate
  target.`). When a bound fails, no rate is checked.

  `notes: [{ source_rate, path, message, plays? }]`, at most one per allowed
  rate where the design upsamples, of two kinds:
  - the design does not build there but does at other rates: it is applied,
    and that rate plays `plays` (`recommended`, or `recommended_light` for
    a lighter form where Recommended does not fit the CPU) instead;
    `message` says why
    in a few words (`too short here (needs about 1,700 taps)`);
  - it builds there but does something differently: a band's
    `passband_end_hz` that does not fit the band at that rate (`path`
    `design.cutoff.passband_end_hz`, no `plays`; the message names the
    frequency, the fraction it came to and the fraction that stands in, as
    in `at 32000 Hz, the passband end 20000 Hz (1.250 of the Nyquist) does
    not fit below the stopband start (1): the passband ends at 0.908 of the
    Nyquist (14528 Hz) instead`).

  `advice` (absent when the design builds at every rate it plays): `{
  message, applied, details, fixes }`. `message` is the plain sentences
  above (ending `or allow fewer rates.` when `applied`); `applied` whether
  the design plays anyway (false: refused, `errors` carries the message);
  `details: [{ source_rate, ratio, path, message }]` the engine's own
  refusal at each rate (the technical text); `fixes: [{ label, design }]`
  edits that build at every rate it plays, each a whole design: `Use
  automatic length`, `Set 847 taps`, `Set 212 taps per input sample`,
  `Widen the transition: pass to 0.850` (only when every rate is too short).
  Each fix is held to the check the daemon applies the design with (the
  searched length and the rational budget at every landing), and one that
  fails it is not offered.
  Each rate's verdict comes from trying other lengths with the same check
  (`design::explain`), so it covers every design kind's refusals.

  `plans: [{ source_rate, ratio, plan }]`, one per allowed rate where the
  design upsamples: its plan, or Recommended's at a rate in `notes`.
  `ratio` is where the engine would open it for that source rate with this
  target (the driver's rate policy and ratio ladder, which skips a rung the
  design does not build at; with a probe of the device, as
  `upsampler_response` resolves it, and a rate it does not offer there, or
  lands 1:1, has no plan). A `plan` (`DesignPlan`) holds `grid_up` (L)
  and `grid_down` (M), `grid_taps` with `grid_taps_exact` (false for an
  upper bound, such as a reflected intermediate phase, or an automatic
  length's estimate before its search), `workspace_bytes` (peak memory
  while it is built), `resident_bytes` (what the running filter keeps,
  both channels), and `cpu_load` (the fraction of one core the busiest
  thread spends on one pipeline on this machine, as the admission check
  prices it under the current config: both channels on one thread, or one
  channel with `channel_workers` on; omitted when it cannot be priced). `bounded_peak_db` (the largest output a full-scale
  input can produce) is only known once the kernel is designed, so only
  `upsampler_response` carries it.

A design refused by `set_upsampler`, `set_config` or `upsampler_response`
names the field the same way in its message
(`chain.upsampler.design.length: 1,279 taps is too short ... (At 44.1k ×16:
<the engine's own words>.)` for chain A's custom design; `chain.upsampler.design.length: at 44100 Hz (x8), ...` for a
preset, chain B or a measurement).

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
- Measurements are cached per (design, source rate, engine block): the
  newest always, older ones within 16 Mi samples, so zooming never
  re-measures.
- `latency_samples` (output frames) is the delay the live engine adds for
  this design at `source_rate`, what `status.filter_latency_ms` shows for a
  chain with no other delayed stage (no convolution, no limiter lookahead):
  the filter's own delay (linear phase: the kernel's centre; minimum and
  intermediate phase: the impulse peak) plus the partitioned convolution's
  block × the ratio. That block is the one the engine's upsampler runs at
  this source rate: the chain's largest block,
  `scaled_quantum(quantum, source_rate)` clamped to 512..8192 frames,
  rounded up to a power of two and capped at 1024 frames (×16 from
  44.1 kHz at the default quantum adds 512 × 16 = 8192 frames; from
  48 kHz up it is 1024 × the ratio). Other runtimes (block FFT, IIR) report
  their own delay at that block. The offline measurement's own partition
  block does not count.
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
- It runs outside the control lock; long kernels take a moment. The
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
    "Stereo image off"}`. A filter compare: `{"b": {"patch": {"upsampler":
    {"preset": "gaussian_long", "design": null}}}, "label": "Filter:
    gaussian_long"}`.
  - `label`: free text naming B, set by the client (omitted when unset).
  - `calibration_b` (optional): side B's own room calibration on the master
    in place of chain A's (`bundle_ab`); left out, B keeps chain A's
    calibration whatever `b` is ("Room calibration bundles").
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
  is `max(0, max(gain_a + P_a, gain_b + P_b) − max(0, P_a))`, capped at
  12 dB: A alone plays at its natural gain (the output headroom and the
  limiter handle its peaks), so only what B needs beyond A's own peaks is
  cut, and a B that peaks no higher than A costs nothing. `P` is the
  side's predicted sine worst-case peak gain (per channel, EQ `max |H|` with
  every preamp, negative when the EQ only cuts, plus convolution `max |H|`,
  certified between bins; a filter matrix output sums its row, "FIR
  matrix" above; the louder channel, times the crossfeed bound, then
  carried through the harmonics stage's peak bound), minus `output.headroom_db`
  (cut before the mix, so peaks it already took off need no more; an
  upsampler's magnitude is taken without the output headroom folded into
  its kernel). The comparison plays up to
  `|o| + H` quieter than chain A alone. Gains are static per comparison,
  never a moving AGC; a change ramps over 50 ms.
- **The analytic offset** integrates each side's power response over a
  20 Hz–20 kHz pink reference, K-weighted (BS.1770, as LUFS) or A-weighted:
  DC block, EQ (tilt and preamp included), crossfeed as a stereo stage at
  inter-channel correlation 0.5, the convolution IRs' magnitude (their FFT;
  a filter matrix's rows composed with the crossfeed and EQ before it, at
  that correlation of the sources)
  when B's upsampler design differs from A's, both upsamplers'
  magnitude (without the output headroom folded into a kernel), and the
  harmonics stage's loudness change, metered in LUFS on 4 s of loud pink
  noise (the compression on loud passages and the added harmonics), each channel's EQ and IR taken
  together. A built pair computes
  it from its own stages and IRs. Until the engine has built one (at
  start-up, while idle or with DSP off, before a reopen), status and the
  `ab_select` reply estimate the gains from the configs (once per chain
  pair, level and rate), with convolution by its `gain_db` only (a matrix
  item: times its largest row's sum of entry gains). A pair
  with a manual offset or a fixed headroom is never reported bit-perfect.
- `AbStatus`: `{ enabled, current, blind, offset_db, gain_a_db, gain_b_db,
  headroom_db, overs: {a, b}, level, measuring, measure_error, error }`.
  `current` is `"a"`, `"b"`, or `null` while a blind test runs (`blind`
  true, below).
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
    the previous config is still on its way, which rebuilds the pair).
  - **Cancelled** (`measure_error: "cancelled: …"`, the queued hops
    dropped): another track (the now-playing player and its
    `mpris:trackid`, reported as `now_playing.track_id`, or without one
    the player, title, artist and album; a pause is not a change), another
    graph or output rate, or a
    change to chain A or to the A/B config other than its level and label; A/B off
    ends it too. An `accept` after a track or rate change is `BAD_STATE`
    and locks nothing.
- **Blind tests.** `ab_blind {"enabled": true}` sets the blind mask, a
  daemon flag that is not persisted; `{"enabled": false}` clears it. The
  reply is `AbStatus`. The app runs the test (ABX or blind A/B), picks the
  hidden side and calls `ab_select` with the real side; the mask only keeps
  other clients from showing it. While it is set:
  - `AbStatus.current` is `null` and `AbStatus.blind` is `true`, in status
    and in every `set_ab`, `ab_select`, `ab_measure` and `ab_blind` reply;
    `overs` read 0 (they differ between the sides);
  - starting `ab_measure` is `BAD_STATE` ("a blind test is running"), and a
    measurement already running is cancelled when the mask is set, so the
    level stays locked; `cancel` still works;
  - the tray shows "A/B: blind" in place of the side items, the OSD shows
    no side, and `avdsp ctl ab …` and `ctl status` print the side as
    `hidden`.
  The mask clears on `ab_blind {"enabled": false}`, on any `set_ab`, and
  when A/B turns off by any route (`set_ab null`, a config or preset load
  without A/B). Without A/B, `ab_blind {"enabled": true}` is
  `INVALID_PARAMS`. CLI: `avdsp ctl ab blind on|off` (the mask only).
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

## Measurement

`measure_play` plays a WAV file straight to chosen output channels for an
external measurement tool (a room-correction sweep, a timing chirp): every
other stream is muted, auto-muting DACs are kept awake, and the reply and
`measure_status` say where and when it played. It works on the main
device and on followers, so a speaker on a follower is measured through
its real path.

```json
→ {"id":1,"method":"measure_play","params":{
     "file":"/home/me/stim.wav",
     "routes":[{"device":"master","channel":0},{"device":"master","channel":1}],
     "chain":"bypass","mute_others":true,"keepalive_db":-100.0,"lead_in_ms":2000}}
← {"id":1,"ok":{"run":3,"state":"preparing", ...MeasureStatus...}}
→ {"id":2,"method":"measure_status"}
→ {"id":3,"method":"measure_stop"}
```

**`MeasurePlayRequest`:**

- `file`: a WAV file on the daemon's machine: 16-, 24- or 32-bit integer
  or 32-bit float, at most 600 s. One channel plays on every route;
  otherwise the file has one channel per route, in route order.
- `routes`: `{device, channel}`. `device` is `"master"` (or the main
  device's configured name) or an enabled follower's device; `channel`
  counts from 0 and is the engine's channel (0 left, 1 right): on a device
  with `output_channels` it plays on `output_channels[channel]` ("Output
  channels"). Each pair at most once.
- `chain` (default `"bypass"`):
  - `"bypass"`: the chain's stages are skipped. On the main device the
    file is written after the chain, at the device's output rate, with
    the main volume's digital part (volume, balance, mute; a hardware
    volume stays on the hardware). The limiter, the output headroom and
    the chain's quantizer and dither are skipped: at unity volume the file's
    samples reach the device exactly. On a follower the file enters at its
    stage input (the drift loop's resampler, so it follows the master's
    clock like music does) and the stage's output goes to the device
    around the follower's chain, at the follower's `volume_db` alone (the
    master's level does not reach it, whatever its `follow_volume`: the
    file's samples stay exact). That needs the
    follower's device at the graph rate (×1); otherwise `measure_play`
    ends with an error naming the rate. The file may peak at **−3 dBFS**:
    nothing limits it, and the follower's resampler overshoots a step a
    little.
  - `"active"`: the file enters at the chain's input (on the main device
    after the alignment delay, on a follower at its stage input) and runs
    through every stage, the limiter and the quantizer: a verification
    run. The chain's limiter and headroom set the level, as for music,
    under the measurement ceiling (below): the main volume's digital part
    on the main device, and on a follower its level as for music
    (`level_db`).
  - **The ceiling.** While a measurement runs on a device (from its
    lead-in to its drain), nothing it sends that DAC goes above **−1
    dBFS**, either mode, whatever the limiter does: the file and the
    keepalive on a routed channel (after a follower's resampler, in
    `bypass`) and the device's whole output, music on other channels
    included. Clamped samples are counted (`clamped_samples`) and named
    in `notes`.
  - Either way the file keeps the device's timing: it starts where that
    device's path plays source frame `source_frame` (below), so the main
    device's alignment delay and a follower's `trim_ms`, lateness
    (`offset_ms`) and drift loop apply as they do to music. `bypass` skips
    the chain's latency (it is not in `latency_frames`); `active` includes
    it.
- `mute_others` (default `true`): the source (every stream on the DSP
  sink) is muted before anything else sees it, followers included, over a
  10 ms raised-cosine ramp, from the start of the lead-in until the file
  has left every DAC (or the run stops). A routed channel carries only
  the file and the keepalive, crossfaded over 10 ms (raised cosine) from
  what it carried at the lead-in's start and back to it after the file's
  end (or the stop fade's); with `false` the other channels keep playing.
- `keepalive_db` (default `-100`, `null`: none, `-140..=-60`): uniform
  noise at that RMS on every routed channel from the first lead-in frame
  to the file's last, under the file. It keeps DACs that mute after
  digital silence (some do after about 1 s) from swallowing the start.
  With it, the output is the file plus the noise; without, exact. Where
  it goes:
  - `bypass`: with the file, after the chain.
  - `active`: after the chain, on the routed channels (none at the
    chain's input): at the input a chain could take it down (a sub's
    low-pass leaves −100 dBFS noise near −124 dBFS) and let a DAC's
    auto-mute engage between sweeps; after it the routed channel carries
    the set level whatever the chain does. It runs from the device's
    first lead-in frame (10 ms raised-cosine ramp in) until the stimulus
    is over at the chain's input (10 ms ramp out from there), never past
    the measurement's last frame at the output, added to the chain's
    output and rounded back to the word the chain wrote, with no dither
    of its own (the noise dithers itself; at 16 bits the default −100
    dBFS is about half an LSB and plays at −100 ±1 dBFS).
- `lead_in_ms` (default 2000, `1500..=10000`): the keepalive alone before
  the file.

**Before it plays:** the daemon refuses (`INVALID_PARAMS`) a file whose
peak is above **−3 dBFS** in `bypass` or **−1 dBFS** in `active` (after
resampling too), a sample that is not a finite number, more than 64 M
samples (the file, and every device's buffers at their injection rates
together, checked before any is built), a bad route, a channel count that
does not match, or a value out of range; and (`BAD_STATE`) a second run
while one runs, DSP off, or varispeed on. It then starts a silent
PipeWire stream of its own into Audivane DSP ("Audivane measurement",
node `audivane_measure`), so the graph runs whatever else plays; the
engine leaves that stream out of its rate choice (the graph keeps the
rate the rate policy picks from the other streams). Before it arms, it
waits until, for 0.5 s together:
- the engine plays the DSP sink at a steady rate (up to 30 s);
- the multi-device timing has settled (`outputs.settling` is `null`: no
  follower opening or locking, routed or not, and the master's alignment
  and compensation holding still), up to 30 s from the request. Right
  after the daemon starts, a follower settling grows the master's
  alignment a few times, and each change would move every device;
- every routed follower is heard (`locked` or `unaligned`): one that
  stays `locking`, `muted` (disabled, its compensation pad shut), `lost`
  or `opening` for **10 s** (counted from when the engine plays, and again
  from each alignment change, since the followers re-acquire then) ends
  the run in `error`, naming the follower and its state
  (`follower <device> is not heard after 10 s: it is locking (muted until
  its drift loop locks)`).

Every wait ends at 30 s from the request, whatever holds it (a rate or ratio
that keeps changing, a follower whose 10 s keep restarting with alignment
moves): `the engine was not ready within 30 s: <what it waited for>`.
The wait varies: on the test rig it was about 10 s when the run started the
graph (the followers reopen and lock, the alignment settles) and 15 s right
after the daemon started. A tool that captures alongside should find the
stimulus by its own timing over the whole capture (or from
`start_time_ns`), not in a fixed window after it called `measure_play`.

Where the file's rate
differs from a device's injection rate (the output rate for `bypass` on
the main device, the graph rate otherwise) it is resampled offline for
that device (a `notes` entry says so). The audio threads then take the
job; the main device's thread arms on its next block and picks the
start. The job carries the graph rate, each device's rate and the main
device's name it was built for: a device whose thread finds another
refuses to play it and the run ends in `error`.

**`MeasureStatus`** (`measure_play`, `measure_status`, `measure_stop`):

- `run` (counts runs since the daemon started; 0: none), `state`:
  `idle`, `preparing`, `lead_in`, `playing`, `tail` (handed to every
  device, or stopping; waiting for it to leave the DACs before the mute
  lifts),
  `done`, `stopped`, `error` (with `error`).
- The request's `file`, `chain`, `mute_others`, `keepalive_db`,
  `lead_in_ms`; the file's `file_rate_hz`, `file_frames`, `duration_s`;
  `progress_s` (what every device has played); `graph_rate_hz`.
- `source_frame`: the source frame (the engine's count of graph frames)
  file frame 0 plays at on every device.
- `devices[]` (the main device first): `device`, `role` (`master` or
  `follower`), `channels`, `rate_hz` (its output rate),
  `injection_rate_hz`, `resampled`, and once the file started:
  - `latency_frames` and `latency_us`: from the injection point to the
    DAC, device frames: `chain_latency_frames` (`bypass`: 0 on the main
    device, the stage's delay on a follower; `active`: the chain's latency,
    plus the stage's on a follower, and a follower's compensation pad,
    "Multi-device output"; the master's compensation is in its alignment
    delay, before the injection point) plus `queue_frames` (the device queue,
    the engine's buffers and ALSA's delay, read once after the start);
  - `start_frame`: the engine's output frame (its count for this device)
    that carries file frame 0 (fractional on a follower, rounded);
  - `start_time_ns`: when that frame leaves the device queue
    (`CLOCK_MONOTONIC_RAW`, from the ALSA status timestamp), so devices
    can be compared with each other and with a capture timestamped on the
    same clock. The DAC's own delay is not in it: from a USB DAC's line
    out into a USB audio interface, a timing chirp arrived 3 to 4 ms after
    it (the DAC's and the ADC's filters and USB buffering together, the
    capture timestamped as the engine's).
  - `frames_played`: file frames handed to the device's injection point
    (what the engine fed it), not what its DAC played: a run whose DAC
    lost frames still counts them all;
  - `clamped_samples`: samples clamped at the ceiling on this device;
  - `drained`: its DAC played the last frame the measurement touched;
  - `continuity` (`null` until the run ended, or with fewer than three
    readings), the playback continuity check: the device's DAC position
    (frames written minus the ALSA delay, at the status timestamp,
    `CLOCK_MONOTONIC_RAW`), recorded at most every 2 ms from its first
    lead-in frame until it drained, against its clock as the host sees it
    (the rate taken out as the median slope over 8 readings, so a step or
    a stray does not pull it):
    - `readings`;
    - `jitter_us`: the readings' timing jitter (a spread of neighbouring
      differences that ignores strays);
    - `strays`: readings off the running median of their neighbours by
      more than 1 ms (and 8 times the jitter): a late timestamp, left out
      of the residual and never read as a step;
    - `max_residual_frames`, `max_residual_us`: the farthest reading from
      a slowly drifting clock (a quadratic through the readings, the
      strays left out);
    - `jump_frames`, `jump_us`, `jump_at_s`: the largest step in the
      position, the difference between the medians of the 8 readings on
      either side (so it must hold across most of them; positive: the
      position ran ahead), and when (the largest neighbouring difference
      there), seconds from the file's first frame at the DAC (negative:
      in the lead-in);
    - `dropped`: readings the buffer had no room for (sized for the
      lead-in, the file and 62 s after; the check covers the run up to
      there, and a note says so).

    A step (or, without one, a residual) above 1 ms and 8 times the
    jitter adds a note (`<device>: its playback
    position jumped by <N> frames (<µs> µs) at <t> s (from the file's
    start): the timeline after it is shifted`); the run still ends `done`.
    It sees a step the host can see (frames the driver reports lost or
    repeated, a reading that moves against the clock); a drop inside the
    DAC, after what the driver reports, stays invisible to it.
- `notes`: what was done to play it (resampling, the ceiling, clamps, a
  step in a device's playback position).

**Ending.** Each device tracks the last output frame the measurement
touched: the injection's end (after the crossfade back), the processing
after it (a follower's resampler tail, the chain's tail in `active`; in
`bypass` a follower keeps its stage output around the chain until the
stage has passed the file, then crossfades back to the chain over 10 ms),
and its device queue. A device is drained once its DAC has played that
frame (from the ALSA delay). The run ends `done` once every device has
drained, then the mute ramps back. `measure_stop` (it waits for the end
and returns the final status) fades the file out over 5 ms, waits for
every device to drain the same way, then lifts the mute: the run ends
`stopped`. A device that does not drain within its whole tail after the
injection (the chain's, a convolution's included), its queue and 2 s
ends the run in `error` naming it (the mute lifts anyway); so does a
fault, also when a stop was asked for. It ends in `error`, the same way,
when the engine leaves the DSP path (Audivane
Direct takes over, DSP off), the graph or device rate changes, the
master's alignment steps (an alignment or compensation change moves
every device), an underrun or a reopen interrupts it, a routed follower
loses its lock or steps (its trim or lateness), an `active` run's routed
follower is shut (its compensation pad, when its chain or the master's
compensation moves: `a routed follower was muted while it played the
file`, so a silent capture never ends `done`), a routed device's chain is
swapped while the run plays there (`active`: any swap, since the file
runs through it; `bypass`: a swap that changes the chain's latency; the
main device also waits to arm while a swap fades), a channel worker of
the main device's chain misses its deadline while an `active` run plays
there (the chain plays silence, then resets; `bypass` skips the chain),
varispeed or Keep pitch starts, or playback stops making progress for
2 s (a lost device). The daemon
exiting ends it too: the mute lives on the audio thread only.

**Real time.** The job (each device's samples at its injection rate) is
built on the control side and handed over through a lock-free slot: an
audio thread swaps the posted box out and hands the one it held back
through an outbox, which the control side frees; a job a stalled thread
still holds is dropped later, off the audio threads. Nothing on the audio
threads allocates, locks or frees (`tests/processor_no_alloc.rs`).

**Known limits.** On a follower in `bypass` with `mute_others` off and a
chain with latency, the routed channel's music dips for the chain's
latency when the tap hands back to the chain. A worker that stalled
just before the run armed (the chain still silent, no new late block) is
not caught. The main device
changing between the check and the run's start (two commands racing)
is not caught there: the run plays on the new one.

**CLI:**

```sh
avdsp ctl measure play stim.wav --route master:0 --route master:1   # waits, prints the timing
avdsp ctl measure play sub.wav --route hw:CARD=USB_1,DEV=0:0 --active --keepalive-db -110
avdsp ctl measure play stim.wav --route master:0 --json            # the final MeasureStatus
avdsp ctl measure status
avdsp ctl measure stop
```

`--route DEV:CH` takes the channel after the last colon. Other options:
`--no-keepalive`, `--lead-in-ms`, `--no-mute`. `play` exits 1 when the run
ended early (`stopped`, `error`).

## Upsampling CLI

Examples:

```sh
avdsp ctl upsample --preset recommended --max-rate 768000
avdsp ctl upsample --preset gaussian_long --ratio 4
avdsp ctl upsample --design my-filter.json --ratio 4
avdsp ctl upsample --manual --ratio 8 --auto-taps-match-output --passband-end 0.9 --stopband-start 1
avdsp ctl upsample --passband-end 20k     # pass to 20 kHz at every source rate
avdsp ctl upsample --auto-taps-db 140 --phase minimum
avdsp ctl upsample --manual-taps --taps 4097 --beta 14 --cutoff 0.95
avdsp ctl design resolve gaussian_long > my-filter.json
avdsp ctl design check my-filter.json --ratio 8 [--rate 44100]
```

`design resolve` prints a preset's expanded design (a file for `--design`).
`design check` runs `check_design` on a file (`-` reads stdin), with
`--ratio`, `--max-rate`, `--max-rate-any` or `--out-rate` in place of chain
A's target; it prints each refusal, the advice when the design is applied
with fallbacks, each rate that plays Recommended and why, the fixes and the
engine's detail per rate, then each rate's plan, and exits 1 when the
design is refused. `status` prints `upsampler: plays Recommended (...) at
44.1 kHz instead of the requested filter` while the rate playing falls back,
and the per-rate line lists every rate that does.

`upsample --passband-end` takes a fraction of the Nyquist (`0.908`) or a
frequency (`20k`, `20kHz`, `20000Hz`; a plain number above 1.5 is Hz too).
A frequency sets `passband_end_hz` and keeps the band's fraction as the
fallback; a fraction clears the frequency. Editing other flags keeps a
design's frequency.

`--preset` (accepting `"recommended"` and all 68 `FilterPreset` names),
`--design <FILE>` (loading a `FilterDesign` JSON file), and `--manual` are
mutually exclusive. `--auto-taps-db`, `--auto-taps-match-output`, and
`--manual-taps` are mutually exclusive and edit a `standard` `windowed_sinc`
design.

## Tests

How to run the daemon and IPC/CLI suites (and what they cover):
[DEV.md](../DEV.md#build-and-test).
