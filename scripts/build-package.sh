#!/usr/bin/env bash
set -euo pipefail
ROOT=$(realpath "$(dirname "${BASH_SOURCE[0]}")/..")
# Rust package builds can exceed a memory-backed /tmp quota.
# Honor an explicit TMPDIR; otherwise use the user's disk-backed cache.
WORK_BASE=${TMPDIR:-${XDG_CACHE_HOME:-$HOME/.cache}}
mkdir -p -- "$WORK_BASE"
WORK=$(mktemp -d "$WORK_BASE/omarchy-quickshare-package.XXXXXXXX")
trap 'rm -rf -- "$WORK"' EXIT
VERSION=0.3.7
OUTPUT_DIR=${QUICKSHARE_PACKAGE_OUTPUT:-$ROOT/dist}
command -v makepkg >/dev/null || { printf 'Falta makepkg.\n' >&2; exit 1; }
[[ -f "$ROOT/Cargo.lock" ]] || { printf 'Falta Cargo.lock; genera y verifica primero la compilación.\n' >&2; exit 1; }

# Stable order, normalized timestamps/owners and timestamp-free gzip make this
# source snapshot reproducible. No build artifacts enter the archive.
LC_ALL=C tar --sort=name --mtime="@${SOURCE_DATE_EPOCH:-0}" --owner=0 --group=0 --numeric-owner \
  --format=gnu -cf - \
  --exclude='target' --exclude='bindings' --exclude='.git' --exclude='__pycache__' --exclude='*.pyc' \
  --transform="s,^,omarchy-quickshare-$VERSION/," \
  -C "$ROOT" Cargo.toml Cargo.lock src vendor manifest.json Service.qml BarWidget.qml Panel.qml TransferEffects.qml TransferEffectsArt.qml nautilus packaging scripts LICENSE README.md preview.png CONTRIBUTING.md CODE_OF_CONDUCT.md SECURITY.md docs |
  gzip -n > "$WORK/omarchy-quickshare-$VERSION.tar.gz"
SOURCE_CHECKSUM=$(sha256sum "$WORK/omarchy-quickshare-$VERSION.tar.gz")
SOURCE_CHECKSUM=${SOURCE_CHECKSUM%% *}
# Ship a standalone PKGBUILD with the digest of this exact source snapshot,
# rather than an environment-dependent checksum or a verification bypass.
while IFS= read -r line || [[ -n "$line" ]]; do
  if [[ "$line" == 'sha256sums=('* ]]; then
    printf 'sha256sums=(%q)\n' "$SOURCE_CHECKSUM"
  else
    printf '%s\n' "$line"
  fi
done < "$ROOT/packaging/PKGBUILD" > "$WORK/PKGBUILD"
(
  cd "$WORK"
  makepkg --cleanbuild "$@"
)
SOURCE_DIR="$OUTPUT_DIR/source/omarchy-quickshare-$VERSION"
mkdir -p "$SOURCE_DIR"
install -m644 "$WORK/PKGBUILD" "$WORK/omarchy-quickshare-$VERSION.tar.gz" "$SOURCE_DIR/"
for package in "$WORK"/*.pkg.tar.*; do
  [[ -f "$package" ]] || continue
  install -Dm644 "$package" "$OUTPUT_DIR/$(basename "$package")"
done
printf 'Packages built in %s; corresponding source and checksummed PKGBUILD in %s\n' "$OUTPUT_DIR" "$SOURCE_DIR"
