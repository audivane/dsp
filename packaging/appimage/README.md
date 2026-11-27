# AppImage

`packaging/release/appimage.sh` (run by `release.sh`) packs the staged release
tree into `Audivane-DSP-<version>-x86_64.AppImage`: the app, the engine
(`audivane-dsp`) and the CLI (`avdsp`), the same binaries as the packages
(Ubuntu 24.04 baseline, glibc 2.39).

| File | Role |
|---|---|
| `AppRun` | entry point: opens the app, or `--install` / `--uninstall` / `--status`, or runs the bundled `avdsp` / `audivane-dsp` |
| `setup.sh` | the per-user install, update and removal (also copied into the install) |
| `RUNTIME-LICENSES.txt` | notices for the AppImage runtime embedded at the front of the image |

## What it bundles

Nothing besides our own files. The engine needs libasound, libpipewire and
libstdc++; the app GTK 3, WebKitGTK 4.1 (with libsoup 3 and JavaScriptCore)
and D-Bus; the tray is our own StatusNotifierItem over D-Bus, so no
appindicator library. PipeWire and ALSA must come from the host anyway.
WebKitGTK is deliberately not bundled: its helper processes are found by a
compiled-in path, and a bundled copy brings its own GLib, GTK and Mesa
expectations, which is where webview AppImages usually break on newer
distributions. When it is missing, `AppRun` says which package to install
(and the engine and CLI still work).

## Install, update, removal

Running the AppImage with no user install opens the app from the AppImage;
its "engine is not running" page offers **Set up and start**
(`setup.sh install`). That copies the three binaries to
`~/.local/lib/audivane-dsp/` with links in `~/.local/bin`, writes
`~/.config/systemd/user/audivane-dsp.service` (pointing at the copy),
`~/.local/share/applications/audivane-dsp-ui.desktop` and the icons, enables
and starts the service, and lists everything it created in
`~/.local/lib/audivane-dsp/MANIFEST`. The app then relaunches into the
installed copy.

It copies rather than runs from the AppImage: the engine is a sandboxed
service (`NoNewPrivileges`), which cannot FUSE-mount an AppImage, and a copy
keeps working when the AppImage file is moved, renamed or deleted.

With an install present, the AppImage opens the installed app. A newer
AppImage first updates it: each file is written anew and renamed into place,
the service restarts if it was running, and a running app notices its binary
was replaced and relaunches itself, as after a package upgrade. An older
AppImage does not downgrade by itself (`--install` does it on request).

`--uninstall` (or `~/.local/lib/audivane-dsp/setup.sh uninstall`) stops and
disables the service, closes the installed app and removes what the manifest
lists, folders only when empty. `--purge` also deletes the settings, presets
and state, like removing a package.

A system package wins: with `/usr/bin/audivane-dsp` present, `--install`
refuses and the AppImage's app uses the package's engine (one engine, one
socket). If a package is installed after a user install, the user's unit in
`~/.config/systemd/user` takes precedence over the package's until the user
install is removed; `--status` says so.

`avdsp upgrade` updates packages only; it tells a user install to use a new
AppImage.

## Tools

appimagetool 1.9.1 and the type 2 runtime 20251108 (static, needs only
`fusermount`, not libfuse2) are downloaded once into
`~/.cache/audivane-release-tools/` and checked against pinned SHA-256s.
appimagetool runs extracted, so building needs no FUSE.
