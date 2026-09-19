# dsh-gui

A lightweight desktop shell for [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) (`dsh`), built with [Tauri v2](https://v2.tauri.app).

> **Not an official DeepSeek AI product.** This is a community project. It is
> not affiliated with, endorsed by, or supported by DeepSeek AI.

## What it is

A native macOS window around the harness, plus a terminal:

- **Agent** — the harness web UI, served locally by `dsh`.
- **Terminal** — a real PTY with session tabs, git-aware prompt, and a
  themeable terminal. Downloaded on first use, not shipped.

It downloads its own runtime on first launch, so **no system Node.js, npm, or
`dsh` install is required**. Nothing is installed globally and nothing is added
to your `PATH`.

## Install

Early builds are **unsigned**, so macOS Gatekeeper will refuse them on first
open. Either right-click the app and choose **Open**, or clear the quarantine
flag:

```sh
xattr -dr com.apple.quarantine "/Applications/DSH GUI.app"
```

Only `darwin-arm64` runtime payloads are published today, so Apple Silicon is
the supported target. An Intel build needs the `darwin-x64` payload assembled
and released first.

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
| Updates | Shell and runtime are downloaded and verified separately |

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

Icons are generated from `assets/harness-logo.png`:

```sh
bash scripts/build-icons.sh
```

Anything with IPC access is a page we wrote. The harness webview is a remote
origin and is granted none; the right-click menu is suppressed in every webview
and devtools are compiled out of release builds.

## License

MIT. DeepSeek Harness is MIT-licensed and remains the property of its authors.
Monaspace is licensed under the SIL Open Font License 1.1.
