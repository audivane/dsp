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
                                  └── Events: meters @30 Hz; status/streams/now_playing/config/presets on change
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
  validation), `INTERNAL` (-32603), `BAD_STATE` (1), `IO` (2).
- `subscribe` takes any subset of `meters`, `status`, `streams`,
  `now_playing`, `config`, `presets`, `spectrum`. The server sends a `status`/`streams`/
  `now_playing`/`config`/`presets` snapshot immediately after the reply, then
  meters at 30 Hz and the others only when they change. Each connection has a
  bounded outbound queue (256 lines); a client that stops reading is
  disconnected instead of stalling the daemon.
- `config` events carry the whole `EngineConfig` after any change from any
  client (including preset load/save, which update `active_preset`); `presets`
  events carry the sorted preset list after a create, delete, rename, import or
  save. Both are detected with generation counters (a new subscriber may see
  one duplicate event on the first pump tick).
- Requests are answered in order on their connection; clients may pipeline.
  `shutdown` replies first, waits ~20 ms and only then stops the daemon.

`avdsp ctl` builds exactly these lines. `avdsp ctl watch --json` prints event
lines verbatim, which is handy for scripting. Without `--json`, `watch` prints
human-readable text: meter lines update in place on a terminal and become one
newline-terminated line each when stdout is redirected; status, stream,
"now playing", config and preset events are always printed as their own lines.
`watch <topic>` accepts `meters`, `status`, `streams`, `now-playing`, `config`
and `presets`; without a topic it subscribes to all of them.

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
  `S32, S24, S24_3, S16`.
- An empty `device` is filled with the first `/proc/asound/pcm` playback
  device that is not HDMI (`Loopback` cards are only used as a last resort).
  Nothing is ever opened just to pick a default.
- Validation (`control::config::validate`) is shared by every mutation:
  `hw:` device, non-empty formats, power-of-two quantum 16..=8192, rates from
  the supported list with `default_rate` included, taps 1..=4 000 000,
  Kaiser beta 0..=40, cutoff in (0, 1], ratio 1..=64, bands ≤ 4096 per channel
  with sane frequency/Q/gain (applied to both left and optional `right` EQ),
  every enabled convolution IR file must exist (combined enabled length capped
  at 4 000 000 taps in the engine), A/B side B must resolve to the same
  upsampling ratio at every allowed rate and the same output format/dither as
  side A, manual level offsets within ±60 dB,
  `volume.volume_db` within −120..=0 dB, `volume.balance_db` within ±24 dB.
- Chain B is **normalized from chain A before validation** on every edit
  (`SetUpsampler`, `SetConfig`, `SetDspEnabled`, `SetEq`, `SetConvolution`,
  `SetRoutingMode`, `SetVolume`, and when the file is loaded), so changing chain A never
  fails because chain B still carries the old upsampler/output. `ab set` with
  a chain B that differs is realigned rather than rejected: the reply is an
  `AbStatus` plus a `"notes"` array explaining what followed chain A (`avdsp
  ctl` prints each note as `note: …`). A config file with a stale chain B is
  normalized (with a warning) instead of being treated as corrupt.
- `active_preset: Option<String>` records the preset chain A was last
  saved to or loaded from. It is part of the config, not of a preset.

### Rate policy

```json
"rate": {
  "follow_source": true,
  "allowed_rates": [44100, 48000, 88200, 96000, 176400, 192000],
  "default_rate": 48000,
  "conflict": "keep_current"
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
  page toggles rates and sets the default through `set_config`;
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

#### Device capabilities

Each time the engine opens a device (start, device switch, re-acquire after
a release; not on rate switches) it probes it on the handle it is about to
configure (no second open, so a device it holds is never `EBUSY`): for the
engine's channel count and interleaved access, every (rate, format) pair over
every allowed graph rate times every integer ratio up to the maximum DSP
ratio (16), capped at 768 kHz, plus the significant bits ALSA reports per
pair (`snd_pcm_hw_params_get_sbits`, e.g. 24 for a 24-in-32 S32 DAC). Formats
are probed per rate because USB devices often tie them together (24-bit only
up to 96 kHz). The probe leaves the device unconfigured.

- A graph rate `r` is playable when the device takes `r × k` with one of
  `formats` for some `k` on the ratio fallback ladder, from the wanted
  upsampling ratio down to 1 (halving powers of two, otherwise minus one).
- Effective rates = `allowed_rates` ∩ playable. If that is empty: every
  playable rate of the engine's full list (44.1k..768k); if still empty, the
  open fails with an error naming the parameter the device rejects and what
  it does take (unfiltered by the engine's channel count and rate list):
  `device 'X' needs 2 channels; device supports 1 (engine: channels 2;
  device: rate 48000 Hz; formats S16; channels 1)`, `device 'X' supports
  32000 Hz only, which the engine doesn't run at (device supports rate 32000
  Hz; formats S16; channels 2)`, or `takes none of the engine's sample
  formats`.
- **Audivane Direct never upsamples** (it is the bit-perfect path, ×1), so it
  only runs the graph at *Direct rates*: the effective rates the device plays
  natively (`Status.direct_rates`). A Direct stream at a rate the device only
  takes upsampled (e.g. 48 kHz on a 96 kHz-only DAC whose 48 kHz graph the
  DSP sink plays ×2) moves the graph to a Direct rate (the stream's rate's
  next higher Direct rate, else the closest; with `follow_source` off, the
  nearest Direct rate to `default_rate` when that is not native) and PipeWire
  resamples the stream: playback continues, and `passthrough` reports
  `resampled` (not bit-perfect). When no effective rate is native, Direct
  cannot play on the device: routing a stream to it puts the engine in
  `error` (`Audivane Direct cannot play on 'X': it does not upsample, and
  none of the allowed rates (…) is native to the device …`).
- A failed reopen never sticks: when the route or the graph rate changes
  afterwards (e.g. the app goes back to Audivane DSP) the engine retries the
  open, so a playable path recovers without a config apply.
- On open the ratio is the first ladder step the device takes, with the
  first of `formats` it takes at that rate. There is no "nearest rate"
  fallback: the DAC always runs at exactly graph rate × ratio. If PipeWire
  still has the graph at a rate the device cannot play (e.g. right after a
  device switch), the engine opens at the effective rate it then forces.
- A config change alone (e.g. a new filter length) never reopens the device
  when the device already fell back to a smaller ratio for the same request.
- ALSA errors say which parameter the device rejected and what it supports,
  e.g. `device 'hw:CARD=V3,DEV=0' does not support rate 44100 Hz (device
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

This changes error semantics slightly: a failing write can no longer fail the
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

### Chain wire shapes (EQ & convolution)

- **Per-channel EQ (`EqProfile`):** `EqProfile` has an optional
  `right: Option<ChannelEq>` (`{ "preamp_db": f64, "bands": [...] }`), omitted
  when `None`. `None` links all channels to the profile's top-level
  `preamp_db`/`bands`; `Some(right)` applies the top-level set to channel 0
  (and channels ≥ 2) and `right` to channel 1. Analytic A/B level matching
  uses the mean power response of left and right.
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


## Volume, balance, and mute

- Persisted in `EngineConfig.volume` (`VolumeConfig { volume_db, balance_db,
  muted }`, `#[serde(default)]`, defaults `0.0, 0.0, false`).
- `set_volume` (`Call::SetVolume { volume_db, balance_db, muted }`) updates,
  validates (`-120..=0` dB volume, `±24` dB balance), persists, and returns a
  `VolumeStatus { volume_db, balance_db, muted, hardware, min_db, max_db }`.
  `Status.volume` (`#[serde(default)]`) reports the current `VolumeStatus`.
- **Hardware volume first:** on the ALSA card of the configured `hw:` device,
  the builder thread looks for a simple mixer element with playback volume in dB
  (`Master`, `PCM`, `Headphone`, `Speaker`, then the sole playback-volume
  element). When found, master attenuation (and mute if the element has a
  playback switch) is applied in hardware (`hardware: true`, card dB range in
  `min_db..=max_db`), leaving the digital signal bit-perfect, and re-applied
  after every device reopen (rate switch, acquire) and device change.
- **Digital fallback & balance:** when no usable hardware mixer exists, master
  volume is applied in `f64` on the **DSP path only** immediately before final
  quantization/dither. Channel balance (`balance_db < 0` attenuates the right
  channel by `|b|`; `> 0` attenuates the left channel) is always digital and
  DSP-path only. Mute uses the hardware switch when present, otherwise a ramped
  digital mute on both DSP and Direct paths. All digital gain/mute transitions
  ramp smoothly over ~10 ms with lock-free RT updates and no RT allocations.
- The **Direct** (`audivane_direct`) path never applies digital volume or
  balance, staying bit-exact whenever unmuted. On the DSP sink, non-zero
  digital volume or non-zero balance marks the engine `dsp_active = true`
  (`PassthroughBreak::DspActive`), whereas pure hardware volume with centered
  balance does not.
- CLI: `avdsp ctl volume [DB] [--balance DB] [--mute|--unmute]` queries
  `status`/`volume` when given no arguments, or merges supplied changes via
  `get_config` + `set_volume`.

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
  else (e.g. a hot-plugged device), at most every 500 ms.
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
2. `DspActive`
3. `Upsampling { ratio }`
4. `Volume { node, volume }` (sink first, then streams by node id; volume ≠ 1.0
   or muted)
5. `Mixing { streams }`
6. `Resampled { stream_rate, graph_rate }`
7. `ChannelMix { stream_channels, sink_channels }`
8. `SourceOver24Bit` — only when the source precision is actually *known* to
   exceed 24 bits, i.e. the player reported it (MPRIS `native_bits`) for the
   single stream feeding the sink
9. `Truncated { source_bits, output_bits }` (DAC format narrower than the
   widest *known* source)

A negotiated PipeWire format is a container width, not a source precision:
`pw-play` opens a 24-bit WAV as `S32LE`, and an `F32LE`/`F64LE` bus says
nothing about the file behind it. Such streams get a **caveat** instead of a
break: `Passthrough::caveats` carries `UnknownSourcePrecision { format }`
(`"S32LE"`), `intact` stays true, and `avdsp ctl status` renders e.g.
`passthrough: intact (source precision unknown: S32LE container)`. `caveats`
is `#[serde(default)]`, so older clients keep working; they are not breaks and
never appear in `broken_because`.

`intact` is simply "no breaks". The output word length is the negotiated ALSA
format in the engine snapshot (e.g. `S32`); the dither target in
`chain.output.bits` is not part of the snapshot, so a 24-bit source dithered to
16 bits inside a 32-bit container is not reported as truncated.

## Output format and dither

The quantizer dithers to `Status.dither_bits` = `min(chain.output.bits,
container bits of the negotiated format)` (S16 → 16, S24/S24_3 → 24, S32 →
32), for chain A and B, rebuilt whenever a reopen negotiates another format.
Format choice is not strict (the first of `formats` the device takes at the
rate), so "Dither to 24" on a device that only takes S16 dithers to 16 bits
instead of letting the S16 packer truncate the low 8 bits.

## MPRIS

`control::mpris` polls the session bus every 500 ms with `zbus::block_on`
(no tokio). It lists `org.mpris.MediaPlayer2.*`, reads `PlaybackStatus` and the
`Metadata` property, and keeps the most recently *started* `Playing` player as
the current one. A paused current player stays current (the UI keeps the last
track); if it leaves the bus the next most recent player takes over, and with
no history any readable player is shown.

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
  response. Validation covers every section; only enabled sections count as
  active.
- APO / AutoEQ import fills the linked section (`Channel:` directives warn).
- `avdsp ctl eq`: `--section linked|left|right` on band edits, and
  `avdsp ctl eq section <linked|left|right> on|off|clear`.

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
- `clipped` (total) and `clipped_ch` (per channel): samples clipped since the
  daemon started.

`Status.filter_latency_ms` is the filter-delay part of `latency_ms`
(upsampler + convolution in the active chain; 0 on Direct or with DSP off).

## Status: device and rate fields

All `#[serde(default)]`:

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
- `direct_rates: number[]`: the effective rates the device plays natively,
  the only ones Audivane Direct runs at; empty when no device is open or
  none is native.
- `effective_default_rate: number | null` (`null` like `effective_rates`).
- `format: SampleFormat | null` and `dither_bits: number | null`: see "Output
  format and dither"; `null` while no format is negotiated, including in
  `error` after a failed open or switch: status never shows the previous
  device's negotiation.
- `normal_output: string | null`: `node.description` of the session default
  sink when it is not one of Audivane's (where a stream routed to `default`
  plays in per-app routing). `null` in system-wide routing, where the default
  is Audivane DSP, and when there is no other output. The UI offers
  `default` only in per-app routing, as "Not through Audivane (<output>)".
- `ratio` / `output_rate` are what the device was opened at, so a device
  that cannot take the wanted ratio shows the fallback (e.g. `ratio: 1`).

`avdsp ctl status` prints them as `rates:`, `direct:` (only when it differs
from `rates:`), `device can:` and `dither:`.

## Spectrum

`spectrum` events arrive at about 20 Hz and are computed only while some
client is subscribed. The RT thread copies samples into preallocated
wait-free rings (dropping when full: no allocation, no lock); a non-RT thread
analyses them. Each tap (`pre` = chain input, `post` = chain output before the
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
- With upsampling off the reply is flat (`ratio 1`, impulse `[1.0]`).
- It runs outside the control lock; very long kernels take a moment.

## A/B, self-test and device release

- `set_ab` configures chain B (or disables A/B with `null`). B's upsampler
  and output always follow chain A; differing values are replaced, and the
  reply's optional `notes` say what was realigned. Both chains run all the
  time; `ab_select` switches with a crossfade (default 10 ms). The level
  offset (`AbStatus.offset_db`) is static per comparison, never a moving AGC.
- `self_test` plays a known signal through the Direct path and compares an
  FNV-1a hash of the expected left-justified samples with the bytes handed to
  ALSA (`SelfTestResult`).
- `release_device` gives the DAC back to WirePlumber and idles the engine;
  `acquire_device` takes it again. Both complete only after the hand-off has
  actually happened (see [pipewire-driver.md](pipewire-driver.md)).
- `list_streams` returns the current playback streams (`StreamInfo`), the
  same data the `streams` event carries.

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
