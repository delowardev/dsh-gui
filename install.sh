#!/usr/bin/env bash
#
# Install DeepSeek Harness (unofficial) on macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/delowardev/dsh-gui/main/install.sh | bash
#
# Why this exists: the app is not signed with a Developer ID and not notarized,
# so a copy downloaded by a *browser* carries com.apple.quarantine and macOS
# refuses it on first launch. `curl` does not set that attribute, and Gatekeeper
# only assesses quarantined apps -- so installing this way avoids the prompt
# entirely rather than teaching people to click through a security warning.
#
# The download is verified against a SHA-256 published with the release. That
# digest is what stands in for a code signature, so it is checked *before*
# anything is extracted or installed.

set -euo pipefail

REPO="delowardev/dsh-gui"
APP_NAME="DeepSeek Harness (unofficial)"
BUNDLE="$APP_NAME.app"

say() { printf '  %s\n' "$*"; }
fail() { printf '\nerror: %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null || fail "curl is required"
command -v ditto >/dev/null || fail "ditto is required (macOS only)"

# --- this build is Apple Silicon only ---------------------------------------
ARCH="$(uname -m)"
[ "$ARCH" = "arm64" ] || fail "this release is Apple Silicon only; got '$ARCH'.
An Intel build needs the darwin-x64 runtime payload assembled and published first."

# --- which release -----------------------------------------------------------
VERSION="${1:-}"
if [ -z "$VERSION" ]; then
  say "resolving the latest release…"
  VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | grep -o '"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"' \
    | head -1 | sed 's/.*"\([^"]*\)"$/\1/')"
  [ -n "$VERSION" ] || fail "could not determine the latest release"
fi
SEMVER="${VERSION#v}"
ARTIFACT="DeepSeek-Harness-unofficial_${SEMVER}_aarch64.app.zip"
BASE="https://github.com/$REPO/releases/download/$VERSION"

say "release $VERSION"

# --- fetch, verify, extract --------------------------------------------------
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

say "downloading $ARTIFACT"
curl -fsSL -o "$WORK/$ARTIFACT" "$BASE/$ARTIFACT" \
  || fail "download failed — is the release published?"

say "verifying checksum"
if ! curl -fsSL -o "$WORK/SHA256SUMS" "$BASE/SHA256SUMS"; then
  fail "this release publishes no SHA256SUMS, so the download cannot be verified.
Refusing to install unverified bytes."
fi
EXPECTED="$(awk -v name="$ARTIFACT" '$2 == name || $2 == "*"name { print $1 }' "$WORK/SHA256SUMS" | head -1)"
[ -n "$EXPECTED" ] || fail "$ARTIFACT is not listed in SHA256SUMS"
ACTUAL="$(shasum -a 256 "$WORK/$ARTIFACT" | awk '{print $1}')"
[ "$EXPECTED" = "$ACTUAL" ] || fail "checksum mismatch
  expected $EXPECTED
  actual   $ACTUAL"

say "extracting"
ditto -x -k "$WORK/$ARTIFACT" "$WORK/unpacked"
[ -d "$WORK/unpacked/$BUNDLE" ] || fail "the archive did not contain $BUNDLE"

# --- install -----------------------------------------------------------------
if pgrep -x dsh-desktop >/dev/null 2>&1; then
  fail "$APP_NAME is running. Quit it and run this again."
fi

if [ -w /Applications ]; then
  DEST="/Applications"
else
  DEST="$HOME/Applications"
  say "/Applications is not writable; installing to $DEST instead"
  mkdir -p "$DEST"
fi

say "installing to $DEST"
rm -rf "$DEST/$BUNDLE"
ditto "$WORK/unpacked/$BUNDLE" "$DEST/$BUNDLE"

# Belt and braces: if the archive itself came from a browser (for example the
# user downloaded it first and ran this from the same folder), the extracted
# copy would be quarantined and would be refused on launch.
xattr -dr com.apple.quarantine "$DEST/$BUNDLE" 2>/dev/null || true

say "installed: $DEST/$BUNDLE"
say "open it from Launchpad, or: open \"$DEST/$BUNDLE\""
