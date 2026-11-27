# What the tray costs

How much the desktop app costs while it sits in the tray with its window
closed, measured to decide whether a tray-only process without a webview is
worth building. Numbers come from a release build
(`--features custom-protocol`) on a headless Xvfb display, with webkit2gtk 4.1
and GTK 3.24.

## How it was measured

- `Xvfb :77` 1600x1000x24, `GDK_BACKEND=x11`, software GL
  (`LIBGL_ALWAYS_SOFTWARE=1`), `WEBKIT_DISABLE_DMABUF_RENDERER=1`,
  `WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1`, a private session bus and a
  private `XDG_CONFIG_HOME`; the engine daemon on its own socket with no
  PipeWire session and no output device (its waiting engine), so
  status/streams/config events flow but nothing plays and no meters are sent.
- No StatusNotifier host runs under Xvfb, so a minimal watcher
  (`org.kde.StatusNotifierWatcher`, a few lines of zbus) claims the name and
  reports a registered host. The app then takes its normal path: the item
  registers, close-to-tray stays on, and `SecondaryActivate` (a middle click)
  brings the window back. Nothing draws the icon; no app code is skipped for
  that.
- The window is closed the way a window manager closes it (a
  `WM_DELETE_WINDOW` client message to the toplevel), which is the
  close-to-tray path.
- RSS is `VmRSS`, PSS is `smaps_rollup`, CPU is `utime + stime` and wakeups are
  voluntary + nonvoluntary context switches, all sampled once a second from
  `/proc`. States are sampled for 60 s (window open), 180 s (tray) and 60 s
  (reopened), after 4–6 s of settling.

## Numbers

| State | Processes | RSS | PSS | CPU | Wakeups |
| --- | --- | --- | --- | --- | --- |
| Window open | 3 | 343 MB | 226 MB | 14 % of a core (13–21 % over two runs) | ~660/s |
| Closed to the tray | 2 | 164 MB | 115 MB | 0.03 % | 0.02/s |
| Reopened from the tray | 3 | 343 MB | 223 MB | 17 % | ~700/s |
| `--hidden` start (window never made) | 1 | 71 MB | 59 MB | 0.008 % | <0.01/s |
| Minimal tray-only Rust binary | 1 | 5.1 MB | 3.5 MB | 0.0 % | 0 |

Per process:

| State | Process | Threads | RSS | PSS | CPU |
| --- | --- | --- | --- | --- | --- |
| open | `audivane-dsp-ui` | 70 | 131 MB | 81 MB | 3.3 % |
| open | `WebKitWebProcess` | 17 | 163 MB | 122 MB | 11.0 % |
| open | `WebKitNetworkProcess` | 9 | 49 MB | 24 MB | 0.0 % |
| tray | `audivane-dsp-ui` | 67 | 115 MB | 85 MB | 0.03 % |
| tray | `WebKitNetworkProcess` | 8 | 49 MB | 29 MB | 0.0 % |
| reopened | `audivane-dsp-ui` | 70 | 136 MB | 84 MB | 3.9 % |
| reopened | `WebKitWebProcess` | 17 | 157 MB | 116 MB | 13.0 % |
| reopened | `WebKitNetworkProcess` | 9 | 49 MB | 24 MB | 0.0 % |

Closing the window does what it says: the `WebKitWebProcess` ends (163 MB RSS
gone) and the popup/OSD helper windows were not made. What stays is the UI
process with WebKit already initialised and the shared network process.

- The UI process gives back 15 MB of RSS when the window closes, but its PSS
  rises from 81 to 85 MB: the WebKit pages it shared with the web process are
  now shared with nobody. Closing does not undo WebKit start-up, which is why
  the closed-to-tray state (115 MB) is much heavier than a `--hidden` start
  that never made a window (59 MB, 16 threads instead of 67).
- Idle in the tray is idle: 0.06 s of CPU and 4 context switches over three
  minutes, with no periodic traffic of our own (the daemon only pushes on
  change).

Where the tray state's 115 MB PSS sits (main process, 85 MB):

| PSS | Backing |
| --- | --- |
| 14.3 MB | `libwebkit2gtk-4.1.so` |
| 9.9 MB | the executable (Rust code and the embedded frontend) |
| 8.8 MB | `libjavascriptcoregtk-4.1.so` |
| 8.4 MB | `libLLVM.so` (software GL, llvmpipe) |
| 5.2 MB | anonymous |
| 4.9 MB | `libgallium` (software GL, llvmpipe) |
| 4.4 MB | `libgtk-3.so` |
| 4.1 MB | heap |
| 2.5 MB | a SysV shared segment WebKit keeps |

The network process adds 29 MB, about 13 MB of it `libwebkit2gtk` and
`libjavascriptcoregtk`. About 13 MB of the main process (LLVM and gallium) is
software rendering and would mostly not be resident on a GPU desktop.

## What a tray-only process without WebKit would cost

A small Rust binary with the same StatusNotifierItem (ksni over zbus, one
icon, a three-entry menu) measured **5.1 MB RSS / 3.5 MB PSS, 5 threads, no
measurable CPU** and no wakeups in a minute. A tray-only process for this app
would add to that the pieces the app already has and that do not need WebKit:
the daemon client and its `status`/`streams`/`config`/`presets` subscription,
`ui_settings`, `volume_step` (guarded volume writes), the tray-host watch, the
single-instance name and the GlobalShortcuts portal. Those are one socket, one
D-Bus connection each and a few threads: estimate **10–15 MB RSS, ~0.05 %
CPU**, about a tenth of the tray's current cost.

The catch is that the popup and the OSD are webviews, and the tray menu, the
wheel and the volume keys are what make the tray useful. A tray-only process
cannot be the app with its GUI code removed: it is a second binary that
answers the tray, the keys and the daemon, and starts the UI process (and so
the popup and OSD webviews) only when it needs one. The window lifecycle
already fits that (windows made on demand and destroyed after 30 s,
`create: false`, the page rebuilt from the daemon's config), so the split is
feasible, but it is a process split, not a flag.

## What could not be measured here

- **A real panel.** No StatusNotifier host runs under Xvfb, so no icon was
  drawn and no host ever opened the menu. The app's own side of both paths was
  exercised over D-Bus; how a panel renders the icon and its menu is not part
  of the app's cost.
- **Audio and meters.** The daemon was in its waiting state (no PipeWire
  session, no DAC). The window therefore shows status without stream meters,
  so the window-open CPU is a lower bound for real playback, and the meter
  path (30 Hz, acknowledged per animation frame) never ran.
- **Hardware GL.** Software rendering makes the open window repaint under
  Xvfb and adds `libLLVM` + `libgallium` to every process, so the 14–21 % of a
  core and ~13 MB are not what a GPU desktop sees. The tray state is a plain
  process with no window to repaint, so its numbers are unaffected.
- **Scale and multi-monitor.** One 1× display only.
- A window that is *minimised* rather than closed was not measured; the
  web process stays in that case.

## Recommendation

The tray costs 164 MB RSS / 115 MB PSS and ~0.03 % CPU; almost all of the
memory is WebKit staying initialised in the UI process (85 MB PSS) plus the
shared `WebKitNetworkProcess` (29 MB PSS). A tray-only process with ksni and
the app's existing D-Bus and socket pieces would cost ~10–15 MB, saving
~150 MB (90 % of the idle footprint), and is worth building if sitting in the
tray at login all day is the resting state. Because the popup and OSD are
webviews, it requires a separate tray shell binary that starts the UI process
on demand rather than a build flag. If the tray is only used briefly while
listening, leaving it as-is is fine.
