#!/usr/bin/env bash
# Build the local source tarball the Arch PKGBUILD consumes.
#
#   ./make-source.sh            pack the COMMITTED git tree (HEAD)
#   ./make-source.sh --worktree pack the WORKING TREE (uncommitted changes
#                               included; build caches are skipped)
#
# Output: packaging/arch/audivane-dsp-<version>.tar.zst, where <version> is
# [workspace.package] version from Cargo.toml. The archive has one top-level
# directory, audivane-dsp-<version>/, holding the repository root (cargo
# workspace, LICENSE, assets/brand/): exactly what the PKGBUILD expects.
#
# Then build the package:  cd packaging/arch && makepkg -si

set -euo pipefail

mode=${1:-}
if [[ -n "$mode" && "$mode" != "--worktree" ]]; then
  echo "usage: ${0##*/} [--worktree]" >&2
  exit 2
fi

here=${BASH_SOURCE[0]}
here=$(cd -- "$(dirname -- "$here")" && pwd)
repo_root=$(git -C "$here" rev-parse --show-toplevel)
arch_dir="$repo_root/packaging/arch"

# Version straight from the workspace manifest.
version=$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$repo_root/Cargo.toml" \
  | sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p')
if [[ -z "$version" ]]; then
  echo "error: no [workspace.package] version in Cargo.toml" >&2
  exit 1
fi

command -v zstd >/dev/null || { echo "error: zstd is required" >&2; exit 1; }

pkgbuild_ver=$(sed -n 's/^pkgver=//p' "$arch_dir/PKGBUILD")
if [[ "$pkgbuild_ver" != "$version" ]]; then
  echo "error: PKGBUILD pkgver is $pkgbuild_ver but Cargo.toml says $version; update packaging/arch/PKGBUILD" >&2
  exit 1
fi
out="$arch_dir/audivane-dsp-$version.tar.zst"
prefix="audivane-dsp-$version/"

if [[ "$mode" == "--worktree" ]]; then
  echo "==> packing the working tree (uncommitted changes included)"
  # tar's --exclude matches the original (pre --transform) paths, so the
  # patterns below are repo-root relative.
  tar --exclude='./target' \
      --exclude='./app/src-tauri/target' \
      --exclude='./app/dist' \
      --exclude='*/node_modules' \
      --exclude='./packaging/arch/src' \
      --exclude='./packaging/arch/pkg' \
      --exclude='./dist' \
      --exclude='./.git' \
      --exclude='*.tar.zst' \
      --transform "s,^\./,$prefix," \
      -cf - -C "$repo_root" . \
    | zstd -T0 -q -f -o "$out"
else
  if [[ -n $(git -C "$repo_root" status --porcelain) ]]; then
    echo "note: the working tree has uncommitted changes; they are NOT in" \
         "this tarball (use --worktree to include them)." >&2
  fi
  echo "==> packing the committed tree (HEAD)"
  git -C "$repo_root" archive --format=tar --prefix="$prefix" HEAD \
    | zstd -T0 -q -f -o "$out"
fi

echo "==> wrote $out"
echo "==> top-level layout:"
zstd -dc "$out" | tar -tf - | awk -F/ 'NF && $0 !~ /\/$/ {print $1"/"$2}' | sort -u | head -20
