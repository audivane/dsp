#!/usr/bin/env bash
# shellcheck shell=bash
#
# Turn a staged tree from build.sh into an AppImage: the app, the engine
# and the avdsp CLI in one file, with packaging/appimage/AppRun as the entry
# point and setup.sh for the per-user install (see packaging/appimage/).
# appimagetool and the AppImage runtime are pinned, checksum-verified and
# downloaded once; appimagetool runs extracted (no FUSE needed to build).
#
# Usage: appimage.sh <stage-dir> <out-dir> <version>
set -euo pipefail

stage=$(cd "${1:?usage: appimage.sh <stage-dir> <out-dir> <version>}" && pwd)
out=${2:?usage: appimage.sh <stage-dir> <out-dir> <version>}
version=${3:?usage: appimage.sh <stage-dir> <out-dir> <version>}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
src=$(cd "$here/../appimage" && pwd)

TOOL_VERSION=1.9.1
TOOL_SHA=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
RUNTIME_VERSION=20251108
RUNTIME_SHA=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
tools=${XDG_CACHE_HOME:-$HOME/.cache}/audivane-release-tools

fetch() { # fetch <url> <dest> <sha256>
  [ -f "$2" ] && printf '%s  %s\n' "$3" "$2" | sha256sum -c --quiet - 2>/dev/null && return
  mkdir -p "$(dirname "$2")"
  curl -fsSL --retry 3 -o "$2.part" "$1"
  printf '%s  %s\n' "$3" "$2.part" | sha256sum -c --quiet -
  mv "$2.part" "$2"
}
tooldir=$tools/appimagetool-$TOOL_VERSION
runtime=$tools/appimage-runtime-$RUNTIME_VERSION/runtime-x86_64
fetch "https://github.com/AppImage/appimagetool/releases/download/$TOOL_VERSION/appimagetool-x86_64.AppImage" \
  "$tooldir/appimagetool-x86_64.AppImage" "$TOOL_SHA"
fetch "https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_VERSION/runtime-x86_64" \
  "$runtime" "$RUNTIME_SHA"
if [ ! -x "$tooldir/squashfs-root/AppRun" ]; then
  chmod +x "$tooldir/appimagetool-x86_64.AppImage"
  (cd "$tooldir" && rm -rf squashfs-root && ./appimagetool-x86_64.AppImage --appimage-extract >/dev/null)
fi

[ -x "$stage/root/usr/bin/audivane-dsp-ui" ] || { echo "error: $stage/root has no staged binaries" >&2; exit 1; }
mkdir -p "$out"
out=$(cd "$out" && pwd)

# AppDir: the staged /usr tree (binaries, user unit, desktop entry, icons,
# licences) plus the entry point, the setup script and the AppImage's own
# top-level desktop entry and icon.
app=$stage/AppDir
rm -rf "$app"
mkdir -p "$app"
cp -a "$stage/root/usr" "$app/usr"
rm -rf "$app/usr/share/doc"
install -m755 "$src/AppRun" "$app/AppRun"
install -Dm755 "$src/setup.sh" "$app/usr/lib/audivane-dsp/setup.sh"
printf '%s\n' "$version" > "$app/usr/lib/audivane-dsp/VERSION"
install -m644 "$src/RUNTIME-LICENSES.txt" "$app/usr/share/licenses/audivane-dsp/APPIMAGE-RUNTIME-LICENSES.txt"
cp "$app/usr/share/applications/audivane-dsp-ui.desktop" "$app/audivane-dsp-ui.desktop"
cp "$app/usr/share/icons/hicolor/256x256/apps/audivane-dsp.png" "$app/audivane-dsp.png"
ln -s audivane-dsp.png "$app/.DirIcon"

# Reproducible: every entry gets the commit time (or now).
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$here" log -1 --format=%ct 2>/dev/null || date +%s)}
find "$app" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +

file=$out/Audivane-DSP-$version-x86_64.AppImage
rm -f "$out"/*.AppImage
# No update information is embedded (no zsync channel).
log=$stage/appimagetool.log
if ! ARCH=x86_64 VERSION=$version "$tooldir/squashfs-root/AppRun" --no-appstream \
    --runtime-file "$runtime" --comp zstd "$app" "$file" > "$log" 2>&1; then
  cat "$log" >&2
  echo "error: appimagetool failed" >&2
  exit 1
fi
echo "AppImage: $(basename "$file") ($(du -k "$file" | cut -f1) KiB)"

# SHA256SUMS over every release file (package.sh wrote it for the packages).
shopt -s nullglob
(cd "$out" && sums=(*.deb *.rpm *.pkg.tar.zst *.AppImage) && sha256sum -- "${sums[@]}" > SHA256SUMS)
