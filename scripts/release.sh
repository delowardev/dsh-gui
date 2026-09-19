#!/usr/bin/env bash
#
# Prepare a release: set the version, build, sign, package, checksum.
#
#   APPLE_SIGNING_IDENTITY="Apple Development: you@example.com (XXXXXXXXXX)" \
#     scripts/release.sh 0.1.3
#
# This prepares artifacts and prints the publish command; it deliberately does
# not publish. Pushing a release is a deliberate act, and an unsigned build can
# never be replaced by a signed one at the same version.
#
# Why the signing identity matters even though it does not satisfy Gatekeeper:
# without it the binary is only linker-signed, and its code identity is a hash
# derived from the binary, so it changes on every build. macOS keys permissions
# on that identity, so it treats each update as a different app and re-asks for
# filesystem access. Signing gives a stable identity (`io.github.delowardev.dshgui`)
# and the re-prompting stops. It does NOT make a browser download launch without
# a Gatekeeper prompt -- that needs a Developer ID and notarization.

set -euo pipefail

VERSION="${1:-}"
[ -n "$VERSION" ] || { echo "usage: scripts/release.sh <version>   e.g. 0.1.3" >&2; exit 1; }
case "$VERSION" in
  v*) echo "pass the bare version, without a leading 'v'" >&2; exit 1 ;;
esac

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

TAG="v$VERSION"
IDENTITY="${APPLE_SIGNING_IDENTITY:-}"

echo "==> release $TAG"
if [ -z "$IDENTITY" ]; then
  echo "    warning: APPLE_SIGNING_IDENTITY is not set."
  echo "             The build will be ad-hoc signed, so this release gets a fresh"
  echo "             code identity and users will be re-asked for permissions."
else
  echo "    signing as: $IDENTITY"
fi

# Refuse to reuse a tag: clients only accept a greater version, so a corrected
# same-version release cannot reach anyone who already installed it.
if git ls-remote --tags --exit-code origin "$TAG" >/dev/null 2>&1; then
  echo "error: $TAG already exists on origin. Bump the version instead of reusing it." >&2
  exit 1
fi

echo "==> setting version to $VERSION"
sed -i '' "s/^version = \".*\"\$/version = \"$VERSION\"/" src-tauri/Cargo.toml
sed -i '' "s/\"version\": \"[^\"]*\"/\"version\": \"$VERSION\"/" src-tauri/tauri.conf.json
sed -i '' "s/\"version\": \"[^\"]*\"/\"version\": \"$VERSION\"/" package.json
grep -h "\"version\"" src-tauri/tauri.conf.json package.json | sed 's/^/    /'

echo "==> building"
if [ -n "$IDENTITY" ]; then
  APPLE_SIGNING_IDENTITY="$IDENTITY" pnpm tauri build
else
  pnpm tauri build
fi

BUNDLE="src-tauri/target/release/bundle"
ARTIFACT_BASE="DeepSeek-Harness-unofficial_${VERSION}_aarch64"

echo "==> packaging"
rm -rf dist && mkdir -p dist
cp "$BUNDLE/dmg/DeepSeek Harness (unofficial)_${VERSION}_aarch64.dmg" "dist/${ARTIFACT_BASE}.dmg"
ditto -c -k --sequesterRsrc --keepParent \
  "$BUNDLE/macos/DeepSeek Harness (unofficial).app" "dist/${ARTIFACT_BASE}.app.zip"

# install.sh verifies against this file and refuses to install without it.
( cd dist && shasum -a 256 "${ARTIFACT_BASE}.dmg" "${ARTIFACT_BASE}.app.zip" > SHA256SUMS )

echo
echo "==> artifacts"
ls -lh dist | awk 'NR>1 {print "    " $5 "  " $9}'
echo
cat dist/SHA256SUMS | sed 's/^/    /'
echo
echo "==> publish with:"
echo "    git add -A && git commit -m \"Release $VERSION\" && git push"
echo "    gh release create $TAG --repo delowardev/dsh-gui --title \"DeepSeek Harness (unofficial) $VERSION\" \\"
echo "      --notes-file dist/RELEASE_NOTES.md \\"
echo "      dist/${ARTIFACT_BASE}.dmg dist/${ARTIFACT_BASE}.app.zip dist/SHA256SUMS"
