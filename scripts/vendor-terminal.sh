#!/usr/bin/env bash
#
# Vendor the terminal front-end into ui/vendor/.
#
# The terminal used to be downloaded on first use. It is only ~660 KB in total,
# which is not worth an install step, a network dependency, or the IPC channel
# that shipped the assets to the page -- so it is now built into the app and
# loaded with ordinary <link>/<script> tags.
#
# Downloads are pinned to exact versions and verified by tarball digest, so
# refreshing them is deliberate rather than whatever the registry serves today.
#
# Usage: scripts/vendor-terminal.sh

set -euo pipefail

XTERM_VERSION="6.0.0"
XTERM_SHA="908e66e04af6c8dc6b00dd3b54de088e2e81e5ed866284fd6c2fb3c2d1c7a3f6"
FIT_VERSION="0.11.0"
FIT_SHA="26003b4517a132b64e4ff228fd88a5fda3fff5e606c76093f6dcff772e9ecec0"
FONT_VERSION="5.3.0"
FONT_SHA="95da6cdd8279679be96e46691c0e544de550bfdd012337de09bb1a6ea79534fd"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR="$REPO_ROOT/ui/vendor"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$VENDOR"

fetch() { # fetch <url> <sha256> <out>
  local url="$1" want="$2" out="$3"
  curl -fsSL "$url" -o "$out"
  local got
  got="$(shasum -a 256 "$out" | cut -d' ' -f1)"
  if [ "$got" != "$want" ]; then
    echo "checksum mismatch for $url" >&2
    echo "  expected $want" >&2
    echo "  actual   $got" >&2
    exit 1
  fi
}

echo "==> xterm.js $XTERM_VERSION"
fetch "https://registry.npmjs.org/@xterm/xterm/-/xterm-$XTERM_VERSION.tgz" "$XTERM_SHA" "$WORK/xterm.tgz"
tar -xzf "$WORK/xterm.tgz" -C "$WORK" package/lib/xterm.js package/css/xterm.css
cp "$WORK/package/lib/xterm.js" "$VENDOR/xterm.js"
cp "$WORK/package/css/xterm.css" "$VENDOR/xterm.css"

echo "==> addon-fit $FIT_VERSION"
fetch "https://registry.npmjs.org/@xterm/addon-fit/-/addon-fit-$FIT_VERSION.tgz" "$FIT_SHA" "$WORK/fit.tgz"
tar -xzf "$WORK/fit.tgz" -C "$WORK" package/lib/addon-fit.js
cp "$WORK/package/lib/addon-fit.js" "$VENDOR/addon-fit.js"

echo "==> Monaspace Krypton $FONT_VERSION (SIL OFL 1.1)"
fetch "https://registry.npmjs.org/@fontsource/monaspace-krypton/-/monaspace-krypton-$FONT_VERSION.tgz" "$FONT_SHA" "$WORK/font.tgz"
tar -xzf "$WORK/font.tgz" -C "$WORK" \
  package/files/monaspace-krypton-latin-400-normal.woff2 \
  package/files/monaspace-krypton-latin-400-italic.woff2 \
  package/files/monaspace-krypton-latin-700-normal.woff2 \
  package/files/monaspace-krypton-latin-700-italic.woff2 \
  package/LICENSE
cp "$WORK/package/files/monaspace-krypton-latin-400-normal.woff2" "$VENDOR/font-400.woff2"
cp "$WORK/package/files/monaspace-krypton-latin-400-italic.woff2" "$VENDOR/font-400-italic.woff2"
cp "$WORK/package/files/monaspace-krypton-latin-700-normal.woff2" "$VENDOR/font-700.woff2"
cp "$WORK/package/files/monaspace-krypton-latin-700-italic.woff2" "$VENDOR/font-700-italic.woff2"
cp "$WORK/package/LICENSE" "$VENDOR/MONASPACE-LICENSE.txt"

echo
echo "==> vendored into ui/vendor"
ls -1 "$VENDOR" | sed 's/^/    /'
echo "    total: $(du -sh "$VENDOR" | cut -f1)"
