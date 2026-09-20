#!/usr/bin/env bash
#
# Prepare a release: set the version, build, sign, package, checksum, and write
# the updater manifest.
#
#   scripts/release.sh 0.1.3
#
# Requires release-notes/<version>.md to exist. Note the version argument must
# match the filename exactly, so notes for one version can never ship with
# another.
#
# Signing inputs come from the environment:
#   APPLE_SIGNING_IDENTITY          e.g. "Apple Development: you@example.com (XXXXXXXXXX)"
#   TAURI_SIGNING_PRIVATE_KEY_PATH  defaults to .tauri/dsh-gui.key
#
# This prepares artifacts and prints the publish command; it deliberately does
# not publish. Pushing a release is a deliberate act, and a version can never be
# reused once clients have seen it.
#
# Two different signatures are involved and they are not interchangeable:
#   * APPLE_SIGNING_IDENTITY signs the .app for macOS. It gives a *stable code
#     identity*, which is what stops macOS re-asking for permissions on every
#     update. It does NOT satisfy Gatekeeper, which needs a Developer ID and
#     notarization.
#   * the minisign key signs the *updater payload*, and is what the updater
#     verifies. Losing it means installed builds can never be updated again.

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
KEY_PATH="${TAURI_SIGNING_PRIVATE_KEY_PATH:-$REPO_ROOT/.tauri/dsh-gui.key}"
BUNDLE="src-tauri/target/release/bundle"
BASE_NAME="DeepSeek-Harness-unofficial_${VERSION}_aarch64"
REPO="delowardev/dsh-gui"

echo "==> release $TAG"

if git ls-remote --tags --exit-code origin "$TAG" >/dev/null 2>&1; then
  echo "error: $TAG already exists on origin. Clients only accept a greater version," >&2
  echo "       so a corrected release at the same number reaches nobody. Bump it." >&2
  exit 1
fi

if [ -f "$KEY_PATH" ]; then
  # Tauri wants the key's *contents*, not a path.
  export TAURI_SIGNING_PRIVATE_KEY="$(cat "$KEY_PATH")"
  export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}"
else
  echo "error: no updater signing key at $KEY_PATH" >&2
  echo "       Without it the updater payload is unsigned and self-update breaks." >&2
  echo "       Generate one with: pnpm tauri signer generate -w $KEY_PATH" >&2
  exit 1
fi

if [ -z "$IDENTITY" ]; then
  echo "    warning: APPLE_SIGNING_IDENTITY is not set."
  echo "             The build will be ad-hoc signed, so this release gets a fresh"
  echo "             code identity and users will be re-asked for permissions."
else
  echo "    signing the app as: $IDENTITY"
fi

echo "==> setting version to $VERSION"
sed -i '' "s/^version = \".*\"\$/version = \"$VERSION\"/" src-tauri/Cargo.toml
sed -i '' "s/\"version\": \"[^\"]*\"/\"version\": \"$VERSION\"/" src-tauri/tauri.conf.json
sed -i '' "s/\"version\": \"[^\"]*\"/\"version\": \"$VERSION\"/" package.json

echo "==> building"
pnpm tauri build

echo "==> packaging"
rm -rf dist && mkdir -p dist

# Copy the notes in *after* dist/ is wiped. They feed both the updater manifest
# below and `gh release create`, and reading them from dist/ before this point
# is why the manifest's "notes" field -- the text the in-app update prompt shows
# -- could only ever come out empty.
NOTES="$REPO_ROOT/release-notes/${VERSION}.md"
if [ ! -f "$NOTES" ]; then
  echo "error: no release notes at release-notes/${VERSION}.md" >&2
  echo "       Both latest.json and the GitHub release embed them. Without the" >&2
  echo "       file the update prompt ships blank, with nothing to say it broke." >&2
  exit 1
fi
cp "$NOTES" dist/RELEASE_NOTES.md

cp "$BUNDLE/dmg/DeepSeek Harness (unofficial)_${VERSION}_aarch64.dmg" "dist/${BASE_NAME}.dmg"
ditto -c -k --sequesterRsrc --keepParent \
  "$BUNDLE/macos/DeepSeek Harness (unofficial).app" "dist/${BASE_NAME}.app.zip"

# Updater payload. Tauri names these after the product name, so they are renamed
# to the same hyphenated scheme as everything else: the manifest carries a URL,
# and spaces and parentheses in it are a needless source of encoding bugs.
UPDATER_ARCHIVE="$(find "$BUNDLE/macos" -maxdepth 1 -name '*.app.tar.gz' | head -1)"
[ -n "$UPDATER_ARCHIVE" ] || { echo "error: no updater archive was produced" >&2; exit 1; }
[ -f "$UPDATER_ARCHIVE.sig" ] || { echo "error: the updater archive was not signed" >&2; exit 1; }
cp "$UPDATER_ARCHIVE" "dist/${BASE_NAME}.app.tar.gz"
cp "$UPDATER_ARCHIVE.sig" "dist/${BASE_NAME}.app.tar.gz.sig"

# install.sh verifies against this file and refuses to install without it.
( cd dist && shasum -a 256 "${BASE_NAME}.dmg" "${BASE_NAME}.app.zip" > SHA256SUMS )

echo "==> updater manifest"
python3 - "$VERSION" "$BASE_NAME" "$REPO" "$TAG" <<'PY'
import json, pathlib, sys, datetime
version, base, repo, tag = sys.argv[1:5]
dist = pathlib.Path("dist")
signature = (dist / f"{base}.app.tar.gz.sig").read_text().strip()
notes_path = dist / "RELEASE_NOTES.md"
manifest = {
    "version": version,
    "notes": notes_path.read_text() if notes_path.exists() else "",
    "pub_date": datetime.datetime.now(datetime.timezone.utc)
        .isoformat(timespec="seconds").replace("+00:00", "Z"),
    "platforms": {
        # The key the plugin computes for macOS arm64: "darwin" + "aarch64".
        "darwin-aarch64": {
            "signature": signature,
            "url": f"https://github.com/{repo}/releases/download/{tag}/{base}.app.tar.gz",
        }
    },
}
(dist / "latest.json").write_text(json.dumps(manifest, indent=2) + "\n")
print("    wrote dist/latest.json")
PY

echo
echo "==> artifacts"
ls -lh dist | awk 'NR>1 {print "    " $5 "  " $9}'
echo
echo "==> publish with:"
echo "    git add -A && git commit -m \"Release $VERSION\" && git push"
echo "    gh release create $TAG --repo $REPO \\"
echo "      --title \"DeepSeek Harness (unofficial) $VERSION\" \\"
echo "      --notes-file dist/RELEASE_NOTES.md \\"
echo "      dist/${BASE_NAME}.dmg dist/${BASE_NAME}.app.zip \\"
echo "      dist/${BASE_NAME}.app.tar.gz dist/${BASE_NAME}.app.tar.gz.sig \\"
echo "      dist/latest.json dist/SHA256SUMS"
