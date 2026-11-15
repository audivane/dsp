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
hardware mixer: digital volume). In the browser console
`avdsp` is the store.

## Checks

```sh
npm run typecheck      # tsc
npm test               # vitest: store, EQ maths vs dsp-core, APO import/export, passthrough text,
                       #   throttle, overlay parsing/resampling, slider tapers
npm run build          # tsc + vite build into dist/
node scripts/interact.mjs test-results/interact   # Playwright smoke test of the built app (drag, wheel, undo, menus…)
npm run screenshots -- --views eq,conv,ups,out,routing,ab,device   # into test-results/screenshots/
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

The GUI build needs `webkit2gtk-4.1`, `gtk3` and `libayatana-appindicator`, and
a built frontend in `app/dist` (it is embedded at compile time):

```sh
cd app && npm ci && npm run build
cargo build -p audivane-dsp-ui --features gui          # debug binary
npx tauri build --no-bundle                            # release binary (enables `gui` via tauri.conf.json)
cargo build --release -p audivane-dsp-ui --features custom-protocol  # same, without the tauri CLI
npx tauri dev                                          # live reload against the real daemon
```

### In the rootless Arch build root (Ubuntu dev box)

The dev box has no WebKitGTK headers. Build inside `~/.cache/audivane-archroot`
(see `packaging/arch/archroot.sh`); worktrees and `~/Project` are bind-mounted at
the same path, so use a separate target dir:

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
  delivered": they are not forwarded while the window is hidden to the tray or
  minimized, and at most one frame is in flight at a time. The webview
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
  `set_convolution`, `set_upsampler`, `set_volume`, `set_dsp_enabled`,
  `set_ab`, `set_routing_mode`) are used wherever they exist; only output
  depth/dither, the format order and the device go through `set_config`. Live
  drags keep at most one send waiting, which always carries the newest value.
- **Live check:** `examples/probe.rs --exercise IR1.wav IR2.wav` runs every
  call the UI makes against a running daemon (use a test daemon with its own
  `--socket`/`--config`) and checks replies and `config`/`presets` events.
- **Older engines:** calls from plan sections A–G are optional. `list_presets`,
  `list_devices`, `set_volume` or `right`/IR-chain support missing → that
  control is disabled with a plain explanation instead of failing.

### Mappings worth knowing

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
  shown. The crossings row lists where the response reaches −0.0001 … −150 dB;
  click one to zoom there.

## Meters

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

Tray menu (Linux tray icons have no click events, so it is menu-only): Show
window, the passthrough line, Bypass (= DSP off), Listen to A / B, Preset
submenu, Routing (System-wide / Selected apps), Quit. Closing the window hides
it to the tray; the engine is a separate systemd service and keeps running
either way. A second launch shows the existing window (single instance).
Window size and position are remembered (window-state plugin).
`audivane-dsp-ui --hidden` starts in the tray (useful for autostart).
Without a tray host (no StatusNotifier watcher or appindicator library) the
app logs a warning and runs without a tray: closing the window then quits and
`--hidden` is ignored. Without a session bus, single-instance is skipped and
Start engine reports that the session bus is unreachable.

## Fonts

IBM Plex Sans and IBM Plex Mono (latin subset, woff2) are bundled in
`src/assets/fonts` under the SIL Open Font License 1.1 (`OFL.txt`). Nothing is
loaded from the network.
