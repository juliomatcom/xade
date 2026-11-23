#!/usr/bin/env bash
# Build Xade in release mode and install it as /Applications/Xade.app (or $1).
# Uses only macOS built-ins (sips, iconutil) plus cargo.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST_DIR="${1:-/Applications}"
APP="$DEST_DIR/Xade.app"
VERSION="$(grep -m1 '^version' "$ROOT/apps/desktop/Cargo.toml" | cut -d'"' -f2)"

echo "Building xade $VERSION (release)..."
cargo build --release -p xade --manifest-path "$ROOT/Cargo.toml"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "Making icon..."
ICONSET="$WORK/Xade.iconset"
mkdir -p "$ICONSET"
SRC="$ROOT/assets/xade-icon-app.png"
for s in 16 32 128 256 512; do
  sips -z "$s" "$s" "$SRC" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2))
  [ "$d" -le 512 ] && sips -z "$d" "$d" "$SRC" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
cp "$SRC" "$ICONSET/icon_256x256@2x.png"
cp "$SRC" "$ICONSET/icon_512x512.png"
iconutil -c icns "$ICONSET" -o "$WORK/Xade.icns"

echo "Assembling $APP..."
STAGE="$WORK/Xade.app"
mkdir -p "$STAGE/Contents/MacOS" "$STAGE/Contents/Resources"
cp "$ROOT/target/release/xade" "$STAGE/Contents/MacOS/xade"
cp "$WORK/Xade.icns" "$STAGE/Contents/Resources/Xade.icns"
cat > "$STAGE/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Xade</string>
  <key>CFBundleDisplayName</key><string>Xade</string>
  <key>CFBundleIdentifier</key><string>com.github.juliomatcom.xade</string>
  <key>CFBundleExecutable</key><string>xade</string>
  <key>CFBundleIconFile</key><string>Xade</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

rm -rf "$APP"
mv "$STAGE" "$APP"
echo "Installed $APP"
echo "Open it once from Launchpad or Spotlight, then right-click its Dock icon > Options > Keep in Dock."
