# Changelog

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
