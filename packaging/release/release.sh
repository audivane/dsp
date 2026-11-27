#!/usr/bin/env bash
# shellcheck shell=bash
#
# Local release build: the .deb, .rpm and Arch packages, the AppImage and
# SHA256SUMS in
# dist/release/<version>/, built from the committed tree on the Ubuntu 24.04
# baseline (see baseline.sh), then smoke-tested.
#
# Usage: packaging/release/release.sh [--worktree] [--clean] [--skip-smoke]
#   --worktree    build the working tree, uncommitted changes included
#   --clean       drop the cached cargo target dir first (a cold build)
#   --skip-smoke  skip the install tests (Arch root, Ubuntu root, AppImage)
set -euo pipefail

repo=$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)
here=$repo/packaging/release
worktree=0 clean=0 smoke=1
for arg in "$@"; do
  case $arg in
    --worktree) worktree=1 ;;
    --clean) clean=1 ;;
    --skip-smoke) smoke=0 ;;
    *) sed -n '3,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 2 ;;
  esac
done

version=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version *= *"\([^"]*\)".*/\1/p' "$repo/Cargo.toml")
[ -n "$version" ] || { echo "error: no [workspace.package] version" >&2; exit 1; }
work=$repo/target/release-build
out=$repo/dist/release/$version
cache=${AUDIVANE_UBUNTU_ROOT:-$HOME/.cache/audivane-ubuntu-root}

t_all=$(date +%s)
summary=()
step() { _t=$(date +%s); printf '\n######## %s\n' "$1"; _s=$1; }
done_step() { summary+=("$(printf '%5ss  %s' "$(( $(date +%s) - _t ))" "$_s")"); }

step "license records (fonts, third-party notices)"
node "$repo/tools/check-subset-fonts.mjs"
node "$repo/tools/third-party-notices.mjs" --check
done_step

step "export source ($([ $worktree = 1 ] && echo 'working tree' || echo "HEAD $(git -C "$repo" rev-parse --short HEAD)"))"
if [ $worktree = 0 ] && [ -n "$(git -C "$repo" status --porcelain --untracked-files=no)" ]; then
  echo "warning: uncommitted changes are NOT included (use --worktree to include them)" >&2
fi
rm -rf "$work/src"; mkdir -p "$work/src"
if [ $worktree = 1 ]; then
  git -C "$repo" ls-files -co --exclude-standard -z | (cd "$repo" && tar --null -T - -cf -) | tar -xf - -C "$work/src"
else
  git -C "$repo" archive HEAD | tar -xf - -C "$work/src"
fi
done_step

step "baseline root (Ubuntu 24.04)"
"$here/baseline.sh" setup
[ $clean = 1 ] && rm -rf "$cache/cache/target"
done_step

step "compile on the baseline"
"$here/baseline.sh" build "$work/src" "$work/stage"
done_step

step "package .deb .rpm .pkg.tar.zst"
"$here/package.sh" "$work/stage" "$out" "$version"
done_step

step "AppImage"
"$here/appimage.sh" "$work/stage" "$out" "$version"
done_step

if [ $smoke = 1 ]; then
  # Installed twice: the second install runs the upgrade scriptlet (on a
  # reinstall too), which must not fail without systemd.
  step "smoke: Arch package in the Arch root (install, then upgrade)"
  pkg=$(ls "$out"/*.pkg.tar.zst)
  "$repo/packaging/arch/archroot.sh" smoke "$pkg" "$pkg" 2>&1 | grep -E '^(==>|SMOKE|    )' | tail -8
  done_step
  step "smoke: .deb in a copy of the Ubuntu 24.04 root (install, then upgrade)"
  deb=$(ls "$out"/*.deb)
  "$here/baseline.sh" deb-smoke "$deb" "$deb"
  done_step
  # Its binaries are the .deb's (same libraries): here only that it runs,
  # without FUSE, and reports its version.
  step "smoke: AppImage (runs, bundled CLI, setup status)"
  app=$(ls "$out"/*.AppImage)
  APPIMAGE_EXTRACT_AND_RUN=1 "$app" avdsp --version
  APPIMAGE_EXTRACT_AND_RUN=1 "$app" --status | sed -n '1p'
  done_step
fi

printf '\n######## done in %ss\n' "$(( $(date +%s) - t_all ))"
printf '%s\n' "${summary[@]}"
echo "compile stages (inside the root):"
sed 's/^/  /' "$work/stage/timings.txt"
echo "artifacts: $out"
