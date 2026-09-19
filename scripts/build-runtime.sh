#!/usr/bin/env bash
#
# Build the DSH Desktop runtime payload.
#
# Produces a single per-arch tarball containing a pinned Node and a
# production-only `@deepseek-ai/dsh` tree, plus the hash manifest the installer
# verifies against.
#
# Design notes (PLAN.md §2, TODO.md Findings #7):
#   * Node and dsh ship TOGETHER. Splitting them is what permits a native-module
#     ABI mismatch — the exact bug the reference implementation hit (issue #441).
#   * `node-linker=hoisted` yields a flat, self-contained `node_modules`. pnpm's
#     default symlinked layout reaches into a store outside the bundle and would
#     not survive being shipped.
#   * Never run a package manager on the user's machine. This is a build/CI step.
#   * The prune mirrors the official Electron app's runtime-file policy: drop
#     type declarations, source maps, TS sources, and non-target prebuilds.
#
# Usage: scripts/build-runtime.sh [target] [--base-url URL]
#   target defaults to the host triple (e.g. darwin-arm64)

set -euo pipefail

NODE_VERSION="24.21.0"
DSH_VERSION="0.1.5-rc.2"
SCHEMA_VERSION="1"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_DIR="$REPO_ROOT/.runtime-build"

TARGET=""
BASE_URL=""
while [ $# -gt 0 ]; do
  case "$1" in
    --base-url) BASE_URL="${2:?--base-url needs a value}"; shift 2 ;;
    *) TARGET="$1"; shift ;;
  esac
done

host_triple() {
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) echo "darwin-arm64" ;;
    Darwin-x86_64) echo "darwin-x64" ;;
    *) echo "unsupported-$(uname -s)-$(uname -m)" ;;
  esac
}
TARGET="${TARGET:-$(host_triple)}"

case "$TARGET" in
  darwin-arm64|darwin-x64) ;;
  *) echo "unsupported target: $TARGET" >&2; exit 1 ;;
esac

STAGE="$BUILD_DIR/stage-$TARGET"
OUT="$BUILD_DIR/out"
ARTIFACT="dsh-runtime-$DSH_VERSION-$TARGET.tar.gz"
mkdir -p "$OUT"

echo "==> building runtime"
echo "    target=$TARGET  node=$NODE_VERSION  dsh=$DSH_VERSION"
rm -rf "$STAGE"
mkdir -p "$STAGE/runtime"

# --- 1. Node ----------------------------------------------------------------
NODE_TARBALL="node-v$NODE_VERSION-$TARGET.tar.gz"
if [ ! -f "$BUILD_DIR/$NODE_TARBALL" ]; then
  echo "==> fetching node $NODE_VERSION"
  curl -fsSL "https://nodejs.org/dist/v$NODE_VERSION/$NODE_TARBALL" -o "$BUILD_DIR/$NODE_TARBALL"
fi
NODE_SHA="$(shasum -a 256 "$BUILD_DIR/$NODE_TARBALL" | cut -d' ' -f1)"
echo "    node tarball sha256: $NODE_SHA"

# Only the binary and its licence are needed; npm/npx/corepack are dead weight.
tar -xzf "$BUILD_DIR/$NODE_TARBALL" -C "$STAGE/runtime" \
  "node-v$NODE_VERSION-$TARGET/bin/node" \
  "node-v$NODE_VERSION-$TARGET/LICENSE"
mv "$STAGE/runtime/node-v$NODE_VERSION-$TARGET/bin/node" "$STAGE/runtime/node"
mv "$STAGE/runtime/node-v$NODE_VERSION-$TARGET/LICENSE" "$STAGE/runtime/NODE-LICENSE"
rm -rf "$STAGE/runtime/node-v$NODE_VERSION-$TARGET"
chmod +x "$STAGE/runtime/node"

# --- 2. dsh (production only, hoisted) --------------------------------------
APP="$STAGE/app"
mkdir -p "$APP"
cat > "$APP/package.json" <<JSON
{
  "name": "dsh-desktop-runtime",
  "private": true,
  "version": "0.0.0",
  "dependencies": { "@deepseek-ai/dsh": "$DSH_VERSION" }
}
JSON
printf 'node-linker=hoisted\n' > "$APP/.npmrc"

echo "==> installing @deepseek-ai/dsh@$DSH_VERSION (store inside the build dir)"
pnpm install --dir "$APP" --prod \
  --store-dir "$BUILD_DIR/.pnpm-store" \
  --cache-dir "$BUILD_DIR/.pnpm-cache" \
  --reporter append-only

RUNTIME="$STAGE/runtime"
rm -rf "$RUNTIME/node_modules"
mv "$APP/node_modules" "$RUNTIME/node_modules"
mv "$APP/package.json" "$RUNTIME/package.json"
rm -rf "$APP"

# --- 3. Prune ---------------------------------------------------------------
NM="$RUNTIME/node_modules"
echo "==> pruning"
BEFORE="$(du -sk "$RUNTIME" | cut -f1)"

# Non-target native prebuilds and Windows-only helpers.
find "$NM/node-pty/prebuilds" -mindepth 1 -maxdepth 1 ! -name "$TARGET" -exec rm -rf {} + 2>/dev/null || true
rm -rf "$NM/node-pty/third_party"

# Never loaded at runtime: type declarations, source maps, TS sources.
find "$NM" -name '*.d.ts' -delete 2>/dev/null || true
find "$NM" -name '*.map' -delete 2>/dev/null || true
find "$NM" -type f -name '*.ts' -delete 2>/dev/null || true
# Docs, keeping licence/notice files — they are a compliance requirement.
find "$NM" -type f -name '*.md' \
  ! -iname 'LICENSE*' ! -iname 'NOTICE*' ! -iname 'COPYING*' -delete 2>/dev/null || true

AFTER="$(du -sk "$RUNTIME" | cut -f1)"
echo "    $((BEFORE / 1024)) MB -> $((AFTER / 1024)) MB"

# --- 4. Package + manifest --------------------------------------------------
echo "==> packaging $ARTIFACT"
rm -f "$OUT/$ARTIFACT"
# COPYFILE_DISABLE stops bsdtar emitting AppleDouble `._name` sidecars, which the
# installer would otherwise extract onto the user's disk as junk.
COPYFILE_DISABLE=1 tar --no-mac-metadata -czf "$OUT/$ARTIFACT" -C "$STAGE" runtime

SHA="$(shasum -a 256 "$OUT/$ARTIFACT" | cut -d' ' -f1)"
BYTES="$(stat -f '%z' "$OUT/$ARTIFACT")"
FILES="$(tar -tzf "$OUT/$ARTIFACT" | wc -l | tr -d ' ')"
# Logical size, NOT `du`: du reports allocated blocks, and on APFS that reads
# well below the real footprint of a freshly extracted tree (249 MB reported vs
# 318 MB actually materialised). Sum real file sizes so the number we quote
# matches what the user's disk will show.
INSTALLED_BYTES="$(find "$RUNTIME" -type f -exec stat -f '%z' {} + | awk '{ total += $1 } END { print total + 0 }')"

MANIFEST="$OUT/runtime.json"
cat > "$MANIFEST" <<JSON
{
  "schemaVersion": $SCHEMA_VERSION,
  "runtimeVersion": "$DSH_VERSION",
  "nodeVersion": "$NODE_VERSION",
  "artifacts": {
    "$TARGET": {
      "name": "$ARTIFACT",
      "url": "${BASE_URL:+$BASE_URL/}$ARTIFACT",
      "sha256": "$SHA",
      "bytes": $BYTES,
      "installedBytes": $INSTALLED_BYTES,
      "files": $FILES
    }
  }
}
JSON

echo
echo "==> done"
echo "    artifact : $OUT/$ARTIFACT"
echo "    download : $((BYTES / 1024 / 1024)) MB"
echo "    installed: $((INSTALLED_BYTES / 1024 / 1024)) MB"
echo "    files    : $FILES"
echo "    sha256   : $SHA"
echo "    manifest : $MANIFEST"
