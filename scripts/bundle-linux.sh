#!/usr/bin/env bash
# Build Xade in release mode and pack Xade-linux-<arch>.tar.gz (x86_64 or aarch64) into $1 (default: ./dist).
# The archive holds the binary, a desktop entry and the icon; scripts/install.sh unpacks it.
# With --install, nothing is kept in ./dist: the build is installed like the release one
# (~/.local/share/xade, desktop entry, icon, ~/.local/bin/xade), the Linux twin of bundle-macos.sh.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INSTALL=0
if [ "${1:-}" = "--install" ]; then
  INSTALL=1
  shift
fi
if [ "$INSTALL" = 1 ]; then
  DEST_DIR="$(mktemp -d)"
  CLEAN="$DEST_DIR"
else
  DEST_DIR="${1:-$ROOT/dist}"
  CLEAN=""
fi
ARCH="$(uname -m)"
ARCHIVE="$DEST_DIR/Xade-linux-$ARCH.tar.gz"

echo "Building xade (release)..."
cargo build --release -p xade --manifest-path "$ROOT/Cargo.toml"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK" "$CLEAN"' EXIT

echo "Assembling $ARCHIVE..."
cp "$ROOT/target/release/xade" "$WORK/xade"
cp "$ROOT/assets/xade-icon-app.png" "$WORK/xade.png"
cat > "$WORK/xade.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Xade
Comment=Desktop client for Claude Code
Exec=xade
Icon=xade
Terminal=false
Categories=Development;
DESKTOP

mkdir -p "$DEST_DIR"
tar -czf "$ARCHIVE" -C "$WORK" xade xade.desktop xade.png
echo "Wrote $ARCHIVE"

if [ "$INSTALL" = 1 ]; then
  XADE_ASSET_URL="file://$ARCHIVE" sh "$ROOT/scripts/install.sh"
fi
