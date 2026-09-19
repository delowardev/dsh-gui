//! DSH Desktop — Tauri shell.
//!
//! One window, several sibling webviews:
//!
//! ```text
//! ┌──────────────────────────────────────────────┐
//! │ chrome   (tab bar, our page, IPC granted)    │  TAB_BAR_HEIGHT
//! ├──────────────────────────────────────────────┤
//! │ agent    (harness UI, remote origin, no IPC) │  rest
//! │   or terminal (local page, no IPC)           │
//! └──────────────────────────────────────────────┘
//! ```
//!
//! Multi-webview is used rather than an iframe on purpose: the harness page
//! stays the **top-level document** of its own webview, so the harness `/api`
//! trust fence is never in question. An iframe under `tauri://` may well be
//! classified `cross-site`, which that fence rejects outright (PLAN.md §3).
//!
//! Requires Tauri's `unstable` feature — `Window::add_child` is gated behind it.
//!
//! Startup order:
//!   1. reap a harness orphaned by a previous run that died without cleanup
//!   2. acquire the pinned runtime (download + verify + install)
//!   3. prepare the isolated harness home and owned profile
//!   4. spawn the harness with a login-shell `PATH`
//!   5. navigate the agent webview to the authenticated URL it prints
//!   6. stop it gracefully on exit, and never leave an orphan

mod runtime;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use tauri::{
    Manager, PhysicalPosition, PhysicalSize, Position, Rect, Size, WebviewUrl, WindowEvent,
};

/// Height of the tab strip, in logical pixels.
const TAB_BAR_HEIGHT: f64 = 38.0;

/// Printed by the `web-runtime` row once the server has bound.
/// See `dsh-web-app/lib/index.js:203`: `dsh web: <url>[ (LAN: <url>)]`
const URL_MARKER: &str = "dsh web: ";

/// Profile this app owns.
///
/// Deliberately NOT `web`: sharing that profile would let a terminal `dsh web`
/// and this app mutate each other's plugin state concurrently. Also NOT
/// `desktop`, which the CLI reserves for the official Electron app.
const PROFILE_NAME: &str = "tauri";

/// Shipped template the owned profile is created from.
const PROFILE_TEMPLATE: &str = "web";

/// Webview labels.
const CHROME: &str = "chrome";
const AGENT: &str = "agent";
const TERMINAL: &str = "terminal";

/// The sidecar handle, owned for the lifetime of the process.
static SIDECAR: Mutex<Option<Child>> = Mutex::new(None);

// --- window layout ----------------------------------------------------------

/// Lay the webviews out for a window of `size`.
///
/// Children do not reflow with the window, so every resize has to be applied by
/// hand. Kept in one place so the three webviews can never disagree.
fn layout(app: &tauri::AppHandle, size: PhysicalSize<u32>) {
    let scale = app
        .get_webview(CHROME)
        .and_then(|webview| webview.window().scale_factor().ok())
        .unwrap_or(1.0);
    let bar = (TAB_BAR_HEIGHT * scale).round().max(1.0) as u32;
    let body_height = size.height.saturating_sub(bar);

    let strip = |label: &str, top: u32, height: u32| {
        if let Some(webview) = app.get_webview(label) {
            let _ = webview.set_bounds(Rect {
                position: Position::Physical(PhysicalPosition::new(0, top as i32)),
                size: Size::Physical(PhysicalSize::new(size.width, height)),
            });
        }
    };

    strip(CHROME, 0, bar);
    strip(AGENT, bar, body_height);
    strip(TERMINAL, bar, body_height);
}

// --- page helpers -----------------------------------------------------------

/// Run a script in one of our own pages.
///
/// `eval` needs no page-side permission, which is what lets the loading and tab
/// chrome be driven without widening the IPC surface.
fn eval_in(app: &tauri::AppHandle, label: &str, script: &str) {
    if let Some(webview) = app.get_webview(label) {
        let _ = webview.eval(script);
    }
}

fn quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

fn report_progress(app: &tauri::AppHandle, fraction: f32, message: &str) {
    eval_in(
        app,
        AGENT,
        &format!(
            "window.__dshProgress && window.__dshProgress({fraction}, {})",
            quoted(message)
        ),
    );
}

fn report_status(app: &tauri::AppHandle, message: &str) {
    eval_in(
        app,
        CHROME,
        &format!("window.__dshStatus && window.__dshStatus({})", quoted(message)),
    );
}

fn report_failure(app: &tauri::AppHandle, message: &str) {
    eprintln!("[shell] startup failed: {message}");
    report_status(app, "Startup failed");
    eval_in(
        app,
        AGENT,
        &format!("window.__dshFailure && window.__dshFailure({})", quoted(message)),
    );
}

// --- sidecar orphan reaping -------------------------------------------------

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
/// Only signals a pid whose live executable still matches the interpreter we
/// recorded, so a recycled pid cannot cause an unrelated program to be killed.
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

// --- environment ------------------------------------------------------------

/// Capture the user's real `PATH` from a login shell.
///
/// A macOS app launched from Finder/Dock inherits launchd's `PATH`
/// (`/usr/bin:/bin:/usr/sbin:/sbin`), not the user's. Without this the harness
/// bash tool, `git`, and model-driven commands fail in confusing ways.
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

// --- harness -----------------------------------------------------------------

/// The authenticated URL printed once the server has bound.
///
/// The line may append a LAN URL in parentheses; `split_whitespace` drops it.
/// Requiring the loopback host and a token means a stray log line can never
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

/// Point the agent webview at the harness.
fn open_harness(app: &tauri::AppHandle, url: &str) {
    let parsed = match tauri::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(error) => {
            report_failure(app, &format!("refusing to open unparsable URL: {error}"));
            return;
        }
    };
    match app.get_webview(AGENT) {
        Some(webview) => {
            if let Err(error) = webview.navigate(parsed) {
                report_failure(app, &format!("could not load the harness UI: {error}"));
            } else {
                report_status(app, "");
            }
        }
        None => report_failure(app, "agent webview is missing"),
    }
}

/// The isolated `DSH_HOME` this app owns.
///
/// An explicit `DSH_HOME` still wins, which keeps the development loop and the
/// test suite able to redirect it.
fn harness_home(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    if let Ok(existing) = std::env::var("DSH_HOME") {
        if !existing.trim().is_empty() {
            return Ok(PathBuf::from(existing));
        }
    }
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve the application data directory: {e}"))?;
    Ok(data_dir.join("harness"))
}

/// Create this app's profile from the shipped template on first run.
///
/// Uses the CLI's own `--from-default-profile` rather than writing the profile
/// files here, so the bundle list stays owned by dsh and cannot drift. `--help`
/// makes the booted app print usage and exit without binding a server.
fn ensure_profile(node: &str, bin: &str, home: &Path) -> Result<(), String> {
    let profile_dir = home.join("profiles").join(PROFILE_NAME);
    if profile_dir.join("package.json").exists() {
        return Ok(());
    }

    eprintln!("[shell] initializing profile '{PROFILE_NAME}' from template '{PROFILE_TEMPLATE}'");
    let output = Command::new(node)
        .arg(bin)
        .args([
            "--profile",
            PROFILE_NAME,
            "--from-default-profile",
            PROFILE_TEMPLATE,
            "--help",
        ])
        .env("DSH_HOME", home)
        .output()
        .map_err(|e| format!("could not initialize the harness profile: {e}"))?;

    if !profile_dir.join("package.json").exists() {
        let stderr: String = String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(800)
            .collect();
        return Err(format!(
            "harness profile initialization failed (exit {:?})\n{stderr}",
            output.status.code()
        ));
    }
    Ok(())
}

/// Resolve the interpreter and dsh entry script to run.
///
/// `DSH_NODE` + `DSH_BIN` together bypass acquisition entirely, which keeps the
/// development loop fast and lets the runtime pipeline be tested in isolation.
fn resolve_runtime(app: &tauri::AppHandle) -> Result<(String, String), String> {
    if let (Ok(node), Ok(bin)) = (std::env::var("DSH_NODE"), std::env::var("DSH_BIN")) {
        eprintln!("[shell] using DSH_NODE/DSH_BIN override");
        return Ok((node, bin));
    }

    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve the application data directory: {e}"))?;

    let handle = app.clone();
    let runtime_dir = runtime::ensure_runtime(&data_dir, &move |fraction, message| {
        report_progress(&handle, fraction, message);
    })?;

    Ok((
        runtime::node_binary(&runtime_dir).to_string_lossy().into_owned(),
        runtime::dsh_entry(&runtime_dir).to_string_lossy().into_owned(),
    ))
}

/// Acquire the runtime, prepare the isolated harness home, then spawn.
fn bootstrap(app: tauri::AppHandle) {
    let (node, bin) = match resolve_runtime(&app) {
        Ok(pair) => pair,
        Err(error) => {
            report_failure(&app, &error);
            return;
        }
    };

    let home = match harness_home(&app) {
        Ok(home) => home,
        Err(error) => {
            report_failure(&app, &error);
            return;
        }
    };

    report_progress(&app, 1.0, "Preparing harness…");
    if let Err(error) = ensure_profile(&node, &bin, &home) {
        report_failure(&app, &error);
        return;
    }

    run_sidecar(app, node, bin, home);
}

fn run_sidecar(app: tauri::AppHandle, node: String, bin: String, home: PathBuf) {
    let mut command = Command::new(&node);
    command
        .arg(&bin)
        // Launcher flags first; everything after reaches the web app
        // (`dsh --profile web --port 8080` is the documented shape).
        .args(["--profile", PROFILE_NAME, "--no-open", "--port", "0"])
        // Set explicitly rather than inherited: a user with DSH_HOME exported
        // would otherwise silently break this app's isolation.
        .env("DSH_HOME", &home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(path) = login_shell_path() {
        command.env("PATH", path);
    } else {
        eprintln!("[shell] warning: could not read a login-shell PATH; using the inherited one");
    }

    eprintln!("[shell] spawning harness: {node}");
    eprintln!("[shell]   DSH_HOME={}", home.display());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            report_failure(&app, &format!("could not start the harness: {error}"));
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
                println!("[shell] harness ready");
                open_harness(&app, &url);
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

// --- tab commands -----------------------------------------------------------

/// Create the terminal webview the first time it is needed.
///
/// Lazy on purpose: the terminal is an opt-in component, so the base app never
/// pays for it at startup.
fn ensure_terminal(app: &tauri::AppHandle) -> Result<(), String> {
    if app.get_webview(TERMINAL).is_some() {
        return Ok(());
    }
    let window = app
        .get_window("main")
        .ok_or_else(|| "main window is missing".to_owned())?;
    let size = window
        .inner_size()
        .map_err(|e| format!("could not size the terminal: {e}"))?;
    let scale = window.scale_factor().unwrap_or(1.0);
    let bar = (TAB_BAR_HEIGHT * scale).round().max(1.0) as u32;

    window
        .add_child(
            tauri::webview::WebviewBuilder::new(
                TERMINAL,
                WebviewUrl::App("terminal.html".into()),
            ),
            PhysicalPosition::new(0, bar as i32),
            PhysicalSize::new(size.width, size.height.saturating_sub(bar)),
        )
        .map_err(|e| format!("could not create the terminal webview: {e}"))?;
    Ok(())
}

/// Switch the visible tab.
///
/// The tab strip only repaints after this returns, so the bar can never claim a
/// tab that is not actually on screen.
#[tauri::command]
fn select_tab(app: tauri::AppHandle, tab: String) -> Result<(), String> {
    match tab.as_str() {
        "agent" => {
            if let Some(terminal) = app.get_webview(TERMINAL) {
                let _ = terminal.hide();
            }
            if let Some(agent) = app.get_webview(AGENT) {
                let _ = agent.show();
            }
            Ok(())
        }
        "terminal" => {
            ensure_terminal(&app)?;
            if let Some(agent) = app.get_webview(AGENT) {
                let _ = agent.hide();
            }
            if let Some(terminal) = app.get_webview(TERMINAL) {
                let _ = terminal.show();
            }
            Ok(())
        }
        other => Err(format!("unknown tab {other:?}")),
    }
}

/// Short status line shown at the right of the tab strip.
#[tauri::command]
fn shell_status() -> String {
    if SIDECAR.lock().map(|guard| guard.is_some()).unwrap_or(false) {
        String::new()
    } else {
        "Starting…".to_owned()
    }
}

fn main() {
    // Before anything else: a previous run may have died without cleanup.
    reap_orphaned_sidecar();

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![select_tab, shell_status])
        .setup(|app| {
            let window = tauri::window::WindowBuilder::new(app, "main")
                .title("DSH Desktop")
                .inner_size(1280.0, 860.0)
                .min_inner_size(720.0, 480.0)
                .build()?;

            let size = window.inner_size()?;
            let scale = window.scale_factor()?;
            let bar = (TAB_BAR_HEIGHT * scale).round().max(1.0) as u32;

            // The tab strip. This is the only surface granted IPC.
            window.add_child(
                tauri::webview::WebviewBuilder::new(
                    CHROME,
                    WebviewUrl::App("index.html".into()),
                ),
                PhysicalPosition::new(0, 0),
                PhysicalSize::new(size.width, bar),
            )?;

            // The harness. A remote origin, granted nothing.
            window.add_child(
                tauri::webview::WebviewBuilder::new(AGENT, WebviewUrl::App("content.html".into())),
                PhysicalPosition::new(0, bar as i32),
                PhysicalSize::new(size.width, size.height.saturating_sub(bar)),
            )?;

            let handle = app.handle().clone();
            window.on_window_event(move |event| {
                if let WindowEvent::Resized(size) = event {
                    layout(&handle, *size);
                }
            });

            let handle = app.handle().clone();
            std::thread::spawn(move || bootstrap(handle));
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
