<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/audivane-logo-dark.svg">
    <img src="assets/brand/audivane-logo.svg" alt="Audivane" height="56">
  </picture>
</p>

<h3 align="center">DSP — system-wide EQ and upsampling for Linux</h3>

<p align="center">
  Parametric EQ, convolution and upsampling for everything you play,<br>
  in 64-bit float, driven by your DAC's own clock. With filters you can inspect.
</p>

<p align="center">
  <a href="https://github.com/audivane/dsp/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/audivane/dsp?label=release&color=0b1020"></a>
  <a href="LICENSE"><img alt="License: AGPL-3.0-only" src="https://img.shields.io/badge/license-AGPL--3.0--only-0b1020"></a>
  <img alt="Platform" src="https://img.shields.io/badge/platform-Linux%20%C2%B7%20PipeWire-0b1020">
</p>

<p align="center">
  <a href="#install"><b>Install</b></a> ·
  <a href="https://audivane.app/dsp/">Website</a> ·
  <a href="https://audivane.app/docs/dsp/">Setup guide</a> ·
  <a href="docs/architecture.md">Architecture</a>
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/app-dark.webp">
    <img src="docs/images/app-light.webp" alt="The Audivane DSP app: signal path sidebar, parametric EQ graph with left and right curves over a live spectrum, band cards and a status bar with meters" width="860">
  </picture>
</p>

## Why

- **One processor for the whole system.** Every app goes through the same EQ
  and filters, or only the apps you pick.
- **Bit-perfect when you want it.** A second output, *Audivane Direct*, passes
  audio untouched to the DAC for a dedicated music player.
- **The DAC is the clock.** The engine opens your DAC directly over ALSA and
  drives PipeWire from it, so there is no hidden resampling between them. It
  follows the rate of what you play.
- **Filters you can inspect.** The upsampler and EQ plots show the measured
  response of the actual filter, not a sketch.
- **Fast to set up.** Import an AutoEQ `ParametricEQ.txt`, overlay a target
  curve, compare settings with loudness-matched A/B.

## Features

| | |
| --- | --- |
| **Parametric EQ** | L+R, L and R sections; peaking, shelf, pass, band-pass, notch and all-pass bands; AutoEQ / Equalizer APO import; target and measurement overlays |
| **Convolution** | WAV impulse responses for room correction or crossfeed |
| **Resampling** | integer ×2 to ×64, or up to 3.072 MHz with fractional ratios; 1 to 8.4 M taps; linear, minimum or intermediate phase; fractional (44.1 → 48 kHz) and decimating (192 → 48 kHz) ratios with the same filters |
| **Output** | TPDF or noise-shaped dither, S16/S24/S32, hardware or digital volume |
| **A/B compare** | Two settings, K-weighted level matching that never boosts |
| **Routing** | System-wide or per app; live list of streams and formats |
| **Monitoring** | Level meters, pre/post spectrum, stream and DSP load in the status bar |

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/upsampling-dark.webp">
    <img src="docs/images/upsampling-light.webp" alt="The upsampling view: the measured Nyquist transition of the real filter" width="760">
  </picture>
</p>

## Install

64-bit Linux with PipeWire and WirePlumber. Packages are on
[Releases](https://github.com/audivane/dsp/releases/latest) with SHA-256 checksums.

```sh
# Arch Linux
curl -fLO https://github.com/audivane/dsp/releases/download/0.10.0/audivane-dsp-0.10.0-1-x86_64.pkg.tar.zst
sudo pacman -U ./audivane-dsp-0.10.0-1-x86_64.pkg.tar.zst

# Ubuntu 24.04+ / Debian 13+
curl -fLO https://github.com/audivane/dsp/releases/download/0.10.0/audivane-dsp_0.10.0-1_amd64.deb
sudo apt install ./audivane-dsp_0.10.0-1_amd64.deb

# Fedora 40+
sudo dnf install https://github.com/audivane/dsp/releases/download/0.10.0/audivane-dsp-0.10.0-1.x86_64.rpm
```

Then start the engine for your user and open **Audivane DSP** from your app menu:

```sh
systemctl --user daemon-reload
systemctl --user enable --now audivane-dsp
```

While it runs, the engine uses your DAC exclusively;
`systemctl --user disable --now audivane-dsp` hands it back. The
[setup guide](https://audivane.app/docs/dsp/) covers choosing the DAC,
routing and troubleshooting.

### Update

```sh
avdsp upgrade            # shows installed → latest and the release notes, asks, then updates
avdsp upgrade --check    # only report: exit 0 when up to date, 10 when an update exists
```

Run it as your normal user: it downloads the package for your distribution
into `~/.cache/audivane-dsp/upgrade`, checks its SHA-256 against the release's
`SHA256SUMS`, installs it with `sudo pacman -U` / `sudo apt install` /
`sudo dnf install`, restarts the engine if it was running and confirms the new
version. `--yes` skips the question; `--version <tag>` installs a given release
(also to reinstall or go back). Installing a newer package by hand works too:
the package restarts every running engine, and the open app offers a
**Restart** button to load its new version.

> **Beta.** 0.9.x is the first public release series. Development runs on
> Arch with PipeWire 1.6; the Ubuntu and Fedora packages are built and
> install-tested but have seen less real use. Reports are very welcome.

## Build from source

Rust (stable), Node 22+, clang, and the development packages for PipeWire
(1.0+), ALSA, GTK 3, WebKitGTK 4.1 and D-Bus. The exact
package list is in [DEV.md](DEV.md#build-from-source).

```sh
git clone https://github.com/audivane/dsp.git && cd dsp
(cd app && npm ci && npm run build)
cargo build --release -p audivane-dsp-daemon -p audivane-dsp-cli
cargo build --profile release-ui -p audivane-dsp-ui --features custom-protocol
```

## Repository

| Path | What |
| --- | --- |
| `crates/` | `protocol`, `dsp-core`, `io-alsa`, `io-pw`, `daemon` (`audivane-dsp`), `cli` (`avdsp`) |
| `app/` | Desktop app (Tauri 2 + React) |
| `packaging/` | systemd user unit, release pipeline (`.deb`, `.rpm`, Arch) and `PKGBUILD` |
| `docs/` | [Architecture](docs/architecture.md), [control plane](docs/control-plane.md), [PipeWire driver](docs/pipewire-driver.md) |

## Contributing

Bug reports and pull requests are welcome; please read
[CONTRIBUTING.md](CONTRIBUTING.md). Pull requests need a one-time
[Contributor License Agreement](CLA.md), which a bot will ask you to sign.

## License

Copyright © 2026 Triet ([@Triet1402](https://github.com/Triet1402)).
Audivane DSP is free software under the
[GNU Affero General Public License v3.0 only](LICENSE). Third-party crates and
packages keep their own licenses, listed in
[`THIRD_PARTY_LICENSES.txt`](packaging/THIRD_PARTY_LICENSES.txt). The bundled
fonts are renamed IBM Plex subsets under the SIL OFL 1.1
([font licensing](docs/font-licensing.md)), and the target curves are
documented in the `SOURCES.md` files under [`app/src/assets/`](app/src/assets).
