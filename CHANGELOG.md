# Changelog

## 0.10.1

- Engine: a DAC that is listed but not ready yet when the engine starts
  (at boot, a USB DAC can show up before it accepts connections) no longer
  stops the engine. It starts without the DAC, says why, and retries until
  the DAC opens. Before, the service failed, Audivane's outputs never
  appeared and the desktop could switch to another device until the engine
  was restarted by hand.
- Engine, App: the "Engine accepts" rate chips on the Device page follow the
  DAC in use. A rate it cannot play with the current settings stays allowed
  but is drawn inactive instead of lit, and every chip can be switched back
  on (only the last allowed rate is locked). Before, after a move to a DAC
  with fewer rates, the higher rates stayed lit and could not be re-enabled
  once switched off.
- Engine, App: the Upsampling preview shows what the DAC really gets at each
  source rate. A rate the DAC cannot take at the chosen setting is marked:
  PipeWire converts it first, so the page says so instead of plotting a
  filter that would not run, and a source rate that lands at 1:1 says
  upsampling does nothing there. When "Best for DAC" would do better, the
  page offers it in one click. The cost table follows the same rates.
- App: the PEQ filter-type buttons no longer look as if two were selected
  after clicking around (the keyboard focus ring is drawn inside the
  button).

## 0.10.0

### Highlights

- **Upsampling filters**: 66 presets in families (Gaussian, flat-top,
  Chebyshev, Kaiser, half-band, Lanczos, Nuttall, Hann, short leaky,
  cascades, short interpolators, block FFT), a Recommended filter, and a
  Custom editor over the whole filter design (window, length, cutoff or
  band, linear, minimum or intermediate phase), with live plots and a cost
  table.
- **Any output rate**: fractional ratios (44.1 → 48 kHz) and decimation
  (192 → 48 kHz) with the same filters, integer ratios up to ×64 and output
  rates up to 3.072 MHz, and automatic filter lengths measured against an
  error budget or the output bit depth.
- **New stages**: DC blocker, tilt EQ (bass, treble or both), headphone
  crossfeed, harmonics, tempo-preserving pitch shift and varispeed, and a
  true-peak limiter with automatic headroom.
- **Output**: nine new dither and noise-shaping modes, and one fixed output
  headroom (3 dB by default) against inter-sample overs.
- **A/B compare**: side B as a change to any stage or the filter, quick
  compare from every stage page, level matching that never boosts (analytic
  or measured on the track), and output compare with two quantizers.
- **Also play on**: up to three more DACs play the same music, drift
  tracked and aligned to the main output.
- **Volume**: software, hardware or hybrid volume, a hard ceiling, a guard
  against sudden jumps from apps or the desktop, per-app volume, and the
  desktop's slider driving the engine.
- **Desktop app**: tray volume popup, volume keys with an on-screen display,
  start at login hidden in the tray, and a much lighter idle footprint.
- **Robustness**: a chain that runs over real time falls back to a lighter
  one step by step instead of crackling; an unplugged DAC leaves the music
  where it was.

### Upgrading from 0.9.x

- `avdsp upgrade` installs 0.10.0 like any update.
- Settings carry over (EQ, convolution, devices, presets, routing). The old
  upsampler fields are not read: if upsampling was on, it keeps its rate and
  runs the Recommended filter; pick a preset or a Custom design to change
  it. An A/B setup from 0.9.x is dropped (the engine log says so).
- Removing the package now stops the engine and deletes every user's
  Audivane DSP settings.

## 0.9.4

- Fix: the engine no longer fails to start when its output device is
  missing (DAC unplugged, or the card renumbered after a reboot). It starts,
  answers the app, says which device is missing, and switches to the real
  engine as soon as the device appears or another output is picked. The app
  shows this in a banner with a "Choose output" button. Before, systemd
  restarted the failing service until it gave up and the app only said the
  engine was not answering.
- Fix: unplugging the DAC while it plays no longer leaves the engine
  stuck. Before, its audio thread stopped for good: picking another output
  opened the device but played nothing, and "Release DAC" timed out. Now
  the engine restarts, waits for the DAC (or plays the output you pick), and
  takes the DAC back when it is plugged in again.
- Fix: devices are saved by card id (`hw:CARD=<id>,DEV=<n>`) instead of the
  card number, which changes with boot order. A numeric device in an existing
  config is converted at start-up while the device is present.
- Fix: in system-wide routing, unplugging the DAC no longer leaves the
  session on a silent `auto_null` sink: Audivane hands the default back while
  its sink is gone, and takes it again when the DAC returns.
- The Device and Output views (and `avdsp ctl status`) show how many bits
  the DAC really converts in the format in use, e.g. "S32 · 24-bit" for a
  laptop's built-in codec, and the Output view suggests the matching dither
  target with a one-click button. HD-Audio codecs are read from their own
  report, since recent kernels claim all 32 bits for them.
- When the engine does not start at all, the app shows the engine's own
  error instead of pointing to `journalctl`, and "Start engine" also clears
  systemd's restart limit.
- `avdsp ctl status` says `passthrough: bit-perfect` / `not bit-perfect` /
  `nothing is playing` instead of `intact` / `broken`, with a hint for
  volume. Volume percentages (CLI and app) use the cubic scale of desktop
  volume sliders.

## 0.9.3

- Fix: the status bar now uses the live engine input rate for `IN`, so the
  displayed input rate × upsampling ratio matches the DAC output rate.
  Previously it used the first connected app’s rate, which could show
  `48k → ×4 → 384k` while a 96 kHz track was playing alongside a 48 kHz
  browser stream. Audio processing was unaffected.

## 0.9.2

- `avdsp upgrade`: updates a packaged install in place. It reads the latest
  release from GitHub, shows "installed X → latest Y" and the release notes,
  asks, downloads the package for your distribution plus `SHA256SUMS`, checks
  the SHA-256, installs the local file with `sudo pacman -U` / `apt install` /
  `dnf install`, restarts the engine if it was running and confirms it runs
  the new version. `--check` only reports (exit 10 when an update exists),
  `--yes` skips the question, `--version <tag>` installs a given release
  (reinstall or downgrade). It refuses to run as root and does not touch a
  source build.
- Fix: upgrading the package left the old engine running until the next
  login or a manual restart. On upgrade (not on first install) the package
  now reloads every running systemd user manager and restarts
  `audivane-dsp.service` where it was running, and says for which users;
  without systemd (containers) it only prints the note. Arch packages now
  print a note on upgrade at all.
- App: when the engine runs a newer version than the app (the app was open
  during an upgrade), a banner says "Audivane DSP was updated to X. Restart
  the app to finish." with a **Restart** button that relaunches the app. When
  the engine is the older one, the banner offers **Restart engine** instead.
- Fix: starting `audivane-dsp` by hand while the service already runs failed
  with "device reservation denied for card N: current owner refused release".
  It now stops before touching the device: "Audivane DSP is already running
  (systemctl --user status audivane-dsp)."
- App, Device page: the sample rates are now buttons for all ten engine
  rates (44.1k to 768k). Click one to allow or block it; 352.8k and up stay
  off by default and can be turned on (e.g. so DXD reaches the DAC without
  resampling; upsampling then uses a smaller ratio). A dot marks the default
  rate; right-click (or Shift+F10) a rate for "Use as default". Blocking the
  default moves it to the nearest allowed rate of the same family, the last
  allowed rate cannot be blocked, rates the device cannot play are greyed
  ("Not supported by this device"), and the rate playing now is outlined.
- Engine: a config whose `default_rate` is not among `allowed_rates` is
  corrected (nearest allowed rate, same family first) instead of rejected,
  and the list is kept sorted. New `avdsp ctl rates [--enable HZ…]
  [--disable HZ…] [--default HZ]`.
- App, Upsampling page: the source-rate tabs are labelled "Preview at" (only
  a view, nothing changes in the engine) and list the rates the engine
  actually runs at on this device, so enabled high rates appear and blocked
  ones disappear. Previewing another rate than the one playing shows
  "Playing: 48k", which switches back to following playback.
- App: the Kaiser β presets say what they are: "β 10 · Steep", "β 14 ·
  Standard", "β 17 · Deep", "β 20 · Deeper", "β 24 · Deepest" (were
  Steeper/Standard/Deeper/Deepest/Extreme), with a one-line hint on the trade
  between transition steepness and stopband depth.

## 0.9.1

- Fix: on a fresh install the engine could not save its settings
  ("Read-only file system") because the service sandbox did not make the
  not-yet-existing `~/.config/audivane-dsp` writable.
- Arch install instructions download the package first; `pacman -U <url>`
  demands a `.sig`, which the release does not ship.
- Fix: devices that only play some rates (e.g. a 48 kHz-only USB speaker)
  failed to open with "no supported sample format found". The engine now
  probes what the device accepts (rates × formats, significant bits) when it
  opens it, offers PipeWire only the rates the device can play (so 44.1 kHz
  apps are resampled into a 48 kHz graph), and falls back to a smaller
  upsampling ratio, never to a device rate that does not match the graph.
  ALSA errors now say which parameter the device rejected and what it
  supports.
- Fix: dither targeted "Dither to" even when the device only took a narrower
  format, so an S16 device got 24-bit dither truncated to 16 bits. Dither is
  now capped at the negotiated format (S16 → 16 bits).
- Fix: on a device that fell back to a smaller upsampling ratio, every
  config change (e.g. a new filter length) reopened the device.
- Status reports the device's capabilities, the effective sample rates and
  default rate, and the effective dither depth (`avdsp ctl status` shows
  them).
- App: DAC formats the device never takes are greyed out, and "In use"
  explains a fallback (e.g. "S32 not available at 192 kHz"); "Dither to"
  says when it is capped by the DAC format; the Device page lists the sample
  rates and which ones are resampled; the Upsampling page says when the device
  forced a smaller ratio and no longer flashes to 44.1 kHz while the device
  reopens.
- Fix: at an 88.2, 176.4 or 352.8 kHz graph (an 88.2 kHz file, or a device
  that only plays 88.2 kHz) almost half of the output was silence while
  status said "playing, 0 xruns". The engine sized the DAC period from a
  linearly scaled quantum (1881 frames at 88.2 kHz) while PipeWire ran a
  power-of-two one (1024), and fired the extra graph cycle back to back,
  faster than the player could deliver. The period now matches the quantum
  PipeWire runs at every rate, and when the two still differ (a forced or
  lower-latency quantum) the extra cycles are spread out and buffered, never
  zero-filled. Present since 0.9.0, on any device.
- Fix: on a device that only plays some rates upsampled (e.g. a 96 kHz-only
  DAC whose 48 kHz graph the DSP sink plays x2), a 48 kHz app on Audivane
  Direct failed to open the device and left the engine in Error, even after
  the app went back to Audivane DSP. Direct never upsamples, so it now runs
  the graph at a rate the device plays natively and PipeWire resamples the
  app (status and the app say it is not bit-perfect); with no native rate at
  all Direct reports a clear error. A failed open now retries as soon as the
  route or rate changes, so going back to a playable path recovers.
- Fix: with several playable rates, the sinks' EnumFormat carried no rate at
  all; it now lists them (default first), also after a device switch.
- Fix: an unusable device blamed the sample rates and said it supported "no
  usable rate; no usable format". Errors now name the real problem with what
  the device does take: "needs 2 channels; device supports 1", or "supports
  32000 Hz only, which the engine doesn't run at".
- Fix: after switching to a device that could not be used, status still
  showed the previous device's capabilities, rates, format and dither depth.
  It now shows the new device's capabilities and no negotiated format, dither
  depth or effective rates while the output is closed.
- App: filter lengths 32k, 128k and 512k, and a Kaiser β 24 ("Extreme",
  linear phase only) preset.
- Routing: "System default" is gone from system-wide routing, where it only
  sent the stream back to Audivane DSP. In per-app routing it is now
  "Not through Audivane (<your normal output>)".

## 0.9.0

First public release of Audivane DSP (beta).

- System-wide processing engine for PipeWire: opens the DAC directly over ALSA,
  drives the PipeWire graph from the DAC's clock and processes in 64-bit float.
- Two outputs: *Audivane DSP* (processed) and *Audivane Direct* (bit-perfect).
- Parametric EQ with L+R, L and R sections, AutoEQ / Equalizer APO import and
  target overlays; convolution with WAV impulse responses; upsampling up to
  768 kHz with 16k–4M tap filters, linear or minimum phase.
- Follows the source sample rate. When apps play at different rates, the
  engine keeps the current rate (configurable: highest rate or the default
  rate), with a short dwell between switches.
- TPDF or noise-shaped dither; hardware or digital volume and balance.
- A/B comparison with A-weighted level matching.
- System-wide or per-app routing, live stream list, meters and spectrum.
- Desktop app (Tauri) with tray, presets and Midnight, Graphite and Light
  themes; `avdsp` CLI for control and offline processing.
- Packages for Arch, Ubuntu 24.04+ / Debian 13+ and Fedora 40+.
