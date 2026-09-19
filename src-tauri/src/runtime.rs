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
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

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
        on_progress(1.0, "Runtime ready");
        return Ok(runtime_dir);
    }

    eprintln!(
        "[shell] acquiring runtime dsh {} (node {}) for {target}",
        manifest.runtime_version, manifest.node_version
    );

    clean_staging(&root);
    let staging = root.join(format!(".staging-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)
        .map_err(|e| format!("could not create staging dir: {e}"))?;

    let archive = staging.join(&artifact.name);

    // --- download + hash ----------------------------------------------------
    on_progress(0.0, "Downloading runtime…");
    let response = ureq::get(&artifact.url)
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let total_bytes = response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);

    let mut reader = response.into_reader();
    let mut file = File::create(&archive).map_err(|e| format!("could not create archive: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 128 * 1024];
    let mut written = 0u64;
    let mut last_reported = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| format!("download interrupted: {e}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|e| format!("could not write archive: {e}"))?;
        written += read as u64;
        // Only repaint every ~4 MiB to keep the UI thread quiet.
        if written - last_reported >= 4 * 1024 * 1024 {
            last_reported = written;
            let fraction = if total_bytes > 0 {
                (written as f32 / total_bytes as f32) * 0.85
            } else {
                -1.0
            };
            let mb = written as f32 / 1_048_576.0;
            on_progress(fraction, &format!("Downloading runtime… {mb:.0} MB"));
        }
    }
    drop(file);

    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(&artifact.sha256) {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!(
            "runtime checksum mismatch\n  expected {}\n  actual   {}",
            artifact.sha256, actual
        ));
    }

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
