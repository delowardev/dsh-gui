//! Runtime acquisition (PLAN.md Phase 1.1).
//!
//! The app ships without a Node runtime. On first launch it downloads a pinned,
//! prebuilt per-arch payload, verifies it against a digest **baked into the
//! signed app**, and installs it atomically.
//!
//! Why it is written this way:
//!
//!   * **HTTP happens in-process** (Rust client, not a spawned `curl`). Shelling
//!     out risks stamping `com.apple.quarantine` onto everything the child
//!     writes, and ad-hoc-signed native modules are killed outright by Gatekeeper
//!     rather than merely warned about.
//!   * **Verify before extract.** The manifest digest is the security boundary —
//!     the payload sits outside our notarization, so a compromised host must not
//!     be able to substitute code.
//!   * **Install atomically.** Extract into a staging directory and `rename` into
//!     place, so a half-written runtime is never visible to the next launch.
//!   * **Never run a package manager on the user's machine.** The payload is
//!     built in CI by `scripts/build-runtime.sh`.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Attempts before giving up. A 66 MiB transfer over a flaky link rarely fails
/// the same way twice, so retrying is worth more than reporting immediately.
const DOWNLOAD_ATTEMPTS: u32 = 4;

/// Manifest baked into the binary at build time. Regenerated per release by CI.
const EMBEDDED_MANIFEST: &str = include_str!("../runtime-manifest.json");

/// Manifest layout this build understands. A newer manifest (for example one
/// adding a second artifact shape) must not be silently half-read.
const SUPPORTED_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    pub runtime_version: String,
    pub node_version: String,
    pub artifacts: HashMap<String, Artifact>,
}

#[derive(Debug, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub url: String,
    pub sha256: String,
    #[allow(dead_code)]
    pub bytes: u64,
}

/// Target triple naming used by the released artifacts.
pub fn host_target() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "darwin-arm64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "darwin-x64"
    } else {
        "unsupported"
    }
}

/// Load the manifest: an explicit file when `DSH_RUNTIME_MANIFEST` is set
/// (useful for local testing), otherwise the embedded one.
pub fn load_manifest() -> Result<Manifest, String> {
    let raw = match std::env::var("DSH_RUNTIME_MANIFEST") {
        Ok(path) => fs::read_to_string(&path)
            .map_err(|e| format!("could not read manifest {path}: {e}"))?,
        Err(_) => EMBEDDED_MANIFEST.to_owned(),
    };
    let manifest: Manifest =
        serde_json::from_str(&raw).map_err(|e| format!("invalid runtime manifest: {e}"))?;
    if manifest.schema_version != SUPPORTED_SCHEMA_VERSION {
        return Err(format!(
            "unsupported runtime manifest schema {} (this build supports {})",
            manifest.schema_version, SUPPORTED_SCHEMA_VERSION
        ));
    }
    Ok(manifest)
}

/// Root directory holding every installed runtime version.
pub fn runtimes_root(app_data_dir: &Path) -> PathBuf {
    std::env::var("DSH_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| app_data_dir.join("runtimes"))
}

/// The interpreter this runtime version installs.
pub fn node_binary(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("runtime").join("node")
}

/// The dsh entry script inside an installed runtime.
pub fn dsh_entry(runtime_dir: &Path) -> PathBuf {
    runtime_dir
        .join("runtime")
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("lib")
        .join("bin.js")
}

/// Marker written only after a payload has been verified and extracted.
fn installed_marker(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(".installed")
}

/// Whether a usable runtime is already installed at this version.
pub fn is_installed(runtime_dir: &Path) -> bool {
    installed_marker(runtime_dir).exists() && node_binary(runtime_dir).exists()
}

/// Remove staging leftovers from an interrupted install.
fn clean_staging(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".staging-") {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Discard partial downloads belonging to a different payload.
///
/// Only the current digest's partial is kept, so a superseded version cannot
/// leave tens of megabytes behind forever.
fn forget_other_partials(root: &Path, keep: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".download-") && name.ends_with(".part") && path != keep {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Turn a transport failure into something a person can act on.
///
/// "download failed: i/o error" tells the user nothing; "are you offline?"
/// tells them what to do.
fn describe_failure(error: &ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, _) => match *code {
            404 => "the download URL was not found (404) — the release may have been removed".to_owned(),
            403 => "access denied (403) — the release may not be published".to_owned(),
            other => format!("the server returned HTTP {other}"),
        },
        ureq::Error::Transport(transport) => {
            let text = transport.to_string();
            let lower = text.to_lowercase();
            if lower.contains("timed out") || lower.contains("timeout") {
                "the connection timed out".to_owned()
            } else if lower.contains("resolve") || lower.contains("dns") {
                "the download host could not be reached — are you offline?".to_owned()
            } else if lower.contains("connection") || lower.contains("closed") || lower.contains("reset") {
                "the connection was interrupted".to_owned()
            } else {
                format!("network error: {text}")
            }
        }
    }
}

/// Hash a file on disk.
fn digest_of(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("could not read the download: {e}"))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| format!("could not read the download: {e}"))?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// One attempt, resuming from whatever is already on disk when the server allows.
fn transfer(
    partial: &Path,
    artifact: &Artifact,
    on_progress: &dyn Fn(f32, &str),
) -> Result<(), String> {
    let already = fs::metadata(partial).map(|m| m.len()).unwrap_or(0).min(artifact.bytes);

    let mut request = ureq::get(&artifact.url);
    if already > 0 {
        request = request.set("Range", &format!("bytes={already}-"));
    }
    let response = request.call().map_err(|e| describe_failure(&e))?;

    let resuming = already > 0 && response.status() == 206;
    if already > 0 && !resuming {
        // The server ignored the range and is resending from the start.
        let _ = fs::remove_file(partial);
    }

    let mut written = if resuming { already } else { 0 };
    let mut file = if resuming {
        OpenOptions::new().append(true).open(partial)
    } else {
        File::create(partial)
    }
    .map_err(|e| format!("could not open the download file: {e}"))?;

    let mut reader = response.into_reader();
    let mut buffer = vec![0u8; 128 * 1024];
    let mut last_reported = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| format!("the download was interrupted: {e}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|e| format!("could not write the download: {e}"))?;
        written += read as u64;

        // Repaint about every 512 KiB: often enough to feel live, rare enough
        // not to flood the UI thread.
        if written - last_reported >= 512 * 1024 {
            last_reported = written;
            let done = written as f32 / 1_048_576.0;
            let fraction = if artifact.bytes > 0 {
                (written as f32 / artifact.bytes as f32) * 0.85
            } else {
                -1.0
            };
            let message = if artifact.bytes > 0 {
                let total = artifact.bytes as f32 / 1_048_576.0;
                let percent = (written as f64 / artifact.bytes as f64 * 100.0).min(100.0);
                format!("Downloading the harness runtime… {done:.0} of {total:.0} MB ({percent:.0}%)")
            } else {
                format!("Downloading the harness runtime… {done:.0} MB")
            };
            on_progress(fraction, &message);
        }
    }
    file.flush()
        .map_err(|e| format!("could not finish writing the download: {e}"))?;

    if artifact.bytes > 0 && written < artifact.bytes {
        return Err(format!(
            "the connection ended early ({written} of {} bytes) — it will resume",
            artifact.bytes
        ));
    }
    Ok(())
}

/// Download the artifact and prove it matches the digest baked into the app.
///
/// Retries, and resumes where the previous attempt stopped. A checksum failure
/// discards the partial rather than resuming into the same bad bytes.
fn download(
    partial: &Path,
    artifact: &Artifact,
    on_progress: &dyn Fn(f32, &str),
) -> Result<PathBuf, String> {
    let mut last = String::new();

    for attempt in 1..=DOWNLOAD_ATTEMPTS {
        if attempt > 1 {
            // 1s, 2s, 4s.
            let wait = Duration::from_secs(1 << (attempt - 2));
            on_progress(
                -1.0,
                &format!("Retrying download (attempt {attempt} of {DOWNLOAD_ATTEMPTS})…"),
            );
            std::thread::sleep(wait);
        }

        if let Err(message) = transfer(partial, artifact, on_progress) {
            last = message;
            continue;
        }

        // A resumed transfer is only trustworthy if the whole file hashes right.
        match digest_of(partial) {
            Ok(actual) if actual.eq_ignore_ascii_case(&artifact.sha256) => {
                return Ok(partial.to_path_buf());
            }
            Ok(actual) => {
                let _ = fs::remove_file(partial);
                last = format!(
                    "the download did not match its checksum\n  expected {}\n  actual   {}",
                    artifact.sha256, actual
                );
            }
            Err(message) => last = message,
        }
    }

    Err(format!(
        "{last}\n\nGave up after {DOWNLOAD_ATTEMPTS} attempts."
    ))
}

/// Download, verify, and install the runtime if it is not already present.
///
/// `on_progress` receives an overall fraction in `0.0..=1.0` (negative when the
/// total is unknown) plus a short status line.
pub fn ensure_runtime(
    app_data_dir: &Path,
    on_progress: &dyn Fn(f32, &str),
) -> Result<PathBuf, String> {
    let manifest = load_manifest()?;
    let target = host_target();
    let artifact = manifest
        .artifacts
        .get(target)
        .ok_or_else(|| format!("no runtime artifact for target {target}"))?;

    let root = runtimes_root(app_data_dir);
    fs::create_dir_all(&root).map_err(|e| format!("could not create {}: {e}", root.display()))?;
    let runtime_dir = root.join(&manifest.runtime_version);

    if is_installed(&runtime_dir) {
        // Already installed: report no fraction so the progress bar stays
        // hidden. Saying "downloading" here was wrong and alarming.
        eprintln!(
            "[shell] runtime {} already installed at {}",
            manifest.runtime_version,
            runtime_dir.display()
        );
        on_progress(-1.0, "Starting harness…");
        return Ok(runtime_dir);
    }

    eprintln!(
        "[shell] acquiring runtime dsh {} (node {}) for {target}",
        manifest.runtime_version, manifest.node_version
    );
    eprintln!("[shell]   artifact: {}", artifact.name);

    clean_staging(&root);
    let staging = root.join(format!(".staging-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)
        .map_err(|e| format!("could not create staging dir: {e}"))?;

    // --- download + verify --------------------------------------------------
    // The partial file lives outside staging, keyed by digest, so an interrupted
    // transfer resumes on a later attempt *and* after the app is relaunched.
    // Keying it by digest means a changed payload never resumes the wrong bytes.
    let partial = root.join(format!(".download-{}.part", &artifact.sha256[..16]));
    forget_other_partials(&root, &partial);
    on_progress(0.0, "Downloading the harness runtime…");
    let archive = download(&partial, artifact, on_progress)?;

    // --- extract ------------------------------------------------------------
    on_progress(0.87, "Extracting runtime…");
    let archive_file = File::open(&archive).map_err(|e| format!("could not reopen archive: {e}"))?;
    let decoder = flate2::read::GzDecoder::new(archive_file);
    let mut tar = tar::Archive::new(decoder);
    // `unpack` rejects entries whose paths escape the destination.
    tar.unpack(&staging)
        .map_err(|e| format!("extraction failed: {e}"))?;
    let _ = fs::remove_file(&archive);

    let staged_runtime = staging.join("runtime");
    if !staged_runtime.join("node").exists() {
        let _ = fs::remove_dir_all(&staging);
        return Err("payload did not contain runtime/node".to_owned());
    }

    on_progress(0.97, "Finalizing…");
    // Marker written last: its presence means the tree is complete and verified.
    fs::write(installed_marker(&staging), &artifact.sha256)
        .map_err(|e| format!("could not write install marker: {e}"))?;

    // --- atomic publish -----------------------------------------------------
    match fs::rename(&staging, &runtime_dir) {
        Ok(()) => {}
        Err(_) if is_installed(&runtime_dir) => {
            // Another launch won the race; keep theirs.
            let _ = fs::remove_dir_all(&staging);
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(format!("could not install runtime: {error}"));
        }
    }

    on_progress(1.0, "Runtime ready");
    Ok(runtime_dir)
}
