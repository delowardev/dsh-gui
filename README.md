# DeepSeek Harness (unofficial)

A lightweight desktop shell for [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) (`dsh`), built with [Tauri v2](https://v2.tauri.app).

> **Not an official DeepSeek AI product.** This is a community project. It is
> not affiliated with, endorsed by, or supported by DeepSeek AI. The name says
> so deliberately.

## What it is

A native macOS window around the harness, plus a terminal:

- **Agent** — the harness web UI, served locally by `dsh`.
- **Terminal** — a real PTY with session tabs, git-aware prompt, and a
  themeable terminal. Downloaded on first use, not shipped.

It downloads its own runtime on first launch, so **no system Node.js, npm, or
`dsh` install is required**. Nothing is installed globally and nothing is added
to your `PATH`.

## Install

### Install with one command (recommended)

```sh
curl -fsSL https://raw.githubusercontent.com/delowardev/dsh-gui/main/install.sh | bash
```

This downloads the release, **verifies it against the published SHA-256**, and
installs to `/Applications` (or `~/Applications` if that isn't writable).

It is also the only path that starts without a Gatekeeper prompt, and that is
deliberate rather than a trick:

- The app is not signed with a Developer ID and not notarized, so macOS refuses
  it on first launch.
- What macOS actually acts on is the `com.apple.quarantine` attribute, and that
  is applied by **the application doing the download** — browsers opt in, `curl`
  does not.
- Gatekeeper only assesses quarantined apps, so a copy fetched with `curl` is
  never assessed.

The published SHA-256 is therefore what stands in for a code signature, which
is why the installer refuses to proceed without it.

### Or install from the DMG

Download the DMG, drag the app to Applications, then **right-click → Open** the
first time (a browser download *is* quarantined). Alternatively:

```sh
xattr -dr com.apple.quarantine "/Applications/DeepSeek Harness (unofficial).app"
```

### Requirements

Apple Silicon. Only `darwin-arm64` runtime payloads are published; an Intel
build needs the `darwin-x64` payload assembled and released first.

## First launch

The app downloads a pinned runtime (~66 MiB) on first run: Node 24.21.0 plus
`@deepseek-ai/dsh` 0.1.5-rc.2, verified against a SHA-256 digest baked into the
app before anything is extracted. Expect it to take a minute or so.

Everything it stores lives under `~/Library/Application Support/io.github.delowardev.dshgui`:

```
harness/     your sessions, settings, credentials (the harness home)
runtimes/    installed runtime payloads
terminal/    the terminal front-end, once you install it
```

That directory is private to this app and is not shared with a `dsh`
installation you already have.

## Features

| | |
|---|---|
| Tabs | Agent and Terminal, as icons in a right-hand rail |
| Terminal | Real PTY, session sidebar (drag to resize), Monaspace Krypton |
| Theme | Vira Graphene, matching the VS Code theme of the same name |
| Prompt | git branch shown when your prompt is still a stock default |
| Updates | Self-updating; the update payload is verified against a minisign key baked into the app |

The terminal's emulator front-end (`xterm.js` and the font) is downloaded on
first use and pinned by digest, so it stays out of the base install.

## Build from source

Requires Rust, Node.js, and pnpm.

```sh
pnpm install
pnpm tauri dev      # development
pnpm tauri build    # release bundle
```

The runtime payload is built separately and published as its own release:

```sh
bash scripts/build-runtime.sh --base-url https://github.com/<owner>/<repo>/releases/download/<tag>
```

Icons are generated from `static-assets/logo.png` (800×800):

```sh
bash scripts/build-icons.sh
```

Anything with IPC access is a page we wrote. The harness webview is a remote
origin and is granted none; the right-click menu is suppressed in every webview
and devtools are compiled out of release builds.

## Releasing

Write `release-notes/<version>.md` first — `release.sh` requires it, because both
`latest.json` and the GitHub release embed it.

```sh
APPLE_SIGNING_IDENTITY="Apple Development: …" scripts/release.sh 0.1.3
```

Sets the version, builds, signs, packages `dist/`, writes `SHA256SUMS` and
`latest.json`, and prints the publish command. Publishing itself stays a
deliberate step.

> **Keep the updater private key safe.** `.tauri/dsh-gui.key` signs every update,
> and the matching public key is baked into the app. Anyone who obtains it can
> push an update that every installed copy will accept; if you **lose** it, you
> can never update an installed build again. Store it in a password manager or a
> CI secret — it is gitignored and must never be committed.

**Sign the release.** Without `APPLE_SIGNING_IDENTITY` the binary is only
linker-signed and its code identity is a hash derived from the binary, so it
changes on every build. macOS keys permissions on that identity, which means it
treats each update as a different app and re-asks for filesystem access. Signing
pins it to `io.github.delowardev.dshgui` and the re-prompting stops.

Signing does **not** remove the Gatekeeper prompt for browser downloads — that
needs a Developer ID certificate and notarization, which requires a paid Apple
Developer Program membership. It is why `install.sh` is the recommended path,
and why every release must publish `SHA256SUMS`: that digest is what stands in
for a signature.

Never reuse a version. Clients only accept a greater version, so a corrected
release at the same number cannot reach anyone who already installed it;
`release.sh` refuses a tag that already exists.

## License

MIT. DeepSeek Harness is MIT-licensed and remains the property of its authors.
Monaspace is licensed under the SIL Open Font License 1.1.
