# Changelog

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
