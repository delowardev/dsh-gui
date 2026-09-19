//! DSH Desktop — M1 spike shell.
//!
//! Proves the Phase 0.1 integration path end to end:
//!
//!   1. spawn `dsh --profile web --no-open --port 0` with a login-shell `PATH`
//!   2. parse the authenticated URL it prints on stdout
//!   3. open a window on that URL
//!   4. terminate the child gracefully when the app exits, and never leave an
//!      orphan behind if it does not
//!
//! Deliberately minimal. No runtime download yet (Phase 1.1), no bundled
//! frontend beyond the loading page, and **no capabilities granted to the
//! loaded page** — it is a remote origin (see PLAN.md §4).
//!
//! Phase 1.1 replaces `DEFAULT_DSH_BIN` with the downloaded runtime. Phase 1.2
//! adds `DSH_HOME` isolation — note this spike still inherits `DSH_HOME`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

/// Printed by the `web-runtime` row once the server has bound.
/// See `dsh-web-app/lib/index.js:203`: `dsh web: <url>[ (LAN: <url>)]`
const URL_MARKER: &str = "dsh web: ";

/// Spike-only fallback. Replaced by the downloaded runtime in Phase 1.1.
/// Override with `DSH_BIN` to point at any `@deepseek-ai/dsh` `lib/bin.js`.
const DEFAULT_DSH_BIN: &str =
    "/Users/delowar/.npm/_npx/1e7f6d9597241db0/node_modules/@deepseek-ai/dsh/lib/bin.js";

/// The sidecar handle, owned for the lifetime of the process.
static SIDECAR: Mutex<Option<Child>> = Mutex::new(None);

/// Recorded so a crash or Force Quit cannot orphan the harness (Phase 1.4).
fn pid_file_path() -> PathBuf {
    std::env::temp_dir().join("dsh-desktop.sidecar.pid")
}

/// Executable path of a live pid, or `None` when the process is gone.
///
/// Uses `proc_pidpath` rather than shelling out to `ps`: no subprocess, and it
/// doubles as a liveness check. A subprocess version is also fragile — `ps` is
/// unavailable under a restricted sandbox, which silently disabled reaping.
fn process_executable(pid: i32) -> Option<String> {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buffer` is valid for `buffer.len()` bytes; the kernel only writes
    // up to that length and returns the number of bytes written.
    let written =
        unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if written <= 0 {
        return None;
    }
    buffer.truncate(written as usize);
    String::from_utf8(buffer).ok()
}

/// Whether a live process is still the interpreter we originally spawned.
///
/// `expected` is absolute when `DSH_NODE` is set; otherwise it may just be
/// `node`, so fall back to comparing file names.
fn executable_matches(actual: &str, expected: &str) -> bool {
    if expected.starts_with('/') {
        actual == expected
    } else {
        Path::new(actual)
            .file_name()
            .is_some_and(|name| name.to_string_lossy() == expected)
    }
}

/// Kill a harness left running by a previous run that died without cleanup.
///
/// The pid file records the interpreter we spawned, and we only signal a pid
/// whose live executable still matches it exactly. That makes a recycled pid —
/// which would otherwise mean killing an unrelated program — effectively
/// impossible to act on.
fn reap_orphaned_sidecar() {
    let path = pid_file_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    let _ = std::fs::remove_file(&path);

    let mut fields = raw.trim().splitn(2, '\t');
    let (Some(pid_field), Some(expected_exe)) = (fields.next(), fields.next()) else {
        return;
    };
    let Ok(pid) = pid_field.parse::<i32>() else {
        return;
    };
    // No such process, or the pid now belongs to something else entirely.
    if !process_executable(pid).is_some_and(|actual| executable_matches(&actual, expected_exe)) {
        return;
    }

    eprintln!("[shell] reaping orphaned harness from a previous run (pid {pid})");
    // SAFETY: the pid's live executable matches the interpreter we recorded.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(100));
        if process_executable(pid).is_none() {
            return;
        }
    }
    // SAFETY: as above; SIGTERM was ignored or the process is wedged.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

/// Capture the user's real `PATH` from a login shell.
///
/// A macOS app launched from Finder/Dock inherits launchd's `PATH`
/// (`/usr/bin:/bin:/usr/sbin:/sbin`), not the user's. Without this the harness
/// bash tool, `git`, and model-driven commands fail in confusing ways.
///
/// Note this is exactly why the runtime must be *bundled and pinned*: on this
/// machine the login shell resolves `node` to a different install than the
/// invoking environment does (PLAN.md §2). See TODO.md "Version locks".
fn login_shell_path() -> Option<String> {
    let output = Command::new("/bin/zsh")
        .args(["-lic", "env -0"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .find_map(|entry| entry.strip_prefix("PATH="))
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
}

/// Pull the authenticated URL out of one stdout line.
///
/// The line may append a LAN URL in parentheses; `split_whitespace` drops it.
/// We require the loopback host and a token so a stray log line can never
/// redirect the window somewhere unexpected.
fn extract_url(line: &str) -> Option<String> {
    let rest = line.split_once(URL_MARKER)?.1;
    let candidate = rest.split_whitespace().next()?;
    let is_loopback = candidate.starts_with("http://127.0.0.1:");
    if is_loopback && candidate.contains("/?token=") {
        Some(candidate.to_owned())
    } else {
        None
    }
}

/// Open the real window on the harness URL and retire the loading window.
///
/// Window creation must happen on the main thread, hence `run_on_main_thread`.
fn open_main_window(app: &tauri::AppHandle, url: &str) {
    let parsed = match tauri::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("[shell] refusing to open unparsable URL {url:?}: {error}");
            return;
        }
    };

    let dispatcher = app.clone();
    let window_app = app.clone();
    let result = dispatcher.run_on_main_thread(move || {
        match WebviewWindowBuilder::new(&window_app, "main", WebviewUrl::External(parsed))
            .title("DSH Desktop")
            .inner_size(1280.0, 860.0)
            .min_inner_size(720.0, 480.0)
            .build()
        {
            Ok(_) => {
                if let Some(loading) = window_app.get_webview_window("loading") {
                    let _ = loading.close();
                }
            }
            Err(error) => eprintln!("[shell] failed to open the main window: {error}"),
        }
    });

    if let Err(error) = result {
        eprintln!("[shell] could not dispatch window creation: {error}");
    }
}

/// Spawn the harness and stream its output until it exits.
fn run_sidecar(app: tauri::AppHandle) {
    let node = std::env::var("DSH_NODE").unwrap_or_else(|_| "node".to_owned());
    let bin = std::env::var("DSH_BIN").unwrap_or_else(|_| DEFAULT_DSH_BIN.to_owned());

    let mut command = Command::new(&node);
    command
        .arg(&bin)
        // Launcher flags first; everything after reaches the web app
        // (`dsh --profile web --port 8080` is the documented shape).
        .args(["--profile", "web", "--no-open", "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(path) = login_shell_path() {
        command.env("PATH", path);
    } else {
        eprintln!("[shell] warning: could not read a login-shell PATH; using the inherited one");
    }

    eprintln!("[shell] spawning: {node} {bin} --profile web --no-open --port 0");

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("[shell] could not spawn the harness: {error}");
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let pid = child.id();
    let _ = std::fs::write(pid_file_path(), format!("{pid}\t{node}"));
    *SIDECAR.lock().expect("sidecar mutex poisoned") = Some(child);

    if let Some(stderr) = stderr {
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("[dsh:err] {line}");
            }
        });
    }

    if let Some(stdout) = stdout {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            println!("[dsh] {line}");
            if let Some(url) = extract_url(&line) {
                println!("[shell] harness ready, opening window");
                open_main_window(&app, &url);
            }
        }
    }

    eprintln!("[shell] harness process {pid} exited");
    let _ = std::fs::remove_file(pid_file_path());
}

/// Send SIGTERM so the harness disposes its fiber tree and flushes sessions.
/// `Child::kill` would SIGKILL, which skips that (PLAN.md §3).
fn stop_sidecar() {
    let mut guard = match SIDECAR.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(mut child) = guard.take() {
        let pid = child.id() as i32;
        eprintln!("[shell] stopping harness (pid {pid})");
        // SAFETY: `pid` is a live child of this process; SIGTERM is always safe.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        // Give the harness a moment to shut down cleanly before reaping.
        for _ in 0..50 {
            match child.try_wait() {
                Ok(Some(_)) => {
                    let _ = std::fs::remove_file(pid_file_path());
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => break,
            }
        }
        eprintln!("[shell] harness did not exit in time; killing");
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(pid_file_path());
    }
}

fn main() {
    // Before anything else: a previous run may have died without cleanup.
    reap_orphaned_sidecar();

    tauri::Builder::default()
        .setup(|app| {
            // A small loading window while the harness boots. The real window is
            // created once the authenticated URL is known.
            WebviewWindowBuilder::new(app, "loading", WebviewUrl::App("index.html".into()))
                .title("DSH Desktop")
                .inner_size(460.0, 300.0)
                .resizable(false)
                .build()?;

            let handle = app.handle().clone();
            std::thread::spawn(move || run_sidecar(handle));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build the Tauri application")
        .run(|_app, event| {
            if let tauri::RunEvent::Exit = event {
                stop_sidecar();
            }
        });
}
