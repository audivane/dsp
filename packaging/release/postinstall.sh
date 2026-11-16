#!/bin/sh
# Post-install / post-upgrade scriptlet shared by the .deb, .rpm and Arch
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
  echo "    If the app is open, click Restart in its update banner (or reopen it)."
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
