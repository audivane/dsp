#!/usr/bin/env bash
# shellcheck shell=bash
#
# Build the release binaries and stage the package tree.
#
# Must run on the Ubuntu 24.04 baseline (glibc 2.39) so the binaries also run
# on Debian/Ubuntu 24.04+, Fedora 40+ and Arch: natively on a GitHub
# ubuntu-24.04 runner, or locally inside the rootless root that
# packaging/release/baseline.sh provides. packaging/release/package.sh then
# turns the staged tree into .deb, .rpm and .pkg.tar.zst.
#
# Usage: build.sh <source-dir> <stage-dir>
#   CARGO_TARGET_DIR   where cargo builds (default <source-dir>/target);
#                      keep it between runs for fast rebuilds
#   SKIP_TESTS=1       skip the protocol unit tests
#
# Output in <stage-dir>:
#   root/              the file tree every package installs
#   deb-depends.txt    shared-library dependencies (dpkg-shlibdeps)
#   timings.txt        seconds per stage
set -euo pipefail

src=$(cd "${1:?usage: build.sh <source-dir> <stage-dir>}" && pwd)
stage=${2:?usage: build.sh <source-dir> <stage-dir>}
target=${CARGO_TARGET_DIR:-$src/target}
mkdir -p "$stage"
stage=$(cd "$stage" && pwd)
export CARGO_TERM_COLOR=never

log() { printf '\n==> %s\n' "$*"; }
timings=()
stage_start() { _t0=$(date +%s); log "$1"; _name=$1; }
stage_end() { timings+=("$(( $(date +%s) - _t0 ))s  $_name"); }

grep -q '^VERSION_ID="24.04"' /etc/os-release 2>/dev/null \
  || echo "warning: not Ubuntu 24.04; the binaries may need a newer glibc than the 24.04 baseline" >&2

cd "$src"

stage_start "frontend (npm ci + vite build)"
(cd app && npm ci --no-audit --no-fund --loglevel=error && npm run build)
stage_end

# The engine (fat LTO, one codegen unit) and the desktop app (thin LTO, see
# [profile.release-ui]) build at the same time in separate target dirs, so the
# engine's single-threaded LTO link overlaps the app's parallel compile.
stage_start "rust: engine + CLI (release) and app (release-ui) in parallel"
cargo build --release --locked --target-dir "$target/engine" \
  -p audivane-dsp-daemon -p audivane-dsp-cli > "$stage/engine.log" 2>&1 &
engine=$!
cargo build --profile release-ui --locked --target-dir "$target/app" \
  -p audivane-dsp-ui --features custom-protocol > "$stage/app.log" 2>&1 &
app=$!
status=0
wait "$engine" || { status=1; echo "engine build failed:"; tail -40 "$stage/engine.log"; }
wait "$app" || { status=1; echo "app build failed:"; tail -40 "$stage/app.log"; }
grep -h 'Finished' "$stage/engine.log" "$stage/app.log" || true
[ "$status" = 0 ] || exit 1
stage_end

if [ "${SKIP_TESTS:-0}" != 1 ]; then
  # In the engine's target dir: the release profile and its dependencies are
  # already built there (no third target dir).
  stage_start "protocol unit tests (release)"
  cargo test --release --locked --target-dir "$target/engine" -p audivane-protocol -q
  stage_end
fi

stage_start "stage package tree"
rm -rf "$stage/root"
r=$stage/root
install -Dm755 "$target/engine/release/audivane-dsp" "$r/usr/bin/audivane-dsp"
install -Dm755 "$target/engine/release/avdsp" "$r/usr/bin/avdsp"
install -Dm755 "$target/app/release-ui/audivane-dsp-ui" "$r/usr/bin/audivane-dsp-ui"
strip --strip-unneeded "$r/usr/bin/audivane-dsp" "$r/usr/bin/avdsp" "$r/usr/bin/audivane-dsp-ui"

install -Dm644 packaging/audivane-dsp.service "$r/usr/lib/systemd/user/audivane-dsp.service"
sed -i 's#^ExecStart=.*#ExecStart=/usr/bin/audivane-dsp --foreground#' \
  "$r/usr/lib/systemd/user/audivane-dsp.service"
# File name = the window's Wayland app ID (the binary name); see PKGBUILD.
install -Dm644 packaging/arch/audivane-dsp-ui.desktop "$r/usr/share/applications/audivane-dsp-ui.desktop"
install -Dm644 assets/brand/audivane-dsp-icon.svg "$r/usr/share/icons/hicolor/scalable/apps/audivane-dsp.svg"
for size in 16 24 32 48 64 128 256 512; do
  install -Dm644 "assets/brand/icons/audivane-$size.png" \
    "$r/usr/share/icons/hicolor/${size}x${size}/apps/audivane-dsp.png"
done
install -Dm644 LICENSE "$r/usr/share/licenses/audivane-dsp/LICENSE"
# Third-party notices: crates and npm packages in the binaries, the bundled
# fonts (renamed IBM Plex subsets, SIL OFL 1.1) and the target-curve data.
lic=$r/usr/share/licenses/audivane-dsp
install -Dm644 packaging/THIRD_PARTY_LICENSES.txt "$lic/THIRD_PARTY_LICENSES.txt"
install -Dm644 app/src/assets/fonts/OFL.txt "$lic/OFL-IBM-Plex.txt"
install -Dm644 docs/font-licensing.md "$lic/font-licensing.md"
install -Dm644 app/src/assets/targets/SOURCES.md "$lic/targets-SOURCES.md"
install -Dm644 app/src/assets/research-targets/SOURCES.md "$lic/research-targets-SOURCES.md"
install -Dm644 DEV.md "$r/usr/share/doc/audivane-dsp/DEV.md"
install -Dm644 docs/control-plane.md "$r/usr/share/doc/audivane-dsp/control-plane.md"

# Debian dependencies from the libraries the binaries actually link
# (dpkg-shlibdeps needs a debian/control in the working directory).
if command -v dpkg-shlibdeps >/dev/null; then
  tmp=$(mktemp -d)
  mkdir -p "$tmp/debian"
  printf 'Source: audivane-dsp\n\nPackage: audivane-dsp\nArchitecture: amd64\n' > "$tmp/debian/control"
  (cd "$tmp" && dpkg-shlibdeps -O "$r/usr/bin/audivane-dsp" "$r/usr/bin/avdsp" "$r/usr/bin/audivane-dsp-ui" 2>/dev/null) \
    | sed -n 's/^shlibs:Depends=//p' > "$stage/deb-depends.txt"
  rm -rf "$tmp"
  echo "deb shared-library depends: $(cat "$stage/deb-depends.txt")"
fi
stage_end

printf '%s\n' "${timings[@]}" > "$stage/timings.txt"
log "build timings"
cat "$stage/timings.txt"
