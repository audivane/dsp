#!/bin/sh
# Post-install / post-upgrade / pre-remove scriptlets shared by the .deb, .rpm and Arch
# packages built by nfpm. package.sh appends one dispatch line per format:
#
#   deb     postinst:      audivane_postinst deb "$@"     ("configure" <old version>)
#   rpm     %post:         audivane_postinst rpm "$@"     ($1 = 1 install, >= 2 upgrade)
#   arch    post_install:  audivane_postinst arch-install
#   arch    post_upgrade:  audivane_postinst arch-upgrade
#
# First install: print how to start the engine (it is a per-user service; the
# package never starts it). Upgrade: for every running user manager, reload it
# and restart audivane-dsp.service where it was running (try-restart), so the
# new engine runs right away. Every step is best effort: this never fails the
# transaction, and without systemd as init (containers, build roots) it only
# prints the note. packaging/arch/audivane-dsp.install (the makepkg build) has
# the same logic; keep both in step.

audivane_restart_user_engines() {
  # Prints the users whose engine was restarted.
  [ -d /run/systemd/system ] || return 0
  command -v systemctl >/dev/null 2>&1 || return 0
  _t=
  command -v timeout >/dev/null 2>&1 && _t="timeout 30"
  for _dir in /run/user/[0-9]*; do
    [ -d "$_dir" ] || continue
    # A user manager is running for this uid.
    [ -S "$_dir/systemd/private" ] || continue
    _uid=${_dir##*/}
    _user=$(getent passwd "$_uid" 2>/dev/null | cut -d: -f1)
    [ -n "$_user" ] || continue
    $_t systemctl --no-ask-password --user -M "$_user@" daemon-reload >/dev/null 2>&1 || true
    if $_t systemctl --no-ask-password --user -M "$_user@" is-active --quiet audivane-dsp.service >/dev/null 2>&1; then
      if $_t systemctl --no-ask-password --user -M "$_user@" try-restart audivane-dsp.service >/dev/null 2>&1; then
        printf ' %s' "$_user"
      fi
    fi
  done
  return 0
}

audivane_note_install() {
  cat <<'EOF'
==> Audivane DSP installed.
Start the engine for your user session (the package never does this for you):
    systemctl --user daemon-reload && systemctl --user enable --now audivane-dsp
While it runs it takes your output DAC exclusively and registers itself as the
PipeWire sink; stop the service to hand the device back. Check it with
    avdsp ctl status
and open "Audivane DSP" from your app menu.
EOF
}

audivane_note_upgrade() {
  _users=$(audivane_restart_user_engines 2>/dev/null) || _users=
  if [ -n "$_users" ]; then
    echo "==> Audivane DSP updated: restarted the engine for:$_users"
  else
    echo "==> Audivane DSP updated. No running engine was restarted; start it with"
    echo "    systemctl --user daemon-reload && systemctl --user enable --now audivane-dsp"
  fi
  echo "    A running app restarts itself into the new version (if it cannot, it says so)."
}

audivane_postinst() {
  _mode=install
  case ${1:-} in
    deb)
      # postinst configure <most recently configured version>; other actions
      # (abort-upgrade, ...) roll back a failed transaction: stay quiet.
      [ "${2:-}" = configure ] || return 0
      [ -n "${3:-}" ] && _mode=upgrade
      ;;
    rpm) [ "${2:-1}" -ge 2 ] 2>/dev/null && _mode=upgrade ;;
    arch-upgrade) _mode=upgrade ;;
    *) ;;
  esac
  if [ "$_mode" = upgrade ]; then
    audivane_note_upgrade
  else
    audivane_note_install
  fi
  return 0
}

# Removal (not an upgrade) leaves nothing behind, for every user:
# - the engine is stopped and disabled in every running user manager, and
#   the app and engine processes are closed (the DAC is handed back);
# - each user's Audivane DSP files are deleted: settings, presets and
#   overlays (~/.config/audivane-dsp), the app's window and view state, its
#   cache, the engine's state, the start-at-login entry and the service's
#   enable links;
# - KDE forgets the app's global shortcuts. Other programs' settings are
#   not touched.
# Best effort: it never fails the transaction.
audivane_remove_everything() {
  _t=
  command -v timeout >/dev/null 2>&1 && _t="timeout 30"
  if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then
    for _dir in /run/user/[0-9]*; do
      [ -S "$_dir/systemd/private" ] || continue
      _user=$(getent passwd "${_dir##*/}" 2>/dev/null | cut -d: -f1)
      [ -n "$_user" ] || continue
      $_t systemctl --no-ask-password --user -M "$_user@" disable --now audivane-dsp.service >/dev/null 2>&1 || true
    done
  fi
  pkill -x audivane-dsp-ui >/dev/null 2>&1 || true
  pkill -x audivane-dsp >/dev/null 2>&1 || true
  getent passwd | while IFS=: read -r _user _x _uid _gid _gecos _home _shell; do
    [ "$_uid" -ge 1000 ] 2>/dev/null || continue
    [ -d "$_home" ] || continue
    rm -rf "$_home/.config/audivane-dsp" "$_home/.config/com.audivane.dsp" \
      "$_home/.local/share/com.audivane.dsp" "$_home/.cache/audivane-dsp" \
      "$_home/.local/state/audivane-dsp" "/run/user/$_uid/audivane-dsp" \
      "$_home/.config/autostart/audivane-dsp-ui.desktop"
    for _p in "$_home"/.config/audivane-dsp.backup-* "$_home"/.config/systemd/user/*.wants/audivane-dsp.service; do
      [ -e "$_p" ] || [ -L "$_p" ] || continue
      rm -rf "$_p"
    done
    # KDE: drop the app's (now inactive) global shortcuts.
    if [ -S "/run/user/$_uid/bus" ] && command -v gdbus >/dev/null 2>&1 && command -v runuser >/dev/null 2>&1; then
      DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$_uid/bus" $_t runuser -u "$_user" -- \
        gdbus call --session --dest org.kde.kglobalaccel --object-path /component/audivane_dsp_ui \
        --method org.kde.kglobalaccel.Component.cleanUp >/dev/null 2>&1 || true
    fi
  done
  echo "==> Audivane DSP removed: the engine and the app are stopped, and their settings deleted."
  return 0
}

# deb prerm <remove|upgrade|...>, rpm %preun ($1 = 0 erase, >= 1 upgrade),
# arch pre_remove (removal only).
audivane_prerm() {
  case ${1:-} in
    deb) [ "${2:-}" = remove ] || return 0 ;;
    rpm) [ "${2:-1}" = 0 ] || return 0 ;;
    arch-remove) ;;
    *) return 0 ;;
  esac
  audivane_remove_everything
}
