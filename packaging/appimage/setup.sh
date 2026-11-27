#!/bin/sh
# shellcheck shell=sh
#
# Audivane DSP from an AppImage: install it for this user, update that
# install, remove it. No root: everything goes under the user's home.
#
#   setup.sh install [--no-login-start]   (from the AppImage: needs $APPDIR)
#       Copies the engine, the CLI and the app to ~/.local/lib/audivane-dsp/
#       (links in ~/.local/bin), writes the systemd user service, a menu
#       entry and the icons, then starts the engine (and enables it at login
#       unless --no-login-start). Over an older or newer install it is the
#       update: files are replaced (new inodes), the service restarts if it
#       was running, and a running app relaunches into its new binary.
#   setup.sh uninstall [--purge]
#       Stops and disables the service and removes exactly the files and
#       folders `install` added (listed in ~/.local/lib/audivane-dsp/MANIFEST).
#       --purge also deletes the settings, presets and state the engine and
#       the app wrote (what removing a system package deletes).
#   setup.sh status
#
# Copies, not the AppImage itself: the engine runs as a sandboxed service
# (NoNewPrivileges), which cannot FUSE-mount an AppImage, and a copy keeps
# working when the AppImage file is moved, renamed or deleted. A system
# package (/usr/bin/audivane-dsp) wins: with one installed, `install`
# refuses, and the app uses the package's engine on the same socket.
set -eu

LIB=$HOME/.local/lib/audivane-dsp
BIN=$HOME/.local/bin
CFG=${XDG_CONFIG_HOME:-$HOME/.config}
DATA=${XDG_DATA_HOME:-$HOME/.local/share}
UNIT_DIR=$CFG/systemd/user
UNIT=$UNIT_DIR/audivane-dsp.service
WANTS=$UNIT_DIR/default.target.wants
DESKTOP=$DATA/applications/audivane-dsp-ui.desktop
AUTOSTART=$CFG/autostart/audivane-dsp-ui.desktop
ICONS=$DATA/icons/hicolor
MANIFEST=$LIB/MANIFEST
SYSTEM_ENGINE=${AUDIVANE_SYSTEM_ENGINE:-/usr/bin/audivane-dsp}
BINARIES="audivane-dsp avdsp audivane-dsp-ui"

say() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit "${2:-1}"; }

have_user_manager() {
  command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1
}
sctl() { systemctl --user --no-ask-password "$@"; }

# mkdir -p that records each folder it creates (in $NEW), for uninstall.
mkd() {
  [ -d "$1" ] && return 0
  mkd "$(dirname "$1")"
  mkdir "$1"
  printf 'd %s\n' "$1" >> "$NEW"
}

# put <src> <dst> <mode>: write a new file and rename it over the old one,
# so a running engine or app keeps its old inode and sees a replacement.
put() {
  mkd "$(dirname "$2")"
  cp "$1" "$2.new.$$"
  chmod "$3" "$2.new.$$"
  mv -f "$2.new.$$" "$2"
  printf 'f %s\n' "$2" >> "$NEW"
}

# text <dst> <mode>: like put, from stdin.
text() {
  mkd "$(dirname "$1")"
  cat > "$1.new.$$"
  chmod "$2" "$1.new.$$"
  mv -f "$1.new.$$" "$1"
  printf 'f %s\n' "$1" >> "$NEW"
}

# link <target> <name>: a symlink, unless something else already has the name.
link() {
  if [ -L "$2" ] && [ "$(readlink "$2")" = "$1" ]; then
    printf 'f %s\n' "$2" >> "$NEW"
  elif [ -e "$2" ] || [ -L "$2" ]; then
    warn "$2 already exists and is not this install's; left as it is"
  else
    mkd "$(dirname "$2")"
    ln -s "$1" "$2"
    printf 'f %s\n' "$2" >> "$NEW"
  fi
}

installed_version() { cat "$LIB/VERSION" 2>/dev/null || true; }

# Our unit file is the one in the manifest; anything else there is the user's.
unit_is_ours() { [ -f "$MANIFEST" ] && grep -qxF "f $UNIT" "$MANIFEST"; }

# One Exec= argument for a desktop entry (spec: quote, escape " ` $ \, then
# double every backslash for the string value; % becomes %%).
exec_arg() {
  printf '%s' "$1" | sed 's/%/%%/g; s/[\\"`$]/\\&/g; s/\\/\\\\/g; s/^/"/; s/$/"/'
}

# setline <prefix> <value>: stdin to stdout, every line starting with
# <prefix> becomes <prefix><value> (no escape processing, unlike sed).
setline() {
  P=$1 V=$2 awk 'index($0, ENVIRON["P"]) == 1 { print ENVIRON["P"] ENVIRON["V"]; next } { print }'
}

# A path inside a systemd unit: specifiers (%) and variables ($) escaped,
# quoted for spaces.
unit_path() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/%/%%/g; s/\$/$$/g; s/"/\\"/g; s/^/"/; s/$/"/'
}

cmd_install() {
  login_start=1
  for a in "$@"; do
    case $a in
      --no-login-start) login_start=0 ;;
      *) die "install: unknown option $a" 2 ;;
    esac
  done
  if [ -z "${APPDIR:-}" ] || [ ! -x "$APPDIR/usr/bin/audivane-dsp" ]; then
    die "install runs from the AppImage: <file>.AppImage --install"
  fi
  case $HOME in
    *"
"*) die "a home path with a newline is not supported" ;;
  esac
  if [ -e "$SYSTEM_ENGINE" ]; then
    die "a system package of Audivane DSP is installed ($SYSTEM_ENGINE). The app works with its engine; remove the package first to use a user install instead." 3
  fi
  if [ -e "$UNIT" ] && ! unit_is_ours; then
    die "$UNIT exists and was not written by this setup; move it away first." 4
  fi

  new_version=$(cat "$APPDIR/usr/lib/audivane-dsp/VERSION")
  old_version=$(installed_version)
  was_active=0
  if have_user_manager && sctl is-active --quiet audivane-dsp.service 2>/dev/null; then
    was_active=1
  fi

  NEW=$(mktemp "${TMPDIR:-/tmp}/audivane-manifest.XXXXXX")
  trap 'rm -f "$NEW"' EXIT
  # Folders an earlier install created stay recorded.
  if [ -f "$MANIFEST" ]; then grep '^d ' "$MANIFEST" >> "$NEW" || true; fi

  for b in $BINARIES; do
    put "$APPDIR/usr/bin/$b" "$LIB/$b" 755
  done
  put "$APPDIR/usr/lib/audivane-dsp/setup.sh" "$LIB/setup.sh" 755
  put "$APPDIR/usr/lib/audivane-dsp/VERSION" "$LIB/VERSION" 644
  for f in "$APPDIR"/usr/share/licenses/audivane-dsp/*; do
    put "$f" "$LIB/licenses/${f##*/}" 644
  done
  for b in $BINARIES; do
    link "$LIB/$b" "$BIN/$b"
  done

  exe=$(unit_path "$LIB/audivane-dsp")
  setline ExecStart= "$exe --foreground" < "$APPDIR/usr/lib/systemd/user/audivane-dsp.service" \
    | setline ExecStopPost= "-$exe --volume-failsafe" | text "$UNIT" 644
  [ -d "$WANTS" ] || printf 'd %s\n' "$WANTS" >> "$NEW"

  ui=$(exec_arg "$LIB/audivane-dsp-ui")
  setline Exec= "$ui" < "$APPDIR/usr/share/applications/audivane-dsp-ui.desktop" | text "$DESKTOP" 644
  for f in "$APPDIR"/usr/share/icons/hicolor/*/apps/audivane-dsp.*; do
    rel=${f#"$APPDIR/usr/share/icons/hicolor/"}
    put "$f" "$ICONS/$rel" 644
  done
  # The app's own start-at-login entry, made while it ran from the AppImage:
  # point it at the installed app (the AppImage may move). Not ours to list.
  if [ -f "$AUTOSTART" ]; then
    if setline Exec= "$ui --hidden" < "$AUTOSTART" > "$AUTOSTART.new.$$"; then
      mv -f "$AUTOSTART.new.$$" "$AUTOSTART"
    else
      rm -f "$AUTOSTART.new.$$"; warn "cannot update $AUTOSTART"
    fi
  fi

  printf 'f %s\n' "$MANIFEST" >> "$NEW"
  # Files an older version installed that this one does not have.
  if [ -f "$MANIFEST" ]; then
    grep '^f ' "$MANIFEST" | while IFS= read -r line; do
      if ! grep -qxF "$line" "$NEW"; then rm -f "${line#f }"; fi
    done
  fi
  sort -u "$NEW" > "$MANIFEST.new.$$" && mv -f "$MANIFEST.new.$$" "$MANIFEST"

  if [ -z "$old_version" ]; then
    say "==> Audivane DSP $new_version installed for $(id -un) in $LIB"
  elif [ "$old_version" = "$new_version" ]; then
    say "==> Audivane DSP $new_version reinstalled in $LIB"
  else
    say "==> Audivane DSP updated: $old_version -> $new_version"
  fi

  if ! have_user_manager; then
    say "No systemd user manager answers here, so the engine was not started. Start it with"
    say "    systemctl --user daemon-reload && systemctl --user enable --now audivane-dsp"
    return 0
  fi
  sctl daemon-reload || warn "systemctl --user daemon-reload failed"
  if [ -z "$old_version" ]; then
    if [ "$login_start" = 1 ]; then
      sctl enable audivane-dsp.service >/dev/null 2>&1 || warn "cannot enable the engine at login"
    fi
    sctl reset-failed audivane-dsp.service >/dev/null 2>&1 || true
    if sctl start audivane-dsp.service; then
      say "    The engine is running$([ "$login_start" = 1 ] && echo ' and starts at login')."
    else
      warn "the engine did not start; see: journalctl --user -u audivane-dsp"
    fi
  elif [ "$was_active" = 1 ]; then
    if sctl restart audivane-dsp.service; then
      say "    Restarted the engine. A running app restarts itself into the new version."
    else
      warn "the engine did not restart; see: journalctl --user -u audivane-dsp"
    fi
  else
    say "    The engine was not running; start it from the app or with: systemctl --user start audivane-dsp"
  fi
  say "    Remove it with: $LIB/setup.sh uninstall   (--purge also deletes the settings)"
}

# Stop app processes started from this install (the binary may already be
# replaced or deleted: /proc/<pid>/exe then ends in " (deleted)").
stop_installed_app() {
  for p in /proc/[0-9]*; do
    exe=$(readlink "$p/exe" 2>/dev/null) || continue
    case $exe in
      "$LIB/audivane-dsp-ui" | "$LIB/audivane-dsp-ui (deleted)") kill "${p#/proc/}" 2>/dev/null || true ;;
    esac
  done
}

cmd_uninstall() {
  purge=0
  for a in "$@"; do
    case $a in
      --purge) purge=1 ;;
      *) die "uninstall: unknown option $a" 2 ;;
    esac
  done
  installed=0
  [ -f "$MANIFEST" ] && installed=1
  # The engine and the app stop first: neither writes its settings again.
  if [ "$installed" = 1 ]; then
    if unit_is_ours && have_user_manager; then
      sctl disable --now audivane-dsp.service >/dev/null 2>&1 || true
      sctl reset-failed audivane-dsp.service >/dev/null 2>&1 || true
    fi
    stop_installed_app
  fi
  # Then the settings, so the folders they were in can go with the install's.
  if [ "$purge" = 1 ]; then
    rm -rf "$CFG/audivane-dsp" "$CFG/com.audivane.dsp" "$DATA/com.audivane.dsp" \
      "${XDG_CACHE_HOME:-$HOME/.cache}/audivane-dsp" "${XDG_STATE_HOME:-$HOME/.local/state}/audivane-dsp"
    if [ -n "${XDG_RUNTIME_DIR:-}" ]; then rm -rf "$XDG_RUNTIME_DIR/audivane-dsp"; fi
    for p in "$CFG"/audivane-dsp.backup-*; do
      if [ -e "$p" ]; then rm -rf "$p"; fi
    done
    # A system package's app may still want its start-at-login entry.
    if [ ! -e "$SYSTEM_ENGINE" ]; then rm -f "$AUTOSTART"; fi
  fi
  if [ "$installed" = 1 ]; then
    # Its own start-at-login entry would start a binary that is gone.
    if [ -f "$AUTOSTART" ] && grep -qF "$LIB/" "$AUTOSTART"; then
      rm -f "$AUTOSTART"
    fi
    # Files first, then folders deepest first, each only if now empty.
    # (The manifest lists itself: read it whole first.)
    list=$(cat "$MANIFEST")
    printf '%s\n' "$list" | sed -n 's/^f //p' | while IFS= read -r f; do rm -f "$f"; done
    printf '%s\n' "$list" | sed -n 's/^d //p' | sort -r | while IFS= read -r d; do rmdir "$d" 2>/dev/null || true; done
    if have_user_manager; then sctl daemon-reload >/dev/null 2>&1 || true; fi
    say "==> Audivane DSP's user install removed."
  else
    say "No user install of Audivane DSP here ($MANIFEST is missing)."
  fi
  if [ "$purge" = 1 ]; then say "    Settings, presets and state deleted."; fi
}

cmd_status() {
  v=$(installed_version)
  if [ -n "${APPDIR:-}" ]; then say "AppImage:        $(cat "$APPDIR/usr/lib/audivane-dsp/VERSION" 2>/dev/null) (${APPIMAGE:-$APPDIR})"; fi
  say "user install:    ${v:-none}${v:+ ($LIB)}"
  if [ -e "$SYSTEM_ENGINE" ]; then
    say "system package:  yes ($SYSTEM_ENGINE)"
    if [ -n "$v" ]; then say "                 note: the user service in $UNIT_DIR takes precedence over the package's"; fi
  else
    say "system package:  no"
  fi
  if have_user_manager; then
    active=$(sctl is-active audivane-dsp.service 2>/dev/null) || true
    enabled=$(sctl is-enabled audivane-dsp.service 2>/dev/null) || true
    say "engine service:  ${active:-unknown}, ${enabled:-not installed}"
  else
    say "engine service:  no systemd user manager"
  fi
}

case ${1:-} in
  install) shift; cmd_install "$@" ;;
  uninstall) shift; cmd_uninstall "$@" ;;
  status) shift; cmd_status "$@" ;;
  *) sed -n '4,25p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
