#!/usr/bin/env bash
# shellcheck shell=bash
#
# Turn a staged tree from build.sh into .deb, .rpm and .pkg.tar.zst with nfpm
# (pinned, checksum-verified, downloaded once), plus SHA256SUMS.
#
# Usage: package.sh <stage-dir> <out-dir> <version>
set -euo pipefail

stage=$(cd "${1:?usage: package.sh <stage-dir> <out-dir> <version>}" && pwd)
out=${2:?usage: package.sh <stage-dir> <out-dir> <version>}
version=${3:?usage: package.sh <stage-dir> <out-dir> <version>}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

NFPM_VERSION=2.48.0
NFPM_SHA=6d7f9a28b2f397934403950fcbba83f598b08d02edbe79a0815b3b4897341750
tools=${XDG_CACHE_HOME:-$HOME/.cache}/audivane-release-tools
nfpm=$tools/nfpm-$NFPM_VERSION/nfpm
if [ ! -x "$nfpm" ]; then
  mkdir -p "$(dirname "$nfpm")"
  tgz=$tools/nfpm_${NFPM_VERSION}_Linux_x86_64.tar.gz
  curl -fsSL --retry 3 -o "$tgz" \
    "https://github.com/goreleaser/nfpm/releases/download/v$NFPM_VERSION/nfpm_${NFPM_VERSION}_Linux_x86_64.tar.gz"
  printf '%s  %s\n' "$NFPM_SHA" "$tgz" | sha256sum -c --quiet -
  tar -xzf "$tgz" -C "$(dirname "$nfpm")" nfpm
fi

[ -d "$stage/root/usr/bin" ] || { echo "error: $stage/root has no staged binaries" >&2; exit 1; }
mkdir -p "$out"
out=$(cd "$out" && pwd)

# Runtime dependencies per distribution. Debian's library list comes from
# dpkg-shlibdeps (build.sh); the tray library is loaded at runtime, so it is
# listed explicitly everywhere, like the PipeWire session itself.
deb_libs=$(cat "$stage/deb-depends.txt" 2>/dev/null || true)
[ -n "$deb_libs" ] || { echo "error: $stage/deb-depends.txt is missing or empty" >&2; exit 1; }
yaml_list() { tr ',' '\n' | sed 's/^ *//; s/ *$//; /^$/d; s/.*/      - "&"/'; }

# Reproducible timestamps: every entry gets the commit time (or now).
mtime=$(date -u -d "@${SOURCE_DATE_EPOCH:-$(git -C "$here" log -1 --format=%ct 2>/dev/null || date +%s)}" +%Y-%m-%dT%H:%M:%SZ)
# Files are listed one by one (no `type: tree`): nfpm's tree entries give
# Arch's .MTREE directory modes that pacman reports as permission mismatches.
config=$stage/nfpm.yaml
cat > "$config" <<EOF
name: audivane-dsp
arch: amd64
platform: linux
version: "$version"
release: "1"
section: sound
priority: optional
maintainer: "Triet (@Triet1402)"
vendor: Audivane
homepage: https://audivane.app/dsp/
license: AGPL-3.0-only
description: |-
  Bit-perfect system-wide equalizer, convolution and upsampling for PipeWire.
  A background engine that owns the DAC and drives PipeWire from its clock,
  plus a desktop app to control it.
mtime: "$mtime"
contents:
$(cd "$stage/root" && find usr -type f | sort | while read -r f; do
  printf '  - src: %s\n    dst: /%s\n    file_info:\n      mode: 0%s\n' "$stage/root/$f" "$f" "$(stat -c %a "$f")"
done)
scripts:
  postinstall: $here/postinstall.sh
overrides:
  deb:
    depends:
$(printf '%s' "$deb_libs" | yaml_list)
      - "pipewire"
      - "wireplumber"
      - "libayatana-appindicator3-1"
      - "dbus"
    recommends:
      - "rtkit"
  rpm:
    depends:
      - "pipewire"
      - "wireplumber"
      - "pipewire-libs"
      - "alsa-lib"
      - "webkit2gtk4.1"
      - "gtk3"
      - "libayatana-appindicator-gtk3"
      - "dbus"
      - "glibc >= 2.39"
    recommends:
      - "rtkit"
  archlinux:
    depends:
      - "pipewire"
      - "wireplumber"
      - "alsa-lib"
      - "webkit2gtk-4.1"
      - "gtk3"
      - "libayatana-appindicator"
      - "dbus"
      - "gcc-libs"
      - "glibc"
    suggests:
      - "rtkit"
rpm:
  group: Applications/Multimedia
  summary: System-wide bit-perfect DSP for PipeWire
archlinux:
  packager: "Triet (@Triet1402)"
EOF

rm -f "$out"/*.deb "$out"/*.rpm "$out"/*.pkg.tar.zst "$out/SHA256SUMS"
for packager in deb rpm archlinux; do
  "$nfpm" package --config "$config" --packager "$packager" --target "$out/" | sed 's/^/  /'
done
(cd "$out" && sha256sum -- *.deb *.rpm *.pkg.tar.zst > SHA256SUMS)
echo "packages in $out:"
(cd "$out" && ls -l --block-size=K -- *.deb *.rpm *.pkg.tar.zst | awk '{print "  " $5 "  " $9}')
