#!/usr/bin/env bash
#
# Build the app icon set from one square source PNG.
#
# Usage: scripts/build-icons.sh [source.png]
#   defaults to assets/harness-logo.png
#
# macOS needs an .icns whose largest entry is 512@2x (1024px). If the source is
# smaller than that, the large entries are upscaled — they will be soft. Replace
# the source with a higher-resolution export and re-run to fix that; nothing else
# has to change.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOURCE="${1:-$REPO_ROOT/static-assets/logo.png}"
ICONS="$REPO_ROOT/src-tauri/icons"
ICONSET="$ICONS/icon.iconset"

[ -f "$SOURCE" ] || { echo "source not found: $SOURCE" >&2; exit 1; }
command -v sips >/dev/null || { echo "sips not found (macOS only)" >&2; exit 1; }
command -v iconutil >/dev/null || { echo "iconutil not found (macOS only)" >&2; exit 1; }

WIDTH="$(sips -g pixelWidth "$SOURCE" | awk '/pixelWidth/{print $2}')"
HEIGHT="$(sips -g pixelHeight "$SOURCE" | awk '/pixelHeight/{print $2}')"
if [ "$WIDTH" != "$HEIGHT" ]; then
  echo "source must be square, got ${WIDTH}x${HEIGHT}" >&2
  exit 1
fi
echo "==> source ${WIDTH}x${HEIGHT}"
if [ "$WIDTH" -lt 1024 ]; then
  echo "    note: below 1024px, so the largest icon entries are upscaled (soft)"
fi

mkdir -p "$ICONS"
rm -rf "$ICONSET"
mkdir -p "$ICONSET"

# The exact names iconutil expects.
emit() { # emit <px> <iconset-name>
  sips -z "$1" "$1" "$SOURCE" --out "$ICONSET/$2" >/dev/null
}
emit 16   icon_16x16.png
emit 32   icon_16x16@2x.png
emit 32   icon_32x32.png
emit 64   icon_32x32@2x.png
emit 128  icon_128x128.png
emit 256  icon_128x128@2x.png
emit 256  icon_256x256.png
emit 512  icon_256x256@2x.png
emit 512  icon_512x512.png
emit 1024 icon_512x512@2x.png

echo "==> building icon.icns"
iconutil -c icns "$ICONSET" -o "$ICONS/icon.icns"

# PNGs Tauri references directly.
sips -z 32   32   "$SOURCE" --out "$ICONS/32x32.png"        >/dev/null
sips -z 128  128  "$SOURCE" --out "$ICONS/128x128.png"      >/dev/null
sips -z 256  256  "$SOURCE" --out "$ICONS/128x128@2x.png"   >/dev/null
sips -z 1024 1024 "$SOURCE" --out "$ICONS/icon.png"         >/dev/null

rm -rf "$ICONSET"

echo
echo "==> done"
ls -1 "$ICONS" | sed 's/^/    /'
echo "    icon.icns: $(du -h "$ICONS/icon.icns" | cut -f1)"
