#!/usr/bin/env bash
# shellcheck shell=bash
#
# Run packaging/release/build.sh inside a rootless Ubuntu 24.04 root, so local
# release builds link against the same glibc (2.39) as the oldest supported
# distribution, whatever the host runs. No root, docker or podman needed.
#
#   setup  (once, resumable) pinned Ubuntu 24.04.3 base rootfs + build
#          dependencies, installed with proot (it fakes root for apt/dpkg)
#   build  <source-dir> <stage-dir>: compile inside the root with
#          `unshare -r` + chroot at native speed
#   shell  interactive shell inside the root
#   deb-smoke <file.deb> [<upgrade.deb>]: apt-install a .deb into a
#          throwaway copy of the root and check that every binary resolves
#          its libraries; with <upgrade.deb>, then install that over it
#          (the postinst upgrade path)
#
# The host's Rust toolchain (~/.rustup) and Node (found on PATH) are bound in
# read-only; both are built against old glibc and run unchanged on 24.04.
# Cargo's registry, npm's cache and the target dir live in the cache dir, so
# later builds only recompile what changed.
#
# Environment:
#   AUDIVANE_UBUNTU_ROOT   cache dir (default ~/.cache/audivane-ubuntu-root)
set -euo pipefail

CACHE=${AUDIVANE_UBUNTU_ROOT:-$HOME/.cache/audivane-ubuntu-root}
ROOT=$CACHE/rootfs
BASE_VERSION=24.04.3
BASE_URL="https://cdimage.ubuntu.com/ubuntu-base/releases/24.04/release/ubuntu-base-${BASE_VERSION}-base-amd64.tar.gz"
BASE_SHA=6bc2cde3930ad088b3bb46fa45279e96d25bc3810f209850ecbe4722711874f9
PROOT_URL="https://archive.ubuntu.com/ubuntu/pool/universe/p/proot/proot_5.4.0-3_amd64.deb"
PROOT_SHA=8520a33632dc721ec9404eb8bb0079834dcced02f6e861510238bf636cc629e7
PACKAGES=(
  build-essential pkg-config clang libclang-dev ca-certificates git dpkg-dev
  libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev
  libsoup-3.0-dev libjavascriptcoregtk-4.1-dev
  libpipewire-0.3-dev libspa-0.2-dev libasound2-dev libdbus-1-dev
)
STAMP=$ROOT/.audivane-setup-v1

SCRIPT=$(readlink -f "${BASH_SOURCE[0]}")
log() { printf '==> %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

RUSTUP_TC=$(rustup toolchain list 2>/dev/null | awk '/\(default\)/{print $1; exit}')
RUSTUP_TC=${RUSTUP_TC:-stable-x86_64-unknown-linux-gnu}
TOOLCHAIN=${RUSTUP_HOME:-$HOME/.rustup}/toolchains/$RUSTUP_TC
NODE_DIR=$(dirname "$(dirname "$(readlink -f "$(command -v node)")")")

proot_bin() {
  local d=$CACHE/proot
  if [ ! -x "$d/usr/bin/proot" ]; then
    mkdir -p "$d"
    curl -fsSL --retry 3 -o "$d/proot.deb" "$PROOT_URL"
    printf '%s  %s\n' "$PROOT_SHA" "$d/proot.deb" | sha256sum -c --quiet -
    dpkg-deb -x "$d/proot.deb" "$d"
  fi
  echo "$d/usr/bin/proot"
}

# apt/dpkg need (fake) root and chown; proot -0 provides both.
as_fake_root() {
  PROOT_NO_SECCOMP=1 "$(proot_bin)" -0 -r "$1" -b /proc -b /dev -b /sys \
    -b /etc/resolv.conf -w / /usr/bin/env -i HOME=/root \
    PATH=/usr/sbin:/usr/bin:/sbin:/bin DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8 "${@:2}"
}

cmd_setup() {
  [ -f "$STAMP" ] && { log "baseline root ready: $ROOT"; return; }
  mkdir -p "$CACHE"
  local tarball=$CACHE/ubuntu-base-$BASE_VERSION.tar.gz
  if [ ! -f "$tarball" ]; then
    log "downloading Ubuntu $BASE_VERSION base rootfs"
    curl -fsSL --retry 3 -o "$tarball.part" "$BASE_URL" && mv "$tarball.part" "$tarball"
  fi
  printf '%s  %s\n' "$BASE_SHA" "$tarball" | sha256sum -c --quiet -
  if [ ! -f "$ROOT/etc/os-release" ]; then
    rm -rf "$ROOT"; mkdir -p "$ROOT"
    tar -xzf "$tarball" -C "$ROOT" --exclude='dev/*'
  fi
  log "installing build dependencies (${#PACKAGES[@]} packages + their deps; a few minutes)"
  # systemd arrives as a dependency; its maintainer scripts run systemd-tmpfiles
  # and systemd-sysusers, which crash proot and are meaningless in a build root.
  # Divert both to /bin/true before apt runs, and finish any half-done configure.
  as_fake_root "$ROOT" sh -c 'for b in systemd-tmpfiles systemd-sysusers; do
      dpkg-divert --quiet --local --rename --add /usr/bin/$b
      ln -sf /bin/true /usr/bin/$b
    done
    dpkg --configure -a'
  as_fake_root "$ROOT" sh -c "apt-get -o APT::Sandbox::User=root update -q \
    && apt-get -o APT::Sandbox::User=root install -y -q --no-install-recommends ${PACKAGES[*]} \
    && apt-get clean"
  touch "$STAMP"
  log "baseline root ready: $ROOT"
}

# internal: inside `unshare -r -m`, bind what the build needs and chroot.
cmd_ns_exec() {
  local root=$1; shift
  mount --bind "$root" "$root"
  mount --rbind /dev "$root/dev"
  mount --rbind /sys "$root/sys"
  mount -t proc proc "$root/proc"
  cp /etc/resolv.conf "$root/etc/resolv.conf"
  mkdir -p "$root/opt/rust" "$root/opt/node" "$root/cache" "$root/work"
  mount --bind -o ro "$TOOLCHAIN" "$root/opt/rust"
  mount --bind -o ro "$NODE_DIR" "$root/opt/node"
  mount --bind "$CACHE/cache" "$root/cache"
  [ -n "${WORK_DIR:-}" ] && mount --bind "$WORK_DIR" "$root/work"
  exec chroot "$root" /usr/bin/env -i HOME=/cache/home TERM=dumb LANG=C.UTF-8 \
    PATH=/opt/rust/bin:/opt/node/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    CARGO_HOME=/cache/cargo npm_config_cache=/cache/npm \
    CARGO_TARGET_DIR=/cache/target CARGO_BUILD_JOBS="$(nproc)" \
    SKIP_TESTS="${SKIP_TESTS:-0}" "$@"
}

in_root() {
  [ -f "$STAMP" ] || die "run '$0 setup' first"
  mkdir -p "$CACHE/cache/home" "$CACHE/cache/cargo" "$CACHE/cache/npm" "$CACHE/cache/target"
  WORK_DIR=${WORK_DIR:-} TOOLCHAIN=$TOOLCHAIN NODE_DIR=$NODE_DIR SKIP_TESTS=${SKIP_TESTS:-0} \
    unshare -r -m -p -f "$SCRIPT" _ns_exec "$ROOT" "$@"
}

# build <source-dir> <stage-dir>: both must sit under one work dir we bind.
cmd_build() {
  local src stage work
  src=$(cd "${1:?build <source-dir> <stage-dir>}" && pwd)
  mkdir -p "${2:?build <source-dir> <stage-dir>}"; stage=$(cd "$2" && pwd)
  work=$(dirname "$src")
  case $stage in "$work"/*) ;; *) die "stage dir must be inside $work" ;; esac
  WORK_DIR=$work in_root /bin/bash "/work/${src#"$work"/}/packaging/release/build.sh" \
    "/work/${src#"$work"/}" "/work/${stage#"$work"/}"
}

cmd_deb_smoke() {
  local deb deb2= copy
  deb=$(readlink -f "${1:?deb-smoke <file.deb> [<upgrade.deb>]}")
  [ -z "${2:-}" ] || deb2=$(readlink -f "$2")
  copy=$CACHE/smoke-root
  rm -rf "$copy"; cp -a "$ROOT" "$copy"
  cp "$deb" "$copy/tmp/pkg.deb"
  [ -z "$deb2" ] || cp "$deb2" "$copy/tmp/pkg2.deb"
  log "apt install $(basename "$deb") into a throwaway copy of the root"
  # The postinst note goes to stdout; apt's own chatter is dropped.
  as_fake_root "$copy" sh -c 'apt-get -o APT::Sandbox::User=root update -qq \
    && apt-get -o APT::Sandbox::User=root install -y -qq --no-install-recommends /tmp/pkg.deb > /tmp/install.log \
    && grep -A7 "^==> Audivane DSP installed" /tmp/install.log \
    && dpkg -s audivane-dsp | sed -n "s/^Version: /installed version /p" \
    && for b in /usr/bin/audivane-dsp /usr/bin/avdsp /usr/bin/audivane-dsp-ui; do
         if ldd "$b" | grep -q "not found"; then echo "MISSING libs for $b"; ldd "$b" | grep "not found"; exit 1; fi
         echo "  ok   $b"; done'
  if [ -n "$deb2" ]; then
    log "apt install $(basename "$deb2") over it (upgrade)"
    as_fake_root "$copy" sh -c 'apt-get -o APT::Sandbox::User=root install -y -qq --no-install-recommends --reinstall --allow-downgrades /tmp/pkg2.deb > /tmp/upgrade.log \
      && grep -A3 "^==> " /tmp/upgrade.log \
      && grep -q "^==> Audivane DSP updated" /tmp/upgrade.log \
      && dpkg -s audivane-dsp | sed -n "s/^Version: /upgraded to /p"' \
      || { cat "$copy/tmp/upgrade.log" >&2 2>/dev/null; rm -rf "$copy"; die "deb upgrade smoke test failed"; }
  fi
  rm -rf "$copy"
  log "deb smoke test passed"
}

case ${1:-} in
  setup) cmd_setup ;;
  build) shift; cmd_build "$@" ;;
  shell) in_root /bin/bash -i ;;
  deb-smoke) shift; cmd_deb_smoke "$@" ;;
  _ns_exec) shift; cmd_ns_exec "$@" ;;
  *) sed -n '3,24p' "$SCRIPT" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
