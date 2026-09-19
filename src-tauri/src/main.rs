//! DSH Desktop — Tauri shell.
//!
//! One window, several sibling webviews:
//!
//! ```text
//! ┌───────────────────────────────┬──────┐
//! │ title bar band (window's own) │      │
//! ├───────────────────────────────┤ rail │  <- tabs as icons;
//! │ agent   (harness, remote)     │      │     our page, IPC granted
//! │   or terminal (local)         │      │
//! └───────────────────────────────┴──────┘
//!        content: granted no IPC
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
mod terminal;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use tauri::{
    Manager, PhysicalPosition, PhysicalSize, Position, Rect, Size, WebviewUrl, WindowEvent,
};

/// Title bar clearance, in logical pixels.
///
/// Nothing of ours occupies this band: it is left to the window's own
/// background so the native traffic lights sit where macOS puts them. The
/// content starts below it, exactly as under a normal title bar.
const TITLEBAR_HEIGHT: f64 = 28.0;

/// Width of the right tab rail, in logical pixels.
///
/// The tabs live here as icons rather than in a top strip: a rail is not
/// competing with the title bar for vertical space, and it scales to more
/// surfaces later without crowding the chrome.
const RAIL_WIDTH: f64 = 48.0;

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
const LOADING: &str = "loading";
const AGENT: &str = "agent";
const TERMINAL: &str = "terminal";

/// Suppress the right-click menu in every webview.
///
/// On macOS that menu offers "Inspect Element", which is a window into the
/// app's internals that a shipped app has no reason to expose. Applied to the
/// harness page too: the page we did not write is exactly the one whose
/// internals should not be a right-click away.
///
/// This is the *visible* half of the decision. The other half is that devtools
/// are compiled out of release builds entirely (Tauri's `devtools` feature is
/// opt-in, and we do not enable it), so keyboard shortcuts cannot reach them
/// either. Debug builds keep devtools on purpose — losing them would cost us
/// more than it protects, and they are never shipped.
const BLOCK_CONTEXT_MENU: &str = r#"
document.addEventListener(
  "contextmenu",
  (event) => event.preventDefault(),
  { capture: true },
);
"#;

/// The sidecar handle, owned for the lifetime of the process.
static SIDECAR: Mutex<Option<Child>> = Mutex::new(None);

// --- window layout ----------------------------------------------------------

/// Last geometry we laid out, so repeated resize events do not spam the log.
static LAST_LAYOUT: Mutex<Option<(u32, u32, u64)>> = Mutex::new(None);

/// Lay the webviews out for the window's *current* size and scale.
///
/// Queries the window on every call rather than accepting a size, because any
/// geometry read during `setup` is provisional: the window is not yet mapped to
/// a display, so on a Retina screen `scale_factor()` still reports `1.0` and
/// `inner_size()` is equally untrustworthy. Trusting it made the top bar half
/// its intended height and started the harness webview too high, which is what
/// sandwiched the tab strip. Layout is therefore driven by events plus a short
/// settle-in pass, never computed once.
///
/// Children also do not reflow with the window, so every resize has to be
/// applied by hand. Kept in one place so the webviews cannot disagree.
fn layout(app: &tauri::AppHandle) {
    let Some(window) = app.get_window("main") else {
        return;
    };
    let (Ok(size), Ok(scale)) = (window.inner_size(), window.scale_factor()) else {
        return;
    };

    let bar = (TITLEBAR_HEIGHT * scale).round().max(1.0) as u32;
    let rail = (RAIL_WIDTH * scale).round().max(1.0) as u32;
    let body_w = size.width.saturating_sub(rail);
    let body_h = size.height.saturating_sub(bar);

    let key = (size.width, size.height, scale.to_bits());
    {
        let mut last = LAST_LAYOUT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *last != Some(key) {
            *last = Some(key);
            eprintln!(
                "[shell] layout {}x{} @{scale}x -> rail {rail}px, title bar {bar}px, body {body_w}x{body_h}",
                size.width, size.height
            );
        }
    }

    let place = |label: &str, x: u32, top: u32, width: u32, height: u32| {
        if let Some(webview) = app.get_webview(label) {
            let _ = webview.set_bounds(Rect {
                position: Position::Physical(PhysicalPosition::new(x as i32, top as i32)),
                size: Size::Physical(PhysicalSize::new(width, height)),
            });
        }
    };

    // The rail spans the full height on the right, so it reads as a sidebar
    // rather than a toolbar competing with the title bar.
    place(CHROME, body_w, 0, rail, size.height);
    // Content sits below the title bar and to the left of the rail.
    place(LOADING, 0, bar, body_w, body_h);
    place(AGENT, 0, bar, body_w, body_h);
    place(TERMINAL, 0, bar, body_w, body_h);
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
        LOADING,
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
        LOADING,
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

/// Open the harness in its own webview.
///
/// The webview is created *directly on* the authenticated URL rather than being
/// navigated there from our local loading page. That distinction matters: a
/// navigation initiated from `tauri://localhost` is cross-site, and the harness
/// mints its session cookie `SameSite=Strict`. The browser then withholds that
/// cookie from the redirect that immediately follows, so the shell lands on the
/// "authentication required" page holding a cookie it never sends. Creating the
/// webview on the URL means nothing precedes it, so the redirect is same-site.
fn open_harness(app: &tauri::AppHandle, url: &str) {
    let parsed = match tauri::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(error) => {
            report_failure(app, &format!("refusing to open unparsable URL: {error}"));
            return;
        }
    };

    let handle = app.clone();
    let dispatched = app.run_on_main_thread(move || {
        let Some(window) = handle.get_window("main") else {
            report_failure(&handle, "main window is missing");
            return;
        };
        // `layout` reads the live geometry, so the new webview lands in the
        // right place even if the window has been resized since startup.
        let (Ok(size), Ok(scale)) = (window.inner_size(), window.scale_factor()) else {
            report_failure(&handle, "could not size the harness webview");
            return;
        };
        let bar = (TITLEBAR_HEIGHT * scale).round().max(1.0) as u32;
        let rail = (RAIL_WIDTH * scale).round().max(1.0) as u32;

        match window.add_child(
            tauri::webview::WebviewBuilder::new(AGENT, WebviewUrl::External(parsed))
                .initialization_script(BLOCK_CONTEXT_MENU),
            PhysicalPosition::new(0, bar as i32),
            PhysicalSize::new(size.width.saturating_sub(rail), size.height.saturating_sub(bar)),
        ) {
            Ok(_) => {
                // Retire the progress page only once the harness is on screen.
                if let Some(loading) = handle.get_webview(LOADING) {
                    let _ = loading.close();
                }
                layout(&handle);
                report_status(&handle, "");
            }
            Err(error) => report_failure(&handle, &format!("could not open the harness: {error}")),
        }
    });

    if let Err(error) = dispatched {
        eprintln!("[shell] could not dispatch harness webview creation: {error}");
    }
}

/// The isolated `DSH_HOME` this app owns.
///
/// Deliberately does **not** read `DSH_HOME`. That variable belongs to the
/// harness CLI, and any user who exports it would otherwise launch this app
/// straight into their real `~/.dsh` — silently contradicting the promise that
/// the app's data is its own and does not touch an existing `dsh` install. The
/// dev/test override is therefore a distinct name that no user would happen to
/// have set.
fn harness_home(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    if let Ok(override_dir) = std::env::var("DSH_GUI_HOME") {
        if !override_dir.trim().is_empty() {
            return Ok(PathBuf::from(override_dir));
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

    report_progress(&app, -1.0, "Preparing harness…");
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
    report_progress(&app, -1.0, "Starting the harness…");

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
    let bar = (TITLEBAR_HEIGHT * scale).round().max(1.0) as u32;
    let rail = (RAIL_WIDTH * scale).round().max(1.0) as u32;

    window
        .add_child(
            tauri::webview::WebviewBuilder::new(
                TERMINAL,
                WebviewUrl::App("terminal.html".into()),
            )
            .initialization_script(BLOCK_CONTEXT_MENU),
            PhysicalPosition::new(0, bar as i32),
            PhysicalSize::new(size.width.saturating_sub(rail), size.height.saturating_sub(bar)),
        )
        .map_err(|e| format!("could not create the terminal webview: {e}"))?;
    Ok(())
}

/// Only our own surfaces may drive a command.
///
/// Custom commands are invokable by *every* webview by default — including the
/// remote harness page. For tab switching that is merely untidy; for the
/// terminal commands it would hand a shell to whatever the harness happens to be
/// rendering. Checking the caller here is what keeps that boundary shut. (An ACL
/// capability scoped by webview label enforces the same thing; this is explicit
/// and stays auditable in one place.)
fn assert_caller(webview: &tauri::Webview, allowed: &[&str]) -> Result<(), String> {
    let label = webview.label();
    if allowed.contains(&label) {
        return Ok(());
    }
    Err(format!(
        "webview {label:?} is not permitted to call this command"
    ))
}

/// The directory this app owns for installed extras.
///
/// `DSH_APP_DATA_DIR` overrides it, which keeps the development loop and the
/// test suite able to redirect installs without touching the real app data.
fn app_data(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    if let Ok(override_dir) = std::env::var("DSH_APP_DATA_DIR") {
        if !override_dir.trim().is_empty() {
            return Ok(PathBuf::from(override_dir));
        }
    }
    app.path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve the application data directory: {e}"))
}

/// Switch the visible tab.
///
/// The rail only repaints after this returns, so it can never claim a tab that
/// is not actually on screen. The shell is deliberately *not* stopped here:
/// hiding the terminal should not discard the session.
#[tauri::command]
fn select_tab(app: tauri::AppHandle, webview: tauri::Webview, tab: String) -> Result<(), String> {
    assert_caller(&webview, &[CHROME])?;
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

/// Whether the harness process is up yet.
#[tauri::command]
fn shell_status(webview: tauri::Webview) -> Result<String, String> {
    assert_caller(&webview, &[CHROME])?;
    Ok(
        if SIDECAR.lock().map(|guard| guard.is_some()).unwrap_or(false) {
            String::new()
        } else {
            "Starting…".to_owned()
        },
    )
}

// --- terminal commands ------------------------------------------------------
//
// The emulator front-end is vendored into `ui/vendor`, so there is no install
// step and no command to hand assets to the page. These are sessions only.

#[tauri::command]
fn terminal_open(
    app: tauri::AppHandle,
    webview: tauri::Webview,
    cols: u16,
    rows: u16,
) -> Result<String, String> {
    assert_caller(&webview, &[TERMINAL])?;
    terminal::open(&app_data(&app)?, login_shell_path(), cols, rows)
        .inspect_err(|error| eprintln!("[shell] terminal open failed: {error}"))
}

#[tauri::command]
fn terminal_list(webview: tauri::Webview) -> Result<Vec<terminal::SessionInfo>, String> {
    assert_caller(&webview, &[TERMINAL])?;
    Ok(terminal::list())
}

#[tauri::command]
fn terminal_write(webview: tauri::Webview, id: String, data: String) -> Result<(), String> {
    assert_caller(&webview, &[TERMINAL])?;
    terminal::write(&id, &data)
}

/// Drain one session's buffered output. Polled by the page — see the note in
/// `src/terminal.rs` for why this is not an event stream.
#[tauri::command]
fn terminal_read(webview: tauri::Webview, id: String) -> Result<String, String> {
    assert_caller(&webview, &[TERMINAL])?;
    Ok(terminal::read(&id))
}

#[tauri::command]
fn terminal_resize(
    webview: tauri::Webview,
    id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    assert_caller(&webview, &[TERMINAL])?;
    terminal::resize(&id, cols, rows)
}

#[tauri::command]
fn terminal_close(webview: tauri::Webview, id: String) -> Result<(), String> {
    assert_caller(&webview, &[TERMINAL])?;
    terminal::close(&id);
    Ok(())
}

fn main() {
    // Before anything else: a previous run may have died without cleanup.
    reap_orphaned_sidecar();

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            select_tab,
            shell_status,
            terminal_open,
            terminal_list,
            terminal_write,
            terminal_read,
            terminal_resize,
            terminal_close
        ])
        .setup(|app| {
            // Overlay title bar: transparent, with the content view spanning the
            // whole window. Without this, Tauri still enables a fullsize content
            // view for the *default* style, so our top bar would be drawn
            // underneath an opaque title bar and mostly hidden.
            #[allow(unused_mut)]
            let mut window_builder = tauri::window::WindowBuilder::new(app, "main")
                .title("DeepSeek Harness (unofficial)")
                .inner_size(1280.0, 860.0)
                .min_inner_size(720.0, 480.0);
            #[cfg(target_os = "macos")]
            {
                window_builder = window_builder
                    .title_bar_style(tauri::TitleBarStyle::Overlay)
                    // With a transparent title bar the window title would still
                    // draw over our tab strip.
                    .hidden_title(true);
            }
            let window = window_builder.build()?;

            // Placeholder bounds; layout() corrects them as soon as the window
            // is mapped to its display. See the note on `layout`.
            //
            // Transparent so the rail sits on the window's own background
            // instead of an opaque strip beside the content.
            window.add_child(
                tauri::webview::WebviewBuilder::new(CHROME, WebviewUrl::App("index.html".into()))
                    .transparent(true)
                    .initialization_script(BLOCK_CONTEXT_MENU),
                PhysicalPosition::new(1232, 0),
                PhysicalSize::new(48, 860),
            )?;

            // Startup progress. Closed once the harness webview exists.
            window.add_child(
                tauri::webview::WebviewBuilder::new(LOADING, WebviewUrl::App("content.html".into()))
                    .initialization_script(BLOCK_CONTEXT_MENU),
                PhysicalPosition::new(0, 28),
                PhysicalSize::new(1232, 832),
            )?;

            let handle = app.handle().clone();
            window.on_window_event(move |event| match event {
                WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                    layout(&handle)
                }
                _ => {}
            });
            layout(app.handle());

            // Settle-in pass: at this point the window is still not on a display,
            // so the values just read are provisional. Re-apply a few times until
            // the real scale factor and size arrive.
            let settle = app.handle().clone();
            std::thread::spawn(move || {
                for delay in [80u64, 250, 600, 1200] {
                    std::thread::sleep(Duration::from_millis(delay));
                    let handle = settle.clone();
                    let _ = settle.run_on_main_thread(move || layout(&handle));
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
                terminal::close_all();
            }
        });
}
