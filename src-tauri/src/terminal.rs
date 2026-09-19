//! Optional terminal (PLAN.md Phase 4).
//!
//! Split deliberately in two:
//!
//!   * The **shell** runs in Rust through a real PTY (`portable-pty`, compiled
//!     in). That is small and unconditional.
//!   * The **emulator front-end** (`xterm.js`) is *not* shipped. It is
//!     downloaded on first use and pinned by digest, so the base app stays
//!     clean and the terminal remains an explicit opt-in.
//!
//! Sessions are keyed by an opaque id so the UI can hold several at once.
//! Output is buffered per session and drained by the page: a Tauri event stream
//! would be tidier, but events are a core-plugin command and would need an ACL
//! capability, while polling only uses our own commands.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use sha2::{Digest, Sha256};

/// Version directory under `<app-data>/terminal`.
const VERSION: &str = "xterm-6.0.0";

/// One pinned download.
struct Pinned {
    url: &'static str,
    sha256: &'static str,
    /// `(path inside the tarball, file name we write)`.
    members: &'static [(&'static str, &'static str)],
}

/// The terminal front-end, pinned to exact versions and digests.
///
/// Hashes are of the tarballs as published by the registry, so a substituted
/// download is refused rather than executed in a page holding IPC.
const PINNED: &[Pinned] = &[
    Pinned {
        url: "https://registry.npmjs.org/@xterm/xterm/-/xterm-6.0.0.tgz",
        sha256: "908e66e04af6c8dc6b00dd3b54de088e2e81e5ed866284fd6c2fb3c2d1c7a3f6",
        members: &[
            ("package/lib/xterm.js", "xterm.js"),
            ("package/css/xterm.css", "xterm.css"),
        ],
    },
    Pinned {
        url: "https://registry.npmjs.org/@xterm/addon-fit/-/addon-fit-0.11.0.tgz",
        sha256: "26003b4517a132b64e4ff228fd88a5fda3fff5e606c76093f6dcff772e9ecec0",
        members: &[("package/lib/addon-fit.js", "addon-fit.js")],
    },
    // Monaspace Krypton (SIL OFL 1.1), latin subset, via Fontsource. Only the
    // four faces a terminal uses, so the download stays small.
    Pinned {
        url: "https://registry.npmjs.org/@fontsource/monaspace-krypton/-/monaspace-krypton-5.3.0.tgz",
        sha256: "95da6cdd8279679be96e46691c0e544de550bfdd012337de09bb1a6ea79534fd",
        members: &[
            ("package/files/monaspace-krypton-latin-400-normal.woff2", "font-400.woff2"),
            ("package/files/monaspace-krypton-latin-400-italic.woff2", "font-400-italic.woff2"),
            ("package/files/monaspace-krypton-latin-700-normal.woff2", "font-700.woff2"),
            ("package/files/monaspace-krypton-latin-700-italic.woff2", "font-700-italic.woff2"),
            ("package/LICENSE", "MONASPACE-LICENSE.txt"),
        ],
    },
];

/// Install location for the front-end.
pub fn install_root(app_data: &Path) -> PathBuf {
    app_data.join("terminal").join(VERSION)
}

pub fn is_installed(root: &Path) -> bool {
    root.join(".installed").exists()
        && root.join("xterm.js").exists()
        && root.join("addon-fit.js").exists()
        && root.join("font-400.woff2").exists()
}

/// Download, verify, and unpack the terminal front-end.
pub fn install(app_data: &Path) -> Result<(), String> {
    let root = install_root(app_data);
    if is_installed(&root) {
        return Ok(());
    }

    let staging = root.with_extension("staging");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("could not create staging dir: {e}"))?;

    for asset in PINNED {
        let response = ureq::get(asset.url)
            .call()
            .map_err(|e| format!("download failed: {e}"))?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|e| format!("download interrupted: {e}"))?;

        let actual = format!("{:x}", Sha256::digest(&bytes));
        if !actual.eq_ignore_ascii_case(asset.sha256) {
            let _ = fs::remove_dir_all(&staging);
            return Err(format!(
                "terminal asset checksum mismatch for {}\n  expected {}\n  actual   {}",
                asset.url, asset.sha256, actual
            ));
        }

        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]));
        for entry in archive.entries().map_err(|e| e.to_string())? {
            let mut entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path().map_err(|e| e.to_string())?.to_path_buf();
            let Some(target) = asset
                .members
                .iter()
                .find(|(source, _)| Path::new(source) == path)
                .map(|(_, target)| *target)
            else {
                continue;
            };
            let mut contents = Vec::new();
            entry.read_to_end(&mut contents).map_err(|e| e.to_string())?;
            fs::write(staging.join(target), contents)
                .map_err(|e| format!("could not write {target}: {e}"))?;
        }
    }

    fs::write(staging.join(".installed"), VERSION)
        .map_err(|e| format!("could not write the install marker: {e}"))?;

    let _ = fs::remove_dir_all(&root);
    if let Some(parent) = root.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::rename(&staging, &root).map_err(|e| format!("could not install the terminal: {e}"))?;
    Ok(())
}

/// Front-end sources handed to the page to inject.
///
/// Fonts travel as `data:` URLs because the page is served from `tauri://` and
/// cannot reach files in the app data directory directly.
#[derive(serde::Serialize)]
pub struct Assets {
    pub css: String,
    pub xterm: String,
    pub fit: String,
    pub font_regular: String,
    pub font_regular_italic: String,
    pub font_bold: String,
    pub font_bold_italic: String,
}

pub fn assets(app_data: &Path) -> Result<Assets, String> {
    let root = install_root(app_data);
    let read = |name: &str| {
        fs::read_to_string(root.join(name)).map_err(|e| format!("could not read {name}: {e}"))
    };
    let font = |name: &str| -> Result<String, String> {
        use base64::Engine as _;
        let bytes = fs::read(root.join(name)).map_err(|e| format!("could not read {name}: {e}"))?;
        Ok(format!(
            "data:font/woff2;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ))
    };
    Ok(Assets {
        css: read("xterm.css")?,
        xterm: read("xterm.js")?,
        fit: read("addon-fit.js")?,
        font_regular: font("font-400.woff2")?,
        font_regular_italic: font("font-400-italic.woff2")?,
        font_bold: font("font-700.woff2")?,
        font_bold_italic: font("font-700-italic.woff2")?,
    })
}

// --- shell integration ------------------------------------------------------

/// A `ZDOTDIR` that sources the user's own zsh config and then supplies a
/// git-aware prompt.
///
/// The user's files are symlinked in rather than replaced, so `.zshenv`,
/// `.zprofile` and `.zlogin` still run exactly as they would normally; only
/// `.zshrc` is ours, and it sources theirs first. The prompt is installed **only
/// when the config left a stock prompt in place**, so anyone with their own
/// theme keeps it untouched.
fn shell_integration(app_data: &Path) -> Result<PathBuf, String> {
    let dir = app_data.join("shell").join("zsh");
    fs::create_dir_all(&dir).map_err(|e| format!("could not create {dir:?}: {e}"))?;

    let user_zdotdir = std::env::var("ZDOTDIR")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| std::env::var("HOME").unwrap_or_default());

    // Pass through the files we are not replacing.
    for name in [".zshenv", ".zprofile", ".zlogin", ".zlogout"] {
        let source = Path::new(&user_zdotdir).join(name);
        let link = dir.join(name);
        let _ = fs::remove_file(&link);
        if source.exists() {
            let _ = std::os::unix::fs::symlink(&source, &link);
        }
    }

    let zshrc = format!(
        r#"# Generated by DSH Desktop. Sources your own configuration, then offers a
# git-aware prompt if you do not already have one.
if [ -f "{user}/.zshrc" ]; then
  source "{user}/.zshrc"
fi

# Leave a prompt you chose yourself completely alone. These are the stock zsh
# and macOS defaults.
case "$PROMPT" in
  ''|'%m%# '|'%n@%m %1~ %#')
    autoload -Uz vcs_info
    setopt prompt_subst
    zstyle ':vcs_info:git:*' formats ' %F{{magenta}}(%b)%f'
    zstyle ':vcs_info:git:*' actionformats ' %F{{magenta}}(%b|%a)%f'
    _dsh_precmd() {{ vcs_info }}
    precmd_functions+=(_dsh_precmd)
    PROMPT='%F{{cyan}}%1~%f${{vcs_info_msg_0_}} %# '
    ;;
esac
"#,
        user = user_zdotdir
    );
    fs::write(dir.join(".zshrc"), zshrc)
        .map_err(|e| format!("could not write the zsh integration: {e}"))?;

    Ok(dir)
}

// --- sessions ---------------------------------------------------------------

#[derive(serde::Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
}

struct Session {
    id: String,
    title: String,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    output: Arc<Mutex<Vec<u8>>>,
}

fn sessions() -> std::sync::MutexGuard<'static, Vec<Session>> {
    static SESSIONS: Mutex<Vec<Session>> = Mutex::new(Vec::new());
    SESSIONS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Start a login shell and return its session id.
pub fn open(app_data: &Path, path: Option<String>, cols: u16, rows: u16) -> Result<String, String> {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("could not open a pty: {e}"))?;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_owned());
    let mut command = CommandBuilder::new(&shell);
    command.arg("-l");
    // A login shell so the user's own PATH and rc files apply inside it too.
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command.env("TERM", "xterm-256color");

    // Only zsh gets the integration; other shells keep their stock behaviour.
    if shell.ends_with("zsh") {
        if let Ok(dir) = shell_integration(app_data) {
            command.env("ZDOTDIR", dir);
        }
    }

    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|e| format!("could not start {shell}: {e}"))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("could not read from the pty: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("could not write to the pty: {e}"))?;

    let number = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let id = format!("t{number}");
    let output = Arc::new(Mutex::new(Vec::new()));

    let sink = Arc::clone(&output);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if let Ok(mut buffer) = sink.lock() {
                        buffer.extend_from_slice(&chunk[..read]);
                    }
                }
            }
        }
    });

    sessions().push(Session {
        id: id.clone(),
        title: format!("Terminal {number}"),
        master: pair.master,
        writer,
        child,
        output,
    });
    Ok(id)
}

pub fn list() -> Vec<SessionInfo> {
    sessions()
        .iter()
        .map(|session| SessionInfo {
            id: session.id.clone(),
            title: session.title.clone(),
        })
        .collect()
}

/// Drain one session's buffered output.
///
/// A chunk can end mid-UTF-8-sequence, so anything after the last complete
/// character is kept back rather than replaced with a replacement character.
pub fn read(id: &str) -> String {
    let guard = sessions();
    let Some(session) = guard.iter().find(|session| session.id == id) else {
        return String::new();
    };
    let mut output = session.output.lock().unwrap_or_else(|p| p.into_inner());
    if output.is_empty() {
        return String::new();
    }
    match String::from_utf8(output.clone()) {
        Ok(text) => {
            output.clear();
            text
        }
        Err(error) => {
            let valid = error.utf8_error().valid_up_to();
            let remainder = output.split_off(valid);
            let text = String::from_utf8_lossy(&output).into_owned();
            *output = remainder;
            text
        }
    }
}

pub fn write(id: &str, data: &str) -> Result<(), String> {
    let mut guard = sessions();
    let Some(session) = guard.iter_mut().find(|session| session.id == id) else {
        return Err(format!("no such terminal session: {id}"));
    };
    session
        .writer
        .write_all(data.as_bytes())
        .map_err(|e| format!("could not write to the terminal: {e}"))?;
    session
        .writer
        .flush()
        .map_err(|e| format!("could not flush the terminal: {e}"))
}

pub fn resize(id: &str, cols: u16, rows: u16) -> Result<(), String> {
    let mut guard = sessions();
    let Some(session) = guard.iter_mut().find(|session| session.id == id) else {
        return Ok(());
    };
    session
        .master
        .resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("could not resize the terminal: {e}"))
}

pub fn close(id: &str) {
    let mut guard = sessions();
    let Some(index) = guard.iter().position(|session| session.id == id) else {
        return;
    };
    let mut session = guard.remove(index);
    let _ = session.child.kill();
    let _ = session.child.wait();
}

/// Kill every shell. Called on application exit.
pub fn close_all() {
    let mut guard = sessions();
    for mut session in guard.drain(..) {
        let _ = session.child.kill();
        let _ = session.child.wait();
    }
}
