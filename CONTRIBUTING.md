# Contributing

Thanks for helping. This is a community project — **not** an official DeepSeek AI
product — and it stays that way by not implying otherwise. `README.md` covers
what the app is and how it is installed; this file covers working on it.

## Before you start

- **macOS on Apple Silicon.** Only the `darwin-arm64` runtime payload is
  published, so an Intel build needs that payload assembled and released first.
- **Rust** ≥ 1.77.2 (Tauri v2's floor), **Node.js**, **pnpm**, and the Xcode
  Command Line Tools.
- **Use pnpm, not npm.** On some machines `~/.npm` contains root-owned files and
  the `npm` CLI fails with `EPERM`. If you hit that, the permanent fix is
  `sudo chown -R 501:20 ~/.npm`.

```sh
pnpm install
pnpm tauri dev
```

## Version pins

Everything shipped is pinned to an **exact** version. Upstream is a developer
preview that warns about breaking changes, and the runtime is downloaded at first
run, so a moving target is not acceptable. Do not widen a range or bump a pin as
a side effect of an unrelated change — if a pin has to move, say why in the
commit message.

| Component | Pin | Why |
|---|---|---|
| Node.js | 24.21.0 | Krypton LTS; `dsh` needs ≥ 22 for `node:sqlite` |
| `@deepseek-ai/dsh` | 0.1.5-rc.2 | the version the integration contract was verified against |
| Tauri | ≥ 2.11.1 | CVE-2026-42184 (remote origin → local IPC) is fixed there |

## The development loop

`ui/` is served from disk in development and embedded into the binary for a
release build. There is no browser-style live reload, so **restart the app after
a UI change** and rebuild for a release bundle.

Iterating on the shell should not cost a 66 MiB download every time. These
variables override the defaults:

| Variable | Effect |
|---|---|
| `DSH_NODE` + `DSH_BIN` | use a Node and `dsh` you already have; skips runtime acquisition entirely |
| `DSH_GUI_HOME` | the harness home — sessions, settings, credentials |
| `DSH_APP_DATA_DIR` | the app's own data directory |
| `DSH_RUNTIME_MANIFEST` | use a local manifest instead of the embedded one |
| `DSH_RUNTIME_DIR` | where runtime payloads are installed |

> ⚠️ The app must **never** read `DSH_HOME`. That is the CLI's own variable, and
> inheriting it would silently break the isolation this shell guarantees — a user
> with `DSH_HOME` exported would have their real `~/.dsh` mutated. Use
> `DSH_GUI_HOME`.

## Layout

| Path | What lives there |
|---|---|
| `src-tauri/src/main.rs` | window and child-webview layout, sidecar supervisor, orphan reaping, every command |
| `src-tauri/src/runtime.rs` | runtime acquisition: manifest, download, resume, digest, extract |
| `src-tauri/src/terminal.rs` | multi-session PTY and zsh shell integration |
| `src-tauri/src/credentials.rs` | first-run API key: verify and store |
| `ui/index.html` | the right-hand icon rail |
| `ui/content.html` | the loading / download screen |
| `ui/terminal.html` | the xterm.js terminal |
| `ui/vendor/` | vendored xterm.js, the fit addon, and Monaspace Krypton |
| `scripts/` | build, vendor, release, and test helpers |

## Invariants

These are load-bearing. A change that quietly breaks one is worse than a bug,
because nothing fails loudly.

- **Every command calls `assert_caller`.** Tauri makes custom commands invokable
  from *any* webview, and one of our webviews loads a remote origin — the harness
  UI. Each command whitelists the webview it belongs to. A new command without a
  guard hands the harness page IPC.
- **The harness page loads directly at its authenticated URL.** Navigating to it
  from `tauri://localhost` is a cross-site request, so the `SameSite=Strict` auth
  cookie is withheld and the harness answers `authentication required`.
- **The harness home is `DSH_GUI_HOME`**, never `DSH_HOME` (see above).
- **The API key is never logged**, and `.credentials.yaml` stays mode `0600`.
- **`open_help` takes a destination key, not a URL.** A page able to name an
  arbitrary URL could make the app open anything.
- **No context menu, and no devtools in release builds.** On macOS the context
  menu offers "Inspect Element", which is a window into the shell.
- **Never commit `.tauri/dsh-gui.key` or `api-key.txt`.** Both are gitignored.

## Testing

```sh
cargo test --manifest-path src-tauri/Cargo.toml
```

The API-key check talks to DeepSeek, so its live test is `#[ignore]`d and run
explicitly. Wrongly rejecting a *valid* key is the one failure a user cannot work
around, and from the outside it looks exactly like a typo:

```sh
DSH_TEST_API_KEY=sk-… cargo test --manifest-path src-tauri/Cargo.toml -- --ignored
```

To exercise the first-run download without the real 66 MiB transfer, point the
app at a local manifest served by the Range-capable test server:

```sh
python3 scripts/test-range-server.py <dir> 8931 2.0   # 2s per 64 KiB chunk
DSH_RUNTIME_MANIFEST=<dir>/manifest.json DSH_GUI_HOME=… DSH_APP_DATA_DIR=… pnpm tauri dev
```

`THROTTLE` is what makes the download take long enough to watch — it is also how
the resume path was verified, since the server logs every `Range` header.

## Verifying UI changes

Screenshots are the only honest check for layout. Activating the app
programmatically needs Accessibility permission, and a full-screen capture can
miss the window entirely — the window may open on a secondary display, which
makes before/after captures byte-identical and reads exactly like "the window
never opened". Capture the window itself:

```sh
python3 scripts/window-id.py dsh          # list windows, no permissions needed
screencapture -x -o -l <window-id> out.png
```

## Committing

Keep commits focused and explain *why* in the body when it isn't obvious — the
diff already says what changed. `PLAN.md` and `TODO.md` are local working notes
and are gitignored; don't commit them.

## Releasing

1. Write `release-notes/<version>.md`. `release.sh` **requires** it and fails
   without it, because both `latest.json` and the GitHub release embed it — an
   empty file would ship a blank update prompt with nothing to indicate a
   problem.
2. Build and package:

   ```sh
   APPLE_SIGNING_IDENTITY="Apple Development: …" scripts/release.sh 0.1.3
   ```

3. Commit the version bump, push, then publish the artifacts it printed.

**Sign the release.** Without `APPLE_SIGNING_IDENTITY` the binary is only
linker-signed, so its code identity is a hash of the binary and changes on every
build. macOS keys permissions on that identity, so it treats each update as a
different app and re-asks for filesystem access. Signing pins it to
`io.github.delowardev.dshgui`.

**Never reuse a version.** Clients only accept a *greater* version, so a
corrected release at the same number reaches nobody who already installed it.
`release.sh` refuses a tag that already exists.

> ⚠️ **The updater private key is the whole trust root.** `.tauri/dsh-gui.key`
> signs every update and the matching public key is baked into the app. Anyone
> who obtains it can push an update every installed copy will accept; if you
> **lose** it, you can never update an installed build again. Keep it in a
> password manager or a CI secret.

## Reporting bugs

Include the app version, macOS version, and what you were doing. If it involves
the download or startup, the app's stderr is the useful part — it logs each
startup step and the failure reason. Never paste an API key.
