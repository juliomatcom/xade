#!/usr/bin/env bash
# Build Xade in release mode and pack Xade-linux-<arch>.tar.gz (x86_64 or aarch64) into $1 (default: ./dist).
# The archive holds the binary, a desktop entry and the icon; scripts/install.sh unpacks it.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST_DIR="${1:-$ROOT/dist}"
ARCH="$(uname -m)"
ARCHIVE="$DEST_DIR/Xade-linux-$ARCH.tar.gz"

echo "Building xade (release)..."
cargo build --release -p xade --manifest-path "$ROOT/Cargo.toml"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

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
