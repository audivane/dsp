# Audivane DSP desktop app

Tauri 2 + React 19 + TypeScript + Vite. It controls the `audivane-dsp` daemon
over its Unix-socket control plane ([docs/control-plane.md](../docs/control-plane.md)).

```
app/
  src/                 React frontend
    api/generated/     TS types generated from crates/protocol (ts-rs), do not edit
    api/types.ts       the names the UI imports (re-exports of api/generated)
    api/mock.ts        in-memory daemon for the browser, Playwright and vitest
    api/tauri.ts       real transport (Tauri commands + event Channel)
    store/             app state (useSyncExternalStore), meter buffer
    lib/               EQ maths, ParametricEQ.txt, passthrough wording, throttle, history
    views/, components/
  src-tauri/           Rust side: daemon client, tray, systemd start, window
  scripts/             type generation, screenshots, interaction smoke test
```

## Develop in a browser (mock daemon)

```sh
cd app
npm ci
npm run dev            # http://127.0.0.1:1430
```

Without a Tauri runtime the app uses `MockDaemon` (`src/api/mock.ts`): realistic
config, presets, streams, devices, A/B, volume, simulated meters at 30 Hz, with
the daemon's semantics for presets (BAD_STATE on name conflicts, EQ-only
imports), devices and volume validation. URL switches: `?engine=down` (engine-not-running
screen; Start engine "starts" it), `?split` (L/R split EQ), `?apps`
(Selected-apps routing), `?overlays=demo` (adds a built-in target next to the
simulated room measurement the mock always has), `?digital` (a DAC without a
hardware mixer: digital volume), `?device=V3` (a 48 kHz / S16-only USB speaker, so rates, formats, ratio and dither are capped), `?device=USB` (a hi-res DAC
that also plays 1.4112 / 1.536 / 2.8224 / 3.072 MHz in S32, for ×32 and ×64), `?legacy_ratio` (an engine from before fractional rates: no
`rate_ratio`, integer rate targets only), `?legacy_streams` (an engine
without `set_stream_volume`: stream volumes disabled), `?followers=USB,SPK`
(start with those cards as "Also play on" devices, already in sync; SPK
plays 42 ms late, the unaligned case). In the
browser console
`avdsp` is the store.

## Checks

```sh
npm run typecheck      # tsc
npm test               # vitest: store, EQ maths vs dsp-core, APO import/export, passthrough text,
                       #   throttle, overlay parsing/resampling, slider tapers
npm run build          # tsc + vite build into dist/
node scripts/interact.mjs test-results/interact   # Playwright smoke test of the built app (drag, wheel, undo, menus…)
npm run screenshots -- --views eq,conv,ups,out,routing,ab,device   # into test-results/screenshots/
                       #   effect views: dc_block,pitch,crossfeed,harmonics,limiter
```

Regenerate derived files when the protocol or dsp-core changes:

```sh
npm run gen:types      # ts-rs export from crates/protocol (feature `ts`) into src/api/generated
npm run gen:eq-fixture # reference EQ responses from dsp-core for eqmath.test.ts
npm run gen:targets    # AutoEq target curves (network) into src/assets/targets
```

## Rust side

`src-tauri` (package `audivane-dsp-ui`, binary `audivane-dsp-ui`) is a member of
the Cargo workspace but not a default member. Everything that needs GTK or
WebKitGTK (window, tray, plugins) is behind the `gui` feature, so on a machine
without those headers these still work and include the UI crate's pure tests
(daemon client, socket path, EQ, IR header, tray text):

```sh
# from the repository root
cargo nextest run --workspace
cargo test -p audivane-dsp-ui
```

The GUI build needs `webkit2gtk-4.1` and `gtk3`, and
a built frontend in `app/dist` (it is embedded at compile time):

```sh
cd app && npm ci && npm run build
cargo build -p audivane-dsp-ui --features gui          # debug binary
npx tauri build --no-bundle                            # release binary (enables `gui` via tauri.conf.json)
cargo build --release -p audivane-dsp-ui --features custom-protocol  # same, without the tauri CLI
npx tauri dev                                          # live reload against the real daemon
```

### In the rootless Arch build root

On a machine without WebKitGTK headers, build inside `~/.cache/audivane-archroot`
(see `packaging/arch/archroot.sh`); use a separate target dir so host and root builds stay isolated:

```sh
R=~/.cache/audivane-archroot/enter.sh
W=$PWD   # audivane-dsp checkout
$R bash -lc "cd $W/app && npm ci && npm run build"
$R bash -lc "cd $W && CARGO_TARGET_DIR=$W/target/arch cargo build -p audivane-dsp-ui --features gui"
$R bash -lc "cd $W && CARGO_TARGET_DIR=$W/target/arch cargo clippy -p audivane-dsp-ui --features gui --all-targets"
$R bash -lc "cd $W/app && CARGO_TARGET_DIR=$W/target/arch npx tauri build --no-bundle"
```

## How it talks to the daemon

- **Requests:** one socket connection shared by all calls, with ids and
  pipelining (`client.rs`). The webview calls `daemon_call({method, params?})`;
  presence of `params` is kept (`set_eq` with `null` clears the EQ). Lost
  connections are reopened on the next call.
- **Events:** a second connection sends `subscribe` for `meters, status,
  streams, now_playing, config, presets`; a daemon that refuses the newer
  topics gets the original four. It reconnects with backoff (0.25 → 3 s) and
  reports connecting / up / down. Events go to the webview over a
  `tauri::ipc::Channel`; the latest of each (except meters) is replayed when
  the webview reloads. Meters (30 Hz) are the one exception to "everything is
  delivered": they are not forwarded while the window is minimized, and
  while it is closed to the tray the `meters` topic is dropped altogether
  (a silent resubscribe; `spectrum` likewise), so the daemon sends none.
  At most one frame is in flight at a time. The webview
  acknowledges each frame on its next animation frame (`meters_ack`); frames
  arriving meanwhile replace each other, so a suspended webview cannot build
  a backlog. Status, streams, config and preset events are never dropped.
- **Socket path:** `$AUDIVANE_DSP_SOCKET`, else `$XDG_RUNTIME_DIR/audivane-dsp.sock`,
  else `/tmp/audivane-dsp-<uid>.sock` (same order as the daemon).
- **Start engine:** `StartUnit("audivane-dsp.service", "replace")` on the
  systemd user manager over the session bus (`zbus`).
- **The daemon is the authority.** The UI renders what the daemon echoes: after
  a change it re-reads `get_config` (and also follows `config` events). The only
  local copies are short-lived drafts that keep an EQ drag or a volume drag
  smooth; they are sent at most ~25 times a second and the final value always
  goes out on release.
- **One change at a time.** Every change the UI makes goes through one queue
  (`lib/queue.ts`) and runs only after the previous one has finished and its
  config has been re-read. A change is an intent (`config → config'`), applied
  when its turn comes to the config read with `get_config` right then, not to
  what the UI showed when it was clicked. The targeted calls (`set_eq`,
  `set_convolution`, `set_upsampler`, `set_dc_block`, `set_pitch`,
  `set_crossfeed`, `set_harmonics`, `set_limiter`, `set_volume`, `set_dsp_enabled`,
  `set_ab`, `set_routing_mode`, `set_output`, `set_formats`, `set_rate`,
  `set_device`, `set_stream_volume`) are used wherever they exist; only the volume settings go
  through `set_config`. Live
  drags keep at most one send waiting, which always carries the newest value.
- **Live check:** `examples/probe.rs --exercise IR1.wav IR2.wav` runs every
  call the UI makes against a running daemon (use a test daemon with its own
  `--socket`/`--config`) and checks replies and `config`/`presets` events.
- **Older engines:** optional calls (`list_presets`, `list_devices`, `set_volume`,
  `right`/IR-chain support) degrade gracefully: missing support disables that
  control with a plain explanation instead of failing.

### Mappings worth knowing

- **Effect stages** (DC block, pitch, crossfeed, harmonics, limiter) each
  have a sidebar tab and a view (`views/DcBlockView.tsx` …), and are edited
  with their targeted call through `AppStore.editStage`: a draft while a
  slider moves (~25 sends a second at most, the final value always goes
  out), the daemon's echo replaces it, and a refusal is shown next to the
  controls (`InlineError`). Their power buttons keep settings: `enabled:
  false` for DC block, pitch, crossfeed and harmonics; the limiter's mode is
  remembered locally like the upsampling ratio, and its Off mode goes
  through the same path, so either control brings the mode back (auto
  headroom when none is remembered). When the daemon's clip count rises
  while the limiter is off, `clipNoLimiter` shows a warning with "Turn on
  limiter" (auto headroom) on the Limiter and Output views and in the
  sidebar line (`components/ClipWarning.tsx`). The held automatic headroom
  shows next to the volume in the top bar and the tray popup ("−1.5 dB
  held", `components/HeldHeadroom.tsx`, from `Status.limiter_held_db`, so
  the popup shows it with the main window closed and no meters). An
  optional 4th or 5th harmonic remembers its level the same way. The status
  bar at the bottom is the single place for the bit-perfect / DSP active
  state. The Pitch page has one switch for pitch shift and varispeed
  (`AppStore.setPitchPageEnabled`: a `set_pitch` and a `set_varispeed`, only
  for the ones that change). It shows on when either is on, in the view and
  the sidebar, and reaching 0 cents or 1× keeps it on ("On · no shift").
  Varispeed runs even at 1×, so on is not bit-perfect. The
  shared maths the views show (crossfeed level and response, THD, tilt
  target, dither guidance) is in `lib/effects.ts` and `lib/tilt.ts`.
- **Tilt** is part of the EQ (`EqProfile.tilt`, `set_eq`): `lib/eqmath.ts`
  adds its target response to every channel, so the curve and the auto
  preamp include it. Both are estimates from the ideal target, and the view
  says so (the engine's design is within 0.1 dB of it away from the
  corners, 0.16 dB inside them). The sliders are bounded by the current
  band (both the ±6 dB/oct and ±24 dB limits hold), and switching between
  slope and total keeps full precision.
- **Volume settings** (mode, DAC level, ceiling, desktop slider, guard) use
  `get_config` + `set_config`, with the level, balance and mute left out of
  the payload (the top bar's guarded writes own them). Each box checks the
  engine's bounds (`lib/volumeSettings.ts`) and shows the reason beside it
  before anything is sent. An engine without the volume guard shows an
  update notice instead. "No rise limit (only the cap)" sends
  `max_rise_db_per_s: 0` and disables the rate box; turning it off restores
  the last non-zero rate (remembered locally, else 6 dB/s).
- **Stream volumes** in Routing are dB sliders (0 dB = 100 %, down to
  −120 dB) with a mute button, one per stream, written with
  `set_stream_volume` (store `setStreamLevel`): an optimistic draft while
  dragging, throttled to ~25 sends a second per stream, the engine's answer
  (the level now playing) replacing the draft. Only dB is shown, because
  the desktop mixer's percent is on a cubic scale. When the engine holds
  the level lower (the app stream cap, or the app's own volume), the note
  under the slider says why; a refusal shows inline with the daemon's
  message. A stream that is not playing through Audivane is shown but
  disabled, and an engine without the call (its streams carry no
  `volume_db`, or the call is an unknown method) disables the sliders with
  an "update the engine" tooltip.
- **A/B quick compare** (`lib/abCompare.ts`, `views/AbView.tsx`). Each
  stage header has a `CompareButton` (`components/StageParts.tsx`) that
  builds side B as a merge patch on chain A, like `avdsp ctl ab stage`
  (the stage off when A runs it, on when A has it set up but off; EQ "on"
  uses the EQ remembered when it was switched off), and sends `set_ab`
  keeping the level and headroom settings. The same patch again is "Stop
  compare" (`set_ab null`). The Upsampling view's `FilterCompare` sends
  `{"upsampler": {"preset", "design": null}}` like `ab filter`. The top
  bar names B and the backquote flips (`AppStore.abFlip`). The A/B view
  shows `status.ab`'s offset, gains, headroom and overs; latency is
  `status.filter_latency_ms` (the pair is aligned to the slower side). An
  overload fallback (`status.overload`, any non-varispeed step while
  `config.ab` is set) means A/B is off, shown in the view, the top bar and
  the sidebar. "Match to this track" (`AppStore.abMeasure`,
  `lib/abMeasure.ts`) appears only when the engine's A/B status carries
  `measuring`: it sends `ab_measure` (start, accept as "Use now", cancel)
  and draws the run from `status.ab.measuring` (progress over the 20 s
  minimum plus the 10 s settling window, the running offset), the lock
  from `level: measured` (the Measured segment), and `measure_error` in
  plain words. The mock simulates a run (about 8 s of music per second),
  a track change (`daemon.simulateTrackChange()`) and a silent side B
  (`?measure=silent`).
- **Also play on** (Device page, `views/FollowersPanel.tsx`) lists the
  multi-device followers (`config.outputs.followers`, up to 3) with their
  `status.outputs` state as a chip (opening, locking, in sync, muted, N ms
  late, lost) and a detail line (clock ppm, lateness, path latency, rate,
  xruns, skips). Add, remove, enable, delay (trim) and volume each send one
  `set_follower` (store `addFollower` / `removeFollower` /
  `editFollower`, drafts and throttling like the stages); a refusal shows
  under that follower, or under the add row. Choosing a follower's device
  as the main device sends nothing: the Device page says why and offers
  "Remove it and switch" (`moveDeviceToFollower`). The candidate list and
  the wording live in `lib/followers.ts`. An engine without `outputs` in
  its config shows an update note.

- **Bypass** = `set_dsp_enabled { enabled: false }`. With DSP off the engine
  sends every stream through its bit-exact path (no EQ, convolution,
  upsampling or dither) and keeps all stage settings; routing is not touched.
  This is the simplest faithful mapping: moving streams to the Direct sink as
  well would change routing the user chose.
- **Stage power buttons:** the protocol has no "EQ off" or "upsampling off"
  flag that keeps settings. EQ off sends `set_eq(null)` and upsampling off sets
  the target to `off`; the previous EQ / ratio is remembered in the UI's local
  storage and restored when the stage is turned on. Convolution off turns
  every IR's `enabled` off and remembers which ones were on.
- **Auto preamp** (default on) writes `preamp_db` = dsp-core's automatic
  preamp for the current bands with every EQ edit. Click "auto" to set it by hand.
- **Undo/redo** covers EQ edits (one step per drag or wheel gesture). Loading
  a preset clears it.
- **Import** in the EQ header replaces the EQ with a ParametricEQ.txt (parsed
  in TS). The preset menu's Import sends it to the daemon as a new preset
  (`import_apo`) and loads it. An imported preset is EQ only, so loading it
  can turn convolution and upsampling off; the toast says so. Copy as ParametricEQ.txt writes `Channel: L/R`
  sections for a split EQ.

## EQ graph overlays

The EQ view's **Overlays** menu draws curves behind the EQ: a target (dashed),
preference bounds (a translucent band) and measurements (thin solid lines),
plus the **predicted** measurement + current EQ in a stronger stroke.

- Files: REW text/CSV, squig.link and AutoEq CSV, or plain `freq dB`, separated
  by commas, tabs, semicolons or spaces, with an optional header and `*`/`#`/`;`
  comment lines. Extra columns are ignored. Bounds take `freq, lower, upper`,
  or two single-curve files. Ten points minimum, sorted, non-finite values
  dropped; everything is resampled onto a 1/24-octave log grid
  (`lib/overlays.ts`).
- Each overlay is normalised at a reference frequency (1 kHz, changeable) so
  it sits on the graph's 0 dB line, then shifted by its own offset. Bounds are
  shifted as one band, so their width at the reference stays visible.
- Built-in targets from [AutoEq](https://github.com/jaakkopasanen/AutoEq)
  (MIT, revision 7ae0f56): Harman over-ear 2018, Harman in-ear 2019 and
  diffuse field ISO 11904-1 (`src/assets/targets`), plus diffuse field B&K
  5128, diffuse field GRAS KEMAR and oratory1990 Optimum HiFi
  (`src/assets/research-targets`); a generated speaker tilt exploration band
  (−1.0 to −0.5 dB/oct, an engineering choice, not published limits), a tilt
  target (dB/octave through 0 dB at 1 kHz) and flat. Provenance and licences
  are in the two `SOURCES.md` files; the menu's **Sources** note sums them
  up. No published preference bounds are built in (none come with a licence
  to ship them), so bounds are import-only.
- The menu is compact: Target / Import rows (file-format help in the
  tooltips), one row per curve (eye, colour, name, kind, `+EQ` for
  measurements, offset, delete), the reference frequency and **Sources**.
- Storage: one JSON file per overlay in
  `$XDG_CONFIG_HOME/audivane-dsp/overlays/` (`~/.config/...`), through the
  Tauri commands `overlays_list`, `overlay_save`, `overlay_delete`
  (`src-tauri/src/overlays.rs`). Names are validated and escaped into file
  names, so a name can never reach outside that folder. Visibility, colour,
  offset and the `+EQ` choice are stored with the file. The browser mock keeps
  overlays in memory.
- The drawing lives in `views/OverlayLayer.tsx`; `EqGraph.tsx` only calls
  `paintOverlays` and `useOverlayRedraw`. `lib/overlays.ts::eqResponseDb` is
  the one place the overlay code asks for the EQ's response, so layered L/R
  sections only change that wrapper.

## Upsampling view

- **Rate:** Off, Multiply (common ratios up to ×32 and ×64, plus any whole
  ×1…×64), Highest (common caps up to 1.5M and 3M, plus any 44.1…3072 kHz;
  the same rate family, or "Best for DAC" = `max_rate_any`) or Fixed rate
  (`{rate}`, fractional or downsampling). The ×32 / ×64 and 1.5M / 3M
  buttons are unavailable ("Your DAC does not report 1411.2 kHz or
  1536 kHz") when `status.device_caps` lists neither rate of that step
  (`dacHighRateBlock`); an engine from before them never probes those
  rates, so they are unavailable there too. A response the engine does not
  measure for a ratio shows its message as plain text under the plots. Fixed rate waits for a rate to be picked. What runs now
  (input → output, `rate_ratio`) is the status bar's; the view adds a note
  when the device fell back or cannot play the source rate. In the Custom
  editor, Kaiser β (and the standard Gaussian's stopband) is given with a
  Fraction or Stop-at-Nyquist cutoff and derived with Band, as the engine
  requires; the taps field also takes `2^20`, `64k` and `1M` (k = 1024). A rate a running
  custom design does not fit is not sent: the view asks `check_design`
  with that target first and shows the engine's reasons instead.
- **Older engines:** a status without `rate_ratio` (before fractional
  rates) is read as an integer ratio at the transport boundary
  (`api/legacy.ts`), and `caps.rationalRates` turns Fixed rate and Best for
  DAC off with an update note.
- **Recommended** (`lib/upsamplerRecommended.ts`) is the first card in
  Preset: one `set_upsampler` with `target {max_rate: 768000}`, `preset:
  "recommended"`. It replaces the kept custom design. The
  card and the sidebar ("Recommended · …") follow the config when it runs
  that name on that target; a custom copy of it is a custom design.
- **Preset / Custom:** Preset sets `UpsamplerConfig.preset`; Custom runs
  the last custom design again (the store remembers it), or, with none,
  plays a copy of the running preset as "Edit as custom" does. Preset
  returns to the last preset. `lib/filterPresets.ts` splits
  each `FilterPreset` name into a family, length, shape (roll-off, window,
  stage, method or variant) and phase; its test fails when the generated
  type gains or loses a name. Rows are hierarchical: a row keeps the rows
  above it, a click keeps as much of the rows below as exists, missing
  combinations are disabled with a tooltip. Presets the rate setting rules
  out (the engine's ratio classes and the rational length/cost limits,
  checked like `validate_upsampler`, A/B doubling the pipelines) are
  disabled with the reason; any other refusal is the daemon's message under
  the controls (`upsamplerError`). There is no Apply step: a custom design
  plays as soon as it passes `check_design` (`store.editCustomDesign`).
  One that fails the check or that the engine refuses (a long design at a
  fractional rate) becomes a draft (`upsDraft`, "Not applied", Discard
  changes) and what played before, a preset too, keeps playing.
- **Edit as custom** (on the preset detail line and under the Recommended
  card) asks `resolve_design` for the preset's exact `FilterDesign`
  (`based_on` set) and plays it as the custom design (the same sound). The
  Custom view then reads "Same as X" (with "Use
  the preset", which runs the name) or "Modified from X" (with "Revert to
  preset"), comparing the design with the expansion by value
  (`lib/filterDesign.ts::sameDesign`).
- **Custom** (`views/CustomDesign.tsx`, logic in `lib/filterDesign.ts`)
  edits a `FilterDesign`: Start from (Recommended, Music, Low latency, any
  preset), the type (windowed sinc, Lanczos, cascade, interpolator, block
  FFT), then for a windowed sinc the window and its parameter (Kaiser β or
  derived, Gaussian α or stopband, flat-top set, Chebyshev sidelobes in dB,
  Tukey taper), the length rule (taps, per input sample, constant time,
  automatic with its accuracy target; a band length comes with the Band
  cutoff), the phase (with the intermediate blend) and the cutoff
  (fraction, Stop at Nyquist, band edges and depth). Per-rate values of the
  compact presets show as steps with "Use one value". Advanced holds the
  window sampling, the phase method, keeping the original samples,
  decimation, accepted ratios, the fallback window, the arithmetic path and
  grid cutoff, and the bounded-input peak from the measurement. Each edit
  keeps the design whole (a window the path does not build moves it to the
  FIR path, a band length brings a band cutoff, and so on). The view
  checks the shown design with `check_design` as it changes (debounced):
  each error shows under its field (by its `design.…` path, a set_upsampler
  refusal too) and the plan ("Cost per rate", beside the design rows when
  the panel is at least 1000 px wide, else below them) gives taps, memory
  and CPU per allowed source rate, with the latency of a linear design. An
  edit that passes the check is sent at once; one that does not stays a
  draft ("Not applied", Discard changes) and is not sent. The plots measure
  what runs (`upsampler_response`), as before.
  The upsampler has no headroom of its own: the fixed cut
  (`output.headroom_db`) is on the Limiter view, with a slider and a
  number field; it works with the limiter off too.
- **CPU limits:** a design this computer cannot run in real time is
  refused by the engine ("Too heavy for this CPU at 768 kHz: ..."), shown
  under the controls like any refusal. When the engine falls back after an
  overload (`Status.overload`: upsampler off, the last setting that ran,
  bypass, or the bypass resampler's filter at the same rate on a DAC that
  plays nothing lighter), its plain message stands above the
  settings until the upsampler is changed; the settings shown stay the
  saved ones, and the plots are labelled "Saved setting" (not "Playing
  now"). The mock models both
  (`lib/cpuBudget.ts`, `MockDaemon.simulateOverload`).
- **Unavailable options** stay in the tab order (`aria-disabled`), name
  their reason through `aria-describedby` and show it in a bubble while
  focused (`components/Why.tsx`), in segmented controls, family cards,
  checkboxes and the Start-from buttons.
- **What the plots show:** at the rate playing now they measure what the
  engine runs (the target rewritten to the landing in status, so a capped
  ratio shows as it plays) and say "Playing now"; at another preview rate,
  or with the engine stopped, they are labelled "Target preview".

## Spectrum and upsampling plots

- The live spectrum (daemon: 8192- and 32768-point FFTs, band power per
  1/24-octave bin) is smoothed in the UI to 1/3, 1/6, 1/12 (default) or 1/24
  octave by power averaging (`lib/spectrum.ts::smoothFractionalOctave`) and
  drawn as a monotone cubic through the bin centres, which cannot overshoot.
  Its dBFS scale sits in the graph's right gutter, clear of the legend.
- The Upsampling view's transition and full-range plots (`views/ZoomPlot.tsx`,
  maths in `lib/plotView.ts`): drag a box to zoom (X only by default; the
  X+Y toggle or Shift swaps), Ctrl/Alt/middle-drag or the Pan mode pans, the
  wheel zooms frequency around the cursor (Shift: level), double-click
  resets. Hovering shows the curve's frequency and level with precision that
  follows the zoom. When zoomed, the view asks the daemon for that window
  (evaluated directly from the cached impulse), debounced, one request at a
  time over the analysis connection, dropping replies for windows no longer
  shown; a window the engine refuses is not asked again until the view or
  the design changes. The plots run to the Nyquist the engine measures to:
  the output's, or the intermediate grid's (source × up) at a fractional
  ratio. The crossings row lists where the response reaches −0.0001 … −150 dB;
  click one to zoom there.

## Meters

The status bar meters stop drawing while there is nothing to show
(silence, stale or no reports, the bars resting at the floor) and wake on
the next report with signal, a clip reset, a resize or a theme change.

The status bar shows `peak −x dBFS` (held 1.5 s; the same quantity as the
bars: the float output just before the integer conversion, after the volume)
and `chain ±x dB` (what EQ, convolution and the upsampler add at the peaks,
before the volume: `chain_peak_db − input_peak_db`). A peak at or above
0 dBFS latches the clip LED even when no sample was counted as clipped.
DSP, xruns and latency have their own tooltips; the latency tooltip splits
the output buffer from the filter delay (`filter_latency_ms`).

## Volume and balance

Both sliders work in dB with a taper (`lib/taper.ts`): volume gives −40…0 dB
80% of the travel (the remaining 20% reaches the engine's minimum), balance
gives ±3 dB 70% of the travel and the rest reaches ±`BALANCE_MAX_DB`. Arrow
keys move 0.5 dB (Shift 0.1), the mouse wheel likewise, double-clicking the
readout types an exact value, and double-clicking the balance slider centres
it. Drags still go through `lib/queue.ts` and the store's throttle; the final
value is always sent on release.

Next to VOL, `HW` means the DAC's own volume control is used; `DIG` means the
DAC has no hardware volume, so the engine scales the signal in 64-bit float
before dither. On the Direct sink or with Bypass on, digital volume is never
applied: the volume reads "on DAC", and the volume and balance sliders are
disabled (tooltip: "Bit-perfect: set the volume on the DAC").

## Tray and window

The tray icon is our own StatusNotifierItem (the freedesktop/KDE tray
protocol over D-Bus, `src-tauri/src/tray.rs` with `ksni`), not an
appindicator icon, so it gets left clicks, the wheel and, on Wayland, the
click's activation token.

- **Left click** opens the volume popup, or the menu as before (Device →
  Tray and volume keys → Tray icon click). **Right click** always opens the
  menu: Show window, Volume…, the passthrough line, Bypass (= DSP off),
  Listen to A / B, Preset submenu, Routing (System-wide / Selected apps),
  Quit. **Middle click** shows the window. **The wheel** over the icon
  steps the volume like the volume keys: deltas add up and each full notch
  (120 units) is one step, so a touchpad's small deltas do not each count
  as a step. Hosts disagree on the sign (Plasma: positive is wheel-up;
  GNOME's AppIndicator extension forwards Mutter's, positive is down); the
  app picks it from the process owning the StatusNotifier watcher, else
  `XDG_CURRENT_DESKTOP`, with Plasma as the reference (`wheel.rs`). Device
  → Reverse wheel direction turns it round for a host it guesses wrong.
- Closing the window goes to the tray; the engine is a separate systemd
  service and keeps running either way. A second launch shows the window
  (single instance). The main window's size and position are remembered
  (window-state plugin).
- **Nothing runs for a window nobody sees.** Every webview is a WebKit web
  process of its own (the main one about 300 MB with meters running), and
  WebKit keeps a destroyed view's process unless told otherwise. So
  closing to the tray destroys the main window and ends its web process
  (`webkit_web_view_terminate_web_process`), drops the daemon's `meters`
  and `spectrum` topics (status, streams, config and presets stay: the
  tray menu, the popup and the OSD use them), and showing it makes it
  again from the app config (`create: false` in `tauri.conf.json`). The
  page comes back on the same view (local storage, as after a restart) and
  scroll position (kept in the app process only): a close hides the window
  at once and sends `main-closing`; the page answers with where it is
  (`main_close_ready`), and only then (or after 500 ms without an answer)
  is the window destroyed. While it scrolls the page also saves its place
  every 250 ms (`main_view_save`). EQ undo history and unsaved drafts do
  not survive a close to the tray. The popup and our OSD window are made
  when first needed and destroyed 30 s after they were last hidden
  (`window_life.rs`), so a quick reopen is still instant. A window being
  destroyed is marked closing first (Tauri queues `destroy()`): a tray
  click, a second launch or an OSD in that gap is kept and made afresh
  once the old window is gone, never shown on it. Each webview has its
  own web process (the popup and the main window share only the network
  process), so ending one never touches another. With every window gone
  the tray keeps the app running (`ExitRequested` is refused while a tray
  is up).
- `audivane-dsp-ui --hidden` starts in the tray, without making the main
  window (or its web process) until it is shown. **Start at login** (Device)
  writes or removes `$XDG_CONFIG_HOME/autostart/audivane-dsp-ui.desktop`
  (`Exec=<the app> --hidden`, where the app is the AppImage file itself
  when it runs from one, never the binary in its temporary mount); nothing
  outside the home folder.
- **Is there a tray?** The app follows the StatusNotifier watcher
  (`sni_watch.rs`: its owner and `IsStatusNotifierHostRegistered`), not
  just its own item. Close-to-tray is on only while a host is registered
  and our item is up; when the watcher or the host goes away (a panel
  restart), closing the window quits the app instead, whatever helper
  windows exist, and close-to-tray comes back with the host. The item keeps
  waiting for a late watcher and registers when one appears. Started with
  `--hidden` and no tray after about 3 s, the window is shown. A second
  launch shows the window, and opens it again if it was closed.
- **XEmbed-only panels** (the old system tray protocol, without
  StatusNotifier support) show no icon: the app no longer links
  libayatana-appindicator, which used to fall back to XEmbed. There the
  window simply closes and quits like on a desktop without a tray.
- The tray item, the watch on the tray host, the portal session and
  Plasma's OSD all talk D-Bus on their own threads with about 2 s bounds;
  startup and quit never wait on D-Bus on the UI thread. Without a session
  bus, single-instance is skipped and Start engine reports that the session
  bus is unreachable.

### Volume popup, volume keys, on-screen display

Three webviews load the same frontend and pick their view by window label
(`main.tsx`; in the browser `?window=popup|osd`): `main`, `popup` and
`osd`. Each has its own store and event channel (`Hub` keeps one per
window; meters and spectrum go to the main window only), and each is a
WebKit web process of its own, made only while needed (see above).

- **Popup** (`views/TrayPopup.tsx`, 320×184, undecorated, always on top):
  the level in dB (double-click to type), a slider with the top bar's taper
  and keys, mute, HW/DIG, the passthrough badge, what is playing, the
  ceiling, and Open Audivane DSP. It runs a *secondary* `AppStore` (no
  spectrum subscription, presets, devices or overlays) and writes through
  the same guarded gestures as the top bar (`useVolumeModel`), so the
  ceiling and the guard apply as there. Escape, a click elsewhere (focus
  loss) or another click on the icon closes it. On X11 it opens next to
  the icon (`placement.rs`); Wayland compositors place it themselves.
- **Volume keys** (off by default): volume up, down and mute as global
  shortcuts through the GlobalShortcuts portal (`shortcuts.rs`; KDE asks to
  confirm and lists the keys it assigned), else on X11 by grabbing the keys
  (Tauri's global-shortcut plugin). Without either, or when the user says
  no, Device says so (KDE then keeps them unbound; they can be bound in
  System Settings → Shortcuts). The desktop's own volume keys must be
  unbound first, or both act. If the session is lost later (the desktop
  closes it, the portal restarts) the keys show as inactive and are
  registered again when the portal is back. A held key repeats (after
  400 ms, every 90 ms) only once the backend has shown it reports key
  releases; one hold of volume up adds at most 6 dB, the press included,
  then waits for the release (a lost key-up cannot run the volume up), and
  a second press of the held key before its release does nothing. Each
  step is one guarded write (0.5–3 dB, default 1 dB; mute toggles only the
  mute; nothing is sent at the ceiling) made by the app process, window
  open or not (`volume_step.rs`, the same rules as the store's
  `volumeKey`, which the browser mock uses in its place): one serial
  writer, on the newest level from status events and its own echoes, and
  the windows only show the result. A write refused because the level
  moved (`STALE_REVISION`) reads the status and tries once more on the
  same engine instance, after checking again that the path takes a
  digital level; a replaced engine (`STALE_INSTANCE`) drops the press.
  `cargo run -p audivane-dsp-ui --example shortcuts_probe` tries the portal
  without the GUI.
- **On-screen display** (~1.2 s): after a volume key or the tray wheel
  (default), after any level change (the desktop, the CLI, a guard step;
  not while an Audivane window has the focus), or never. On KDE Plasma it
  is Plasma's own OSD (`org.kde.plasmashell` `/org/kde/osdService`,
  `plasma_osd.rs`), drawn by the shell without taking the focus: a bar with
  our label via `mediaPlayerVolumeChanged` (using the slider's perceptual
  taper, with the ceiling as 100 %), falling back to `showText` for muted /
  notes / 0 % or when `mediaPlayerVolumeChanged` fails (cached for the
  session). Elsewhere, or when there is no `osdService`, it is our own window
  (`views/Osd.tsx`, 280×104, not focusable, matching the tray popup's level
  row with the large dB readout and tapered bar). GTK3 cannot keep a new window
  from taking the focus on Wayland, so there our window is only shown
  while an Audivane window has the focus; on other Wayland desktops the
  OSD therefore does not appear over other apps.
- The app's own settings (tray click, keys, step, OSD) are in
  `$XDG_CONFIG_HOME/audivane-dsp/app-settings.json` (`ui_settings.rs`), set
  with the `ui_settings` / `set_ui_settings` / `set_autostart` commands.

## Fonts

Renamed IBM Plex Sans and Mono Latin subsets (`woff2`) are bundled in
`src/assets/fonts` under the SIL Open Font License 1.1 (`OFL.txt`; see
[font licensing](../docs/font-licensing.md)). Nothing is loaded from the network.
