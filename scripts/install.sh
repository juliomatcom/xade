#!/bin/sh
# Install or update Xade from the latest GitHub release and link the `xade` CLI.
#   curl -fsSL https://raw.githubusercontent.com/juliomatcom/xade/main/scripts/install.sh | sh
# Test overrides: XADE_ASSET_URL (e.g. a file:// zip or tarball), XADE_APP_DIR (install dir).
set -eu

RELEASE_URL="https://github.com/juliomatcom/xade/releases/latest/download"
BIN_DIR="$HOME/.local/bin"

die() {
  printf 'xade install: %s\n' "$*" >&2
  exit 1
}

command -v curl >/dev/null 2>&1 || die "curl is required"

os="$(uname -s)"
arch="$(uname -m)"
case "$os-$arch" in
  Darwin-arm64)
    asset="Xade-macos-arm64.zip"
    command -v unzip >/dev/null 2>&1 || die "unzip is required"
    ;;
  Linux-x86_64)
    asset="Xade-linux-x86_64.tar.gz"
    command -v tar >/dev/null 2>&1 || die "tar is required"
    ;;
  *) die "unsupported platform $os/$arch (supported: macOS arm64, Linux x86_64)" ;;
esac
url="${XADE_ASSET_URL:-$RELEASE_URL/$asset}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM

echo "Downloading $url"
curl -fsSL -o "$tmp/$asset" "$url" || die "download failed: $url"
mkdir "$tmp/x"

if [ "$os" = "Darwin" ]; then
  unzip -q "$tmp/$asset" -d "$tmp/x" || die "could not unpack $asset"
  [ -d "$tmp/x/Xade.app" ] || die "Xade.app not found in $asset"
  app_dir="${XADE_APP_DIR:-/Applications}"
  if [ -z "${XADE_APP_DIR:-}" ] && [ ! -w "$app_dir" ]; then
    app_dir="$HOME/Applications"
  fi
  mkdir -p "$app_dir"
  rm -rf "$app_dir/Xade.app"
  cp -R "$tmp/x/Xade.app" "$app_dir/Xade.app"
  cli="$app_dir/Xade.app/Contents/MacOS/xade"
  echo "Installed $app_dir/Xade.app"
else
  tar -xzf "$tmp/$asset" -C "$tmp/x" || die "could not unpack $asset"
  [ -f "$tmp/x/xade" ] || die "xade not found in $asset"
  app_dir="${XADE_APP_DIR:-$HOME/.local/share/xade}"
  apps="$HOME/.local/share/applications"
  icons="$HOME/.local/share/icons"
  mkdir -p "$app_dir" "$apps" "$icons"
  # Replace via rename so a running copy does not block the update.
  cp "$tmp/x/xade" "$app_dir/xade.new"
  chmod 755 "$app_dir/xade.new"
  mv -f "$app_dir/xade.new" "$app_dir/xade"
  cp "$tmp/x/xade.png" "$icons/xade.png"
  sed -e "s|^Exec=.*|Exec=$app_dir/xade|" -e "s|^Icon=.*|Icon=$icons/xade.png|" \
    "$tmp/x/xade.desktop" >"$apps/xade.desktop"
  cli="$app_dir/xade"
  echo "Installed $cli"
  echo "Wrote $apps/xade.desktop and $icons/xade.png"
fi

mkdir -p "$BIN_DIR"
ln -sf "$cli" "$BIN_DIR/xade"
echo "Linked $BIN_DIR/xade -> $cli"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "Add $BIN_DIR to your PATH to run \`xade\` from a terminal, e.g.: export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
esac
echo "Done. Try: xade --help"
