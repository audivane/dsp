# Arch Linux package: `audivane-dsp`

PKGBUILD for Audivane DSP: the daemon (systemd user service), the `avdsp`
CLI and the Tauri desktop app.

## Build and install (on Arch)

```sh
./make-source.sh          # packs the committed git tree (HEAD) into a .tar.zst
makepkg -si
```

`make-source.sh` must run inside the git checkout; it reads the version from
`[workspace.package]` in `Cargo.toml` and writes
`audivane-dsp-<version>.tar.zst` next to the `PKGBUILD`. The package never
downloads sources (the tarball is the only `source` entry, `sha256sums=SKIP`).

Flags / variants:

- `./make-source.sh --worktree` — pack the *working tree* instead of HEAD,
  including uncommitted changes (`target/`, `node_modules/`, `app/dist/` are
  excluded). For testing local edits only; the resulting package is not
  reproducible.

## What gets installed

- `/usr/bin/audivane-dsp` — the DSP engine daemon
- `/usr/bin/avdsp` — CLI (offline processing + `avdsp ctl` daemon control)
- `/usr/bin/audivane-dsp-ui` — desktop app (only once `app/src-tauri` builds)
- `/usr/lib/systemd/user/audivane-dsp.service` — user service
  (`ExecStart=/usr/bin/audivane-dsp --foreground`)
- `/usr/share/applications/audivane-dsp-ui.desktop` (named after the UI binary, which is also its Wayland app ID)
- icons in `/usr/share/icons/hicolor/` (`audivane-dsp.svg` scalable plus the
  pre-rendered PNGs from `assets/brand/icons/`)
- `/usr/share/licenses/audivane-dsp/LICENSE`
- `/usr/share/doc/audivane-dsp/` (`DEV.md`, `control-plane.md`)

Runtime deps: PipeWire + WirePlumber, ALSA, `webkit2gtk-4.1`, `gtk3`,
D-Bus (the tray icon is a StatusNotifierItem over D-Bus; no appindicator
library). `rtkit` is optional but recommended
(real-time scheduling for the audio thread).

## Enable the engine

The package deliberately never touches systemd for you:

```sh
systemctl --user daemon-reload
systemctl --user enable --now audivane-dsp
avdsp ctl status            # health check
journalctl --user -u audivane-dsp -f
```

While the engine runs it owns the PipeWire sink and takes your DAC
exclusively; `systemctl --user disable --now audivane-dsp` hands the device
back.

## Uninstall

```sh
systemctl --user disable --now audivane-dsp   # release the DAC first
pacman -Rns audivane-dsp
```

Per-user config lives in `~/.config/audivane-dsp/` and is kept by pacman;
delete it manually for a full wipe.

## Cleaning this directory

`makepkg` leaves `src/`, `pkg/`, and `*.pkg.tar.zst` here and
`make-source.sh` leaves `*.tar.zst`; all are gitignored. Remove them with
`rm -rf src pkg *.pkg.tar.zst *.tar.zst`.
