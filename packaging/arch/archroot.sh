#!/usr/bin/env bash
# shellcheck shell=bash
#
# ============================================================================
#  archroot.sh — rootless Arch Linux build root for Audivane
# ============================================================================
#
#  Why
#  ---
#  On a non-Arch dev box (e.g. Ubuntu) there is no makepkg, no pacman and no
#  WebKitGTK 4.1 dev headers. To build the real `audivane-dsp` package and to
#  smoke-test installing it we run makepkg inside a real Arch Linux rootfs
#  (the official bootstrap tarball) without root, docker, podman or qemu.
#
#  Usage
#  -----
#    packaging/arch/archroot.sh init            # one-time (resumable): ~1 min
#    packaging/arch/archroot.sh verify          # toolchain / Tauri prereqs
#    packaging/arch/archroot.sh run <cmd...>    # run a command inside the root
#    packaging/arch/archroot.sh makepkg <dir>   # build <dir>/PKGBUILD, copy *.pkg.tar.zst back
#    packaging/arch/archroot.sh smoke <pkg> [<pkg2>]
#                                               # pacman -U into a throwaway overlay + ldd every ELF;
#                                               # with <pkg2>, then upgrade to it (post_upgrade)
#    packaging/arch/archroot.sh selftest        # build+install a hello-world package
#    packaging/arch/archroot.sh path            # print the cache dir
#
#  `run` joins its arguments with spaces and executes them with bash inside the
#  root, so both `archroot.sh run "pacman -Q"` and `archroot.sh run pacman -Q`
#  work; the shell starts in $HOME (/build). Other scripts can use the stable
#  wrapper `~/.cache/audivane-archroot/enter.sh <cmd...>`, which is equivalent
#  to `run` and is (re)written on every invocation.
#
#  Environment overrides:
#    ARCHROOT_CACHE      cache dir (default ~/.cache/audivane-archroot)
#    ARCHROOT_BINDS      colon-separated host paths to bind at the same path
#                        inside the root (default: the checkout this script
#                        lives in)
#    ARCHROOT_MIRROR     Arch mirror base URL
#    ARCHROOT_BUILDER_UID / ARCHROOT_BUILDER_GID   uid/gid makepkg runs as
#
#  `run`, `makepkg` and `smoke` propagate the exit status of what they run, so
#  they are safe to use from CI/scripts.
#
#  Layout (nothing is written outside ~/.cache/audivane-archroot, the repo
#  worktree and temp dirs):
#    ~/.cache/audivane-archroot/
#      dl/          bootstrap tarball + sha256sums
#      root/        the Arch rootfs (592 MiB base, ~3.5 GiB after init)
#      state/       step markers (keyring initialised, bootstrap version)
#      logs/        bootstrap/install logs
#      tmp/         scratch dirs for `smoke`
#      enter.sh     wrapper -> archroot.sh run "$@"
#      READY        written when `init` finished and `verify` passed
#
#  How it works
#  ------------
#  * `init` downloads https://geo.mirror.pkgbuild.com/iso/latest/
#    archlinux-bootstrap-x86_64.tar.zst, checks it against the published
#    sha256sums.txt, and extracts it (`root.x86_64/` stripped) inside
#    `unshare -r` with tar --no-same-owner: with a single mapped uid every file
#    must become that uid, which is exactly what pacman would do for root-owned
#    files anyway.
#  * Every command runs in `unshare -r -m -p -f` (user+mount+pid namespace,
#    uid 0 mapped to the invoking user) + chroot. Mounts: the rootfs bound onto
#    itself (so `/proc/mounts` has a `/` entry, which pacman's free-space check
#    needs — without it pacman aborts with "could not determine cachedir mount
#    point"), /dev and /sys from the host, a fresh /proc, a copy of the host's
#    /etc/resolv.conf, and the host checkout bind-mounted at its own path.
#    Network is the host's (no netns), so mirrors work.
#  * pacman needs three tweaks for a one-uid namespace (all applied to the
#    rootfs' /etc/pacman.conf by `init`, idempotently):
#      - `DownloadUser` is commented out: pacman 7 wants to setuid to the
#        `alpm` user for its download sandbox, and that uid is not mapped here.
#      - `DisableSandboxFilesystem` / `DisableSandboxSyscalls` are enabled for
#        the same reason (landlock/seccomp sandbox for downloads).
#      - `CheckSpace` stays on; it works now that `/` is a visible mountpoint.
#    File extraction by pacman works: ownership to the single mapped uid
#    succeeds (everything ends up owned by the invoking user) and libarchive
#    only warns for the few packages that ship files with another gid (e.g.
#    util-linux's setgid `wall`/`write`); pacman still exits 0 and the setgid
#    bit is dropped. Harmless for building.
#  * makepkg refuses to run as uid 0, and after chroot a *nested* user
#    namespace cannot be created (the kernel returns EPERM for
#    unshare(CLONE_NEWUSER) once the process root is not the namespace root).
#    So the nesting happens *before* the chroot: `unshare -U --map-user=<uid>
#    --map-group=<gid> --keep-caps` (uid 0 in the outer namespace -> uid <uid>
#    in the inner one, <uid> = the invoking host uid) and then chroot as that
#    uid. --keep-caps is needed because changing to a non-zero uid would
#    otherwise drop the CAP_SYS_CHROOT we need for the chroot; makepkg only
#    looks at EUID, so it happily builds as uid <uid>. fakeroot (from
#    base-devel) then fakes root ownership for the package tarball as usual.
#  * `smoke` never touches the real rootfs: it mounts an overlayfs with the
#    rootfs as lowerdir and a scratch upperdir, runs `pacman -U` inside it, then
#    prints `pacman -Qi`, `pacman -Qlq`, runs `ldd` on every ELF file and
#    `pacman -Qkk`, and drops the upperdir. Unprivileged overlayfs works in
#    this user namespace (mounted with the `userxattr` option).
#
#  Limitations / notes
#  -------------------
#  * Only ONE uid/gid is mapped (no newuidmap on this host, /etc/subuid is
#    useless without it), so inside the root every file is owned by you: there
#    are no real per-package uids, setuid/setgid bits are not honoured, and
#    packages that need a second owner keep the single mapped uid. Building and
#    smoke-installing packages is unaffected.
#  * `makepkg -si` / `--syncdeps` do not work inside the root (they want sudo);
#    `archroot.sh makepkg` resolves and installs dependencies with pacman as
#    root before building instead, and falls back to `--nodeps` (loudly) if a
#    dependency is not in the repos.
#  * systemd-related post-install hooks log "Skipped: Current root is not
#    booted." and a few tmpfiles chown warnings on /dev and /sys — expected,
#    pacman still exits 0. makepkg sometimes prints "libfakeroot internal
#    error: payload not recognized!"; the package is still filed as root:root,
#    so it is cosmetic.
#  * Nothing on the host is installed or modified; no sudo is used. The script
#    needs `unshare` (util-linux) on the host, `tar`, `zstd` and `curl`.
#  * The rootfs is shared between concurrent invocations: several `run` calls
#    are fine, but do not run two `pacman` transactions at the same time.
#
# ============================================================================

set -euo pipefail

SCRIPT=$(readlink -f -- "${BASH_SOURCE[0]}")

CACHE=${ARCHROOT_CACHE:-$HOME/.cache/audivane-archroot}
ROOT=$CACHE/root
DL=$CACHE/dl
STATE=$CACHE/state
LOGS=$CACHE/logs
READY=$CACHE/READY
ENTER=$CACHE/enter.sh

MIRROR=${ARCHROOT_MIRROR:-https://geo.mirror.pkgbuild.com}
BOOTSTRAP=archlinux-bootstrap-x86_64.tar.zst
BOOTSTRAP_URL=$MIRROR/iso/latest/$BOOTSTRAP
SHA256SUMS_URL=$MIRROR/iso/latest/sha256sums.txt

# host paths bind-mounted into the root at the same path: by default only this
# checkout (which may be a git worktree), so `run` sees the tree we package
_repo_root=$(git -C "$(dirname "$SCRIPT")" rev-parse --show-toplevel 2>/dev/null || true)
if [ -n "${ARCHROOT_BINDS:-}" ]; then
  EXTRA_BINDS=$ARCHROOT_BINDS
else
  EXTRA_BINDS=$_repo_root
fi

# the non-root uid/gid used to run makepkg (uid 0 in the outer namespace maps
# onto this uid inside the root, so it must not be 0)
BUILDER_UID=${ARCHROOT_BUILDER_UID:-$(id -u)}
BUILDER_GID=${ARCHROOT_BUILDER_GID:-$(id -g)}

# packages the build root must provide. `cargo` is not a package of its own in
# Arch (the `rust` package provides it) — missing names are reported, not fatal.
WANTED=(
  base-devel git rust cargo nodejs npm pnpm
  webkit2gtk-4.1 gtk3 libayatana-appindicator librsvg
  pipewire wireplumber alsa-lib dbus openssl clang mold sccache
)

NS_PATH=/usr/local/sbin:/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin

# ---------------------------------------------------------------------------
# output helpers
# ---------------------------------------------------------------------------
log()  { printf '==> %s\n' "$*" >&2; }
info() { printf '  -> %s\n' "$*" >&2; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# internal: run inside the fresh namespaces (invoked as `archroot.sh _ns_exec`)
#   _ns_exec <root> <root|builder> <cmd...>
# ---------------------------------------------------------------------------
cmd_ns_exec() {
  local root=$1 mode=$2
  shift 2

  # A self-bind gives the chroot dir a real "/" mountpoint in /proc/mounts.
  # pacman's CheckSpace resolves the cachedir's mountpoint from /proc/mounts
  # and gives up when no mount covers the path (which is the case for a chroot
  # that is a plain subdirectory). This must happen before the other mounts so
  # they land on top of it.
  mount --bind "$root" "$root"
  mount --rbind /dev "$root/dev"
  mount --rbind /sys "$root/sys"
  mount -t proc proc "$root/proc"

  mkdir -p "$root/etc" "$root/build" "$root/tmp"
  rm -f "$root/etc/resolv.conf"
  cp /etc/resolv.conf "$root/etc/resolv.conf"

  local p
  for p in ${EXTRA_BINDS//:/ }; do
    [ -e "$p" ] || continue
    mkdir -p "$root$p"
    mount --bind "$p" "$root$p"
  done

  if [ "$mode" = builder ]; then
    [ "$BUILDER_UID" != 0 ] || die "builder uid must not be 0 (cannot run makepkg as root)"
    # Nested user namespace, created here because the kernel refuses
    # CLONE_NEWUSER once the process is chrooted. uid 0 (outer) -> BUILDER_UID
    # (inner); --keep-caps retains CAP_SYS_CHROOT across the uid change.
    exec unshare -U --map-user="$BUILDER_UID" --map-group="$BUILDER_GID" --keep-caps \
      chroot "$root" /usr/bin/env -i \
        HOME=/build USER=builder LOGNAME=builder TERM=dumb \
        PATH="$NS_PATH" LANG=C.UTF-8 LC_ALL=C.UTF-8 \
        MAKEFLAGS="-j$(nproc)" \
        "$@"
  else
    exec chroot "$root" /usr/bin/env -i \
      HOME=/build USER=root LOGNAME=root TERM=dumb \
      PATH="$NS_PATH" LANG=C.UTF-8 LC_ALL=C.UTF-8 \
      "$@"
  fi
}

# _ns_run <root|builder> <cmd...>   (stdin is forwarded)
_ns_run() {
  local mode=$1
  shift
  # the builder uid/gid must be captured outside the namespace: inside it we
  # are uid 0 and `id -u` would give 0 again (which would map 0 -> 0)
  ARCHROOT_BUILDER_UID=$BUILDER_UID ARCHROOT_BUILDER_GID=$BUILDER_GID \
    unshare -r -m -p -f "$SCRIPT" _ns_exec "$ROOT" "$mode" "$@"
}

# run a multi-line script inside the root: _ns_script <mode> [args...] <<'EOS' ... EOS
_ns_script() {
  local mode=$1
  shift
  # `--` keeps $0 as the shell name, so "$@" receives every argument as-is
  _ns_run "$mode" /bin/bash -s -- "$@"
}

# ---------------------------------------------------------------------------
# setup helpers
# ---------------------------------------------------------------------------
ensure_dirs() {
  mkdir -p "$DL" "$STATE" "$LOGS" "$CACHE/tmp"
}

ensure_enter_sh() {
  mkdir -p "$CACHE"
  cat > "$ENTER.tmp.$$" <<EOS
#!/bin/bash
# Generated by $SCRIPT — enter the Arch build root.
# Usage: $ENTER <cmd...>   (same as: $SCRIPT run <cmd...>)
exec $(printf '%q' "$SCRIPT") run "\$@"
EOS
  chmod 755 "$ENTER.tmp.$$"
  mv -f "$ENTER.tmp.$$" "$ENTER"
}

# rm -rf that survives the mode-000 dirs overlayfs leaves in its workdir
_rm_rf() {
  [ -e "$1" ] || return 0
  chmod -R u+rwX -- "$1" 2>/dev/null || true
  rm -rf -- "$1"
}

need_root() {
  [ -x "$ROOT/usr/bin/pacman" ] || die "no Arch root at $ROOT — run: $SCRIPT init"
  if [ ! -f "$READY" ] && [ "${IN_INIT:-0}" != 1 ]; then
    warn "$READY is missing — run: $SCRIPT init"
  fi
}

# ---------------------------------------------------------------------------
# init
# ---------------------------------------------------------------------------
download_bootstrap() {
  ensure_dirs
  local sums=$DL/sha256sums.txt tarball=$DL/$BOOTSTRAP

  if [ -f "$tarball" ] && [ -f "$sums" ] &&
     (cd "$DL" && grep " $BOOTSTRAP\$" sha256sums.txt | sha256sum -c --status -); then
    info "bootstrap tarball already verified: $tarball"
    return 0
  fi

  log "downloading $BOOTSTRAP"
  curl -fL --retry 3 --retry-delay 2 -o "$tarball.part" "$BOOTSTRAP_URL"
  mv -f "$tarball.part" "$tarball"
  curl -fsSL -o "$sums.part" "$SHA256SUMS_URL"
  mv -f "$sums.part" "$sums"

  log "verifying sha256"
  ( cd "$DL" && grep " $BOOTSTRAP\$" sha256sums.txt | sha256sum -c - ) ||
    die "sha256 mismatch for $tarball"
}

extract_bootstrap() {
  if [ -x "$ROOT/usr/bin/pacman" ]; then
    info "Arch root already extracted: $ROOT ($(du -sh "$ROOT" 2>/dev/null | cut -f1))"
    return 0
  fi

  ensure_dirs
  local tmp=$CACHE/root.extract.$$
  log "extracting bootstrap into $ROOT"
  rm -rf "$tmp"; mkdir -p "$tmp"
  # --no-same-owner: only one uid is mapped, so everything must be owned by it
  # (the tarball's top-level dir is `root.x86_64/`).
  if ! unshare -r tar --zstd --numeric-owner --no-same-owner \
        -xf "$DL/$BOOTSTRAP" -C "$tmp" --strip-components=1 \
        > "$LOGS/extract.log" 2>&1; then
    grep -v "Ignoring unknown extended header keyword" "$LOGS/extract.log" >&2 || true
    # tar only fails here for the handful of files with a foreign gid; check
    # for the marker of a complete extraction before giving up.
    [ -x "$tmp/usr/bin/pacman" ] || die "bootstrap extraction failed (see $LOGS/extract.log)"
    warn "tar reported ownership warnings for files with non-root gids (expected)"
  fi
  [ -x "$tmp/usr/bin/pacman" ] || die "bootstrap extraction incomplete (see $LOGS/extract.log)"
  mv -f "$tmp" "$ROOT"
  unshare -r tar --zstd -xOf "$DL/$BOOTSTRAP" version 2>/dev/null \
    > "$STATE/bootstrap-version" || echo unknown > "$STATE/bootstrap-version"
}

configure_root() {
  log "configuring mirror, pacman and makepkg"

  local mirrorlist=$ROOT/etc/pacman.d/mirrorlist
  cat > "$mirrorlist" <<EOS
# written by archroot.sh
Server = $MIRROR/\$repo/os/\$arch
Server = https://mirror.rackspace.com/archlinux/\$repo/os/\$arch
EOS

  local conf=$ROOT/etc/pacman.conf
  # pacman 7 downloads through a sandboxed child running as the `alpm` user.
  # Neither the uid switch nor the landlock setup can work in a one-uid user
  # namespace, so turn both off (idempotent).
  sed -i \
    -e 's/^DownloadUser[[:space:]]*=.*/#DownloadUser = alpm  # disabled by archroot.sh (one-uid userns)/' \
    -e 's/^#[[:space:]]*DisableSandboxFilesystem/DisableSandboxFilesystem/' \
    -e 's/^#[[:space:]]*DisableSandboxSyscalls/DisableSandboxSyscalls/' \
    "$conf"
  grep -q '^DisableSandboxFilesystem' "$conf" || printf '\nDisableSandboxFilesystem\n' >> "$conf"
  grep -q '^DisableSandboxSyscalls' "$conf" || printf 'DisableSandboxSyscalls\n' >> "$conf"

  # pacman's CheckSpace needs the cachedir to exist before the first -Syu
  mkdir -p "$ROOT/var/cache/pacman/pkg" "$ROOT/var/lib/pacman/local" \
           "$ROOT/build" "$ROOT/build/makepkg" "$ROOT/build/srcdest" \
           "$ROOT/etc/pacman.d/gnupg"
  chmod 1777 "$ROOT/tmp"

  # makepkg: use all cores, keep downloaded sources between builds, and skip
  # the -debug split package (nobody installs it here, and it doubles build
  # time). The marker block is rewritten on every init.
  local mk=$ROOT/etc/makepkg.conf
  sed -i '/^OPTIONS=/ s/ debug / !debug /' "$mk"
  sed -i '/^# >>> archroot.sh >>>$/,/^# <<< archroot.sh <<<$/d' "$mk"
  cat >> "$mk" <<EOS
# >>> archroot.sh >>>
MAKEFLAGS="-j$(nproc)"
SRCDEST=/build/srcdest
# <<< archroot.sh <<<
EOS

  # a passwd/group entry for the build uid so `id -un`/`whoami` work inside
  if ! grep -q ":x:$BUILDER_UID:" "$ROOT/etc/passwd"; then
    printf 'builder:x:%s:%s:Arch build user:/build:/bin/bash\n' \
      "$BUILDER_UID" "$BUILDER_GID" >> "$ROOT/etc/passwd"
  fi
  if ! grep -q ":x:$BUILDER_GID:" "$ROOT/etc/group"; then
    printf 'builder:x:%s:\n' "$BUILDER_GID" >> "$ROOT/etc/group"
  fi

  printf '%s\n' "${WANTED[*]}" > "$ROOT/build/.archroot-wanted"
}

init_keyring() {
  if [ -f "$STATE/keyring.ok" ] && [ -s "$ROOT/etc/pacman.d/gnupg/pubring.gpg" ]; then
    info "pacman keyring already initialised"
    return 0
  fi
  log "initialising pacman keyring"
  _ns_script root > "$LOGS/keyring.log" 2>&1 <<'EOS' || { tail -20 "$LOGS/keyring.log" >&2; die "pacman-key failed (see $LOGS/keyring.log)"; }
set -e
pacman-key --init
pacman-key --populate archlinux
EOS
  date -Is > "$STATE/keyring.ok"
  info "keyring ready"
}

sync_and_install() {
  log "pacman -Syu"
  if ! _ns_run root pacman -Syu --noconfirm --disable-download-timeout \
        > "$LOGS/syu.log" 2>&1; then
    # a stale keyring is the usual cause of a failed sync after a while
    warn "pacman -Syu failed, refreshing keyring and retrying"
    _ns_script root > "$LOGS/keyring.log" 2>&1 <<'EOS' || true
set -e
pacman-key --populate archlinux
pacman-key --updatedb
EOS
    if ! _ns_run root pacman -Syu --noconfirm --disable-download-timeout \
          >> "$LOGS/syu.log" 2>&1; then
      tail -30 "$LOGS/syu.log" >&2
      die "pacman -Syu failed (see $LOGS/syu.log)"
    fi
  fi
  grep -E '^(upgrading|installing)' "$LOGS/syu.log" | tail -5 >&2 || true

  log "installing build packages"
  _ns_script root "${WANTED[@]}" > "$LOGS/install.log" 2>&1 <<'EOS' || { tail -30 "$LOGS/install.log" >&2; die "package installation failed (see $LOGS/install.log)"; }
set -u
ok=()
missing=()
rc=0
for p in "$@"; do
  if pacman -Si "$p" >/dev/null 2>&1 || pacman -Sg "$p" >/dev/null 2>&1; then
    ok+=("$p")
  else
    missing+=("$p")
  fi
done
if [ ${#ok[@]} -gt 0 ]; then
  pacman -S --needed --noconfirm --disable-download-timeout "${ok[@]}" || rc=1
fi
if [ ${#missing[@]} -gt 0 ]; then
  echo "ARCHROOT-MISSING: ${missing[*]}"
  for p in "${missing[@]}"; do
    if pacman -T "$p" >/dev/null 2>&1; then
      echo "ARCHROOT-PROVIDED: $p"
    fi
  done
fi
exit $rc
EOS
  local provided missing real="" m
  provided=$(sed -n 's/^ARCHROOT-PROVIDED: //p' "$LOGS/install.log" | xargs)
  missing=$(sed -n 's/^ARCHROOT-MISSING: //p' "$LOGS/install.log" | xargs)
  if [ -n "$provided" ]; then
    info "not separate Arch packages (provided by another package): $provided"
  fi
  for m in $missing; do
    case " $provided " in
      *" $m "*) ;;
      *) real="$real $m" ;;
    esac
  done
  if [ -n "${real// /}" ]; then
    warn "not available in the Arch repos, skipped:$real"
  fi
}

cmd_init() {
  ensure_dirs
  log "archroot init: $CACHE"
  download_bootstrap
  extract_bootstrap
  configure_root
  init_keyring
  sync_and_install

  log "verifying"
  IN_INIT=1
  if ! cmd_verify; then
    rm -f "$READY"
    die "verification failed — the root exists but is not usable"
  fi

  {
    echo "created:      $(date -Is)"
    echo "bootstrap:    $(cat "$STATE/bootstrap-version" 2>/dev/null || echo unknown)"
    echo "pacman:       $(_ns_run root pacman -Q pacman 2>/dev/null | head -1 || echo unknown)"
    echo "packages:     $(_ns_run root pacman -Q 2>/dev/null | wc -l)"
    echo "size:         $(du -sh "$ROOT" 2>/dev/null | cut -f1)"
    echo "builder uid:  $BUILDER_UID"
    echo "enter:        $ENTER"
  } > "$READY.tmp"
  mv -f "$READY.tmp" "$READY"
  log "ready: $READY"
}

# ---------------------------------------------------------------------------
# verify
# ---------------------------------------------------------------------------
cmd_verify() {
  need_root
  log "checking toolchain and Tauri prerequisites"
  _ns_script root <<'EOS'
fail=0
check() {
  local what=$1; shift
  local out
  if out=$("$@" 2>&1); then
    printf '  ok    %-34s %s\n' "$what" "$(printf '%s' "$out" | head -1)"
  else
    printf '  FAIL  %-34s %s\n' "$what" "$(printf '%s' "$out" | head -1)"
    fail=1
  fi
}
check "pkg-config webkit2gtk-4.1"    pkg-config --modversion webkit2gtk-4.1
check "pkg-config gtk+-3.0"          pkg-config --modversion gtk+-3.0
check "pkg-config ayatana-appindicator3-0.1" pkg-config --modversion ayatana-appindicator3-0.1
check "pkg-config librsvg-2.0"       pkg-config --modversion librsvg-2.0
check "pkg-config libpipewire-0.3"   pkg-config --modversion libpipewire-0.3
check "pkg-config alsa"              pkg-config --modversion alsa
check "cargo"                        cargo --version
check "rustc"                        rustc --version
check "node"                         node --version
check "npm"                          npm --version
check "pnpm"                         pnpm --version
check "makepkg"                      makepkg --version
check "fakeroot"                     fakeroot --version
check "gcc"                          gcc --version
check "clang"                        clang --version
check "mold"                         mold --version
check "sccache"                      sccache --version
exit $fail
EOS
}

# ---------------------------------------------------------------------------
# run
# ---------------------------------------------------------------------------
cmd_run() {
  need_root
  [ "${1:-}" = -- ] && shift
  [ $# -gt 0 ] || die "usage: $SCRIPT run <cmd...>"
  # join the arguments and let bash inside the root parse them, so
  # `run "pacman -Q | head"` and `run pacman -Q` both do what is expected.
  # The shell starts in $HOME (/build), the writable scratch dir of the root.
  _ns_run root /bin/bash -c "cd /build && $*"
}

# ---------------------------------------------------------------------------
# makepkg
# ---------------------------------------------------------------------------
cmd_makepkg() {
  ensure_dirs
  need_root
  [ $# -eq 1 ] || die "usage: $SCRIPT makepkg <dir-with-PKGBUILD>"
  local dir
  dir=$(readlink -f -- "$1") || die "no such directory: $1"
  [ -f "$dir/PKGBUILD" ] || die "no PKGBUILD in $dir"

  local base work rel
  base=$(basename "$dir")
  rel=/build/makepkg/$base
  work=$ROOT$rel

  log "copying $dir -> $rel inside the root"
  _rm_rf "$work"
  mkdir -p "$(dirname "$work")"
  cp -a "$dir" "$work"
  # drop results of an earlier run so we only copy back fresh packages
  rm -f "$work"/*.pkg.tar.* "$work"/*.src.tar.*

  # resolve dependencies ourselves: makepkg --syncdeps would want sudo
  local deps="" psi=$LOGS/printsrcinfo.log
  if ! _ns_run builder /bin/bash -c \
        "cd $(printf '%q' "$rel") && exec makepkg --printsrcinfo" > "$psi" 2>&1; then
    warn "makepkg --printsrcinfo failed (see $psi)"
  fi
  deps=$(sed -n 's/^[[:space:]]*\(depends\|makedepends\|checkdepends\)[[:space:]]*=[[:space:]]*//p' "$psi" |
    sed -e 's/[<>=].*$//' -e 's/[[:space:]]*$//' -e '/^$/d' |
    sort -u | tr '\n' ' ')

  local nodeps=() depfail=""
  if [ -n "${deps// /}" ]; then
    info "dependencies: $deps"
    # shellcheck disable=SC2086 # word splitting is intended here
    if ! _ns_run root pacman -S --needed --asdeps --noconfirm --disable-download-timeout $deps \
          > "$LOGS/deps.log" 2>&1; then
      depfail=$(grep -oE "target not found: [^ ]+" "$LOGS/deps.log" | sed 's/.*: //' | tr '\n' ' ' || true)
      [ -n "$depfail" ] || depfail=$(tail -3 "$LOGS/deps.log" | tr '\n' ' ')
      warn "could not install dependencies ($depfail) — building with --nodeps"
      nodeps=(--nodeps)
    fi
  fi

  log "makepkg -f --noconfirm ${nodeps[*]:-} in $rel"
  local rc=0
  # build output goes to stderr, so stdout carries only the package paths
  _ns_run builder /bin/bash -c \
    "cd $(printf '%q' "$rel") && exec makepkg -f --noconfirm ${nodeps[*]:-}" >&2 || rc=$?

  local out=() f
  for f in "$work"/*.pkg.tar.*; do
    [ -e "$f" ] || continue
    cp -f "$f" "$dir/"
    out+=("$dir/$(basename "$f")")
  done

  if [ $rc -ne 0 ]; then
    [ ${#out[@]} -gt 0 ] && warn "makepkg failed but produced packages"
    return $rc
  fi
  [ ${#out[@]} -gt 0 ] || die "makepkg succeeded but produced no package"
  printf '%s\n' "${out[@]}"
}

# ---------------------------------------------------------------------------
# smoke
# ---------------------------------------------------------------------------
cmd_smoke() {
  need_root
  [ $# -eq 1 ] || [ $# -eq 2 ] || die "usage: $SCRIPT smoke <package.tar.zst> [<upgrade.tar.zst>]"
  local pkg pkg2= scratch
  pkg=$(readlink -f -- "$1") || die "no such file: $1"
  [ -f "$pkg" ] || die "no such file: $pkg"
  case $pkg in *.pkg.tar.*) ;; *) warn "$pkg does not look like a pacman package" ;; esac
  if [ $# -eq 2 ]; then
    pkg2=$(readlink -f -- "$2") || die "no such file: $2"
    [ -f "$pkg2" ] || die "no such file: $pkg2"
  fi

  ensure_dirs
  scratch=$(mktemp -d "$CACHE/tmp/smoke.XXXXXX")
  mkdir -p "$scratch/upper" "$scratch/work" "$scratch/merged"
  cp -f "$pkg" "$scratch/pkg.tar.zst"
  [ -z "$pkg2" ] || cp -f "$pkg2" "$scratch/pkg.tar.zst.2"

  log "installing $(basename "$pkg") into a throwaway overlay of $ROOT"
  local rc=0
  unshare -r -m -p -f /bin/bash -c '
    set -e
    root=$1 upper=$2 work=$3 merged=$4 pkg=$5
    mount --bind "$root" "$root"
    mount -t overlay overlay -o "lowerdir=$root,upperdir=$upper,workdir=$work,userxattr" "$merged"
    mount --rbind /dev "$merged/dev"
    mount --rbind /sys "$merged/sys"
    mount -t proc proc "$merged/proc"
    rm -f "$merged/etc/resolv.conf"; cp /etc/resolv.conf "$merged/etc/resolv.conf"
    cp "$pkg" "$merged/tmp/archroot-smoke.pkg.tar.zst"
    [ ! -f "$pkg.2" ] || cp "$pkg.2" "$merged/tmp/archroot-smoke-upgrade.pkg.tar.zst"
    exec chroot "$merged" /usr/bin/env -i HOME=/build TERM=dumb \
      PATH=/usr/local/sbin:/usr/local/bin:/usr/bin LANG=C.UTF-8 LC_ALL=C.UTF-8 \
      /bin/bash -s
  ' _ "$ROOT" "$scratch/upper" "$scratch/work" "$scratch/merged" "$scratch/pkg.tar.zst" <<'EOS' || rc=$?
set -u
pkg=/tmp/archroot-smoke.pkg.tar.zst
name=$(pacman -Qp "$pkg" 2>/dev/null | awk 'NR==1{print $1}')
pkgver=$(pacman -Qp "$pkg" 2>/dev/null | awk 'NR==1{print $2}')
[ -n "$name" ] || { echo "error: cannot read package metadata from $pkg"; exit 1; }

if ! pacman -U --noconfirm "$pkg"; then
  echo "SMOKE-FAIL: pacman -U failed"
  exit 1
fi

echo
echo "--- pacman -Qi $name"
pacman -Qi "$name" || exit 1

echo
echo "--- pacman -Qlq $name"
pacman -Qlq "$name" || exit 1

echo
echo "--- ldd on every ELF object"
bad=0
while IFS= read -r f; do
  [ -e "$f" ] || continue
  [ "$(head -c4 -- "$f" 2>/dev/null | od -An -tx1 | tr -d " \n")" = "7f454c46" ] || continue
  if ! out=$(ldd "$f" 2>&1); then
    case $out in
      *"not a dynamic executable"*|*"statically linked"*) continue ;;
    esac
    printf "  BAD  %s\n%s\n" "$f" "$out"; bad=1; continue
  fi
  if printf "%s" "$out" | grep -q "not found"; then
    printf "  BAD  %s\n%s\n" "$f" "$out"; bad=1
  else
    printf "  ok   %s\n" "$f"
  fi
done < <(pacman -Qlq "$name" | grep -v '/$')

echo
echo "--- pacman -Qkk $name"
pacman -Qkk "$name" || bad=1

if [ $bad -ne 0 ]; then
  echo "SMOKE-FAIL: missing shared libraries or file integrity errors"
  exit 1
fi
echo "SMOKE-OK: $name $pkgver installed and all ELF objects resolve"

pkg2=/tmp/archroot-smoke-upgrade.pkg.tar.zst
if [ -f "$pkg2" ]; then
  echo
  echo "--- pacman -U $(pacman -Qp "$pkg2" 2>/dev/null | awk 'NR==1{print $1 " " $2}') over $pkgver"
  pacman -U --noconfirm "$pkg2" 2>&1 | tee /tmp/archroot-smoke-upgrade.log
  if [ "${PIPESTATUS[0]}" -ne 0 ]; then
    echo "SMOKE-FAIL: upgrade failed"
    exit 1
  fi
  if grep -qi "error: command failed to execute\|scriptlet" /tmp/archroot-smoke-upgrade.log; then
    echo "SMOKE-FAIL: the upgrade scriptlet reported an error"
    exit 1
  fi
  echo "SMOKE-OK: upgraded to $(pacman -Q "$name" | awk '{print $2}')"
fi
EOS

  if [ $rc -eq 0 ]; then
    _rm_rf "$scratch"
    log "smoke test passed"
    return 0
  fi
  warn "smoke test failed — scratch root kept at $scratch/merged"
  return $rc
}

# ---------------------------------------------------------------------------
# selftest: build and smoke-install a hello-world package
# ---------------------------------------------------------------------------
cmd_selftest() {
  ensure_dirs
  need_root
  local tmp
  tmp=$(mktemp -d "$CACHE/tmp/selftest.XXXXXX")
  cat > "$tmp/PKGBUILD" <<'EOS'
pkgname=archroot-hello
pkgver=0.1
pkgrel=1
pkgdesc="archroot self-test package"
arch=('x86_64')
url="https://example.invalid/archroot"
license=('MIT')
options=('!strip')
source=('hello.c')
sha256sums=('SKIP')

build() {
  cd "$srcdir"
  ${CC:-gcc} -O2 -o hello hello.c
}

package() {
  install -Dm755 "$srcdir/hello" "$pkgdir/usr/bin/archroot-hello"
}
EOS
  cat > "$tmp/hello.c" <<'EOS'
#include <stdio.h>
int main(void) {
    puts("hello from the rootless Arch build root");
    return 0;
}
EOS

  log "selftest 1/3: makepkg a C hello-world package"
  local pkg
  pkg=$(cmd_makepkg "$tmp" | tail -1)
  [ -f "$pkg" ] || { _rm_rf "$tmp"; die "selftest: no package produced"; }

  log "selftest 2/3: smoke-install it"
  cmd_smoke "$pkg" || { _rm_rf "$tmp"; die "selftest failed"; }

  log "selftest 3/3: cargo build in the root"
  _ns_run builder /bin/bash -c '
    set -e
    rm -rf /build/selftest-rust && mkdir -p /build/selftest-rust/src
    cd /build/selftest-rust
    printf "[package]\nname = \"selftest\"\nversion = \"0.1.0\"\nedition = \"2021\"\n" > Cargo.toml
    printf "fn main() { println!(\"cargo ok\"); }\n" > src/main.rs
    cargo build --release --offline -q
    ./target/release/selftest
  ' || { _rm_rf "$tmp"; die "selftest: cargo build failed"; }

  _rm_rf "$tmp"
  log "selftest passed"
}

# ---------------------------------------------------------------------------
# dispatch
# ---------------------------------------------------------------------------
cmd_path() { echo "$CACHE"; }

usage() {
  cat <<EOS
usage: $(basename "$SCRIPT") <subcommand> [args]

  init              create/refresh the rootless Arch build root (resumable, ~1 min)
  verify            check the toolchain and the Tauri prerequisites
  run <cmd...>      run a command inside the root (cwd: /build, uid: 0)
  makepkg <dir>     build <dir>/PKGBUILD in a copy inside the root and copy the
                    resulting *.pkg.tar.zst back to <dir>
  smoke <pkg> [<pkg2>]
                    pacman -U <pkg> into a throwaway overlay of the root, then
                    print pacman -Qi/-Qlq and ldd every ELF file; with <pkg2>,
                    then upgrade to it (runs post_upgrade)
  selftest          build and smoke-install a hello-world package plus a cargo build
  path              print the cache directory

  Root:       $ROOT
  Wrapper:    $ENTER <cmd...>   (same as: $(basename "$SCRIPT") run <cmd...>)
  Docs:       the comment block at the top of $SCRIPT
EOS
}

main() {
  local cmd=${1:-}
  [ $# -gt 0 ] && shift
  case $cmd in
    _ns_exec) cmd_ns_exec "$@" ;;          # internal
    init)     cmd_init "$@" ;;
    verify)   cmd_verify "$@" ;;
    run)      cmd_run "$@" ;;
    makepkg)  cmd_makepkg "$@" ;;
    smoke)    cmd_smoke "$@" ;;
    selftest) cmd_selftest "$@" ;;
    path)     cmd_path ;;
    ""|-h|--help|help) usage ;;
    *) die "unknown subcommand '$cmd' (try: $SCRIPT --help)" ;;
  esac
}

# keep the stable wrapper for other scripts around, whatever we are asked to do
if [ "${1:-}" != _ns_exec ]; then
  ensure_enter_sh
fi

main "$@"
