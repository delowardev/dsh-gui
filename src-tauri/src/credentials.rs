//! Collecting the user's API key during first run.
//!
//! The download leaves roughly eighty seconds of dead time, and the key is the
//! one thing the user cannot finish without — so it is collected there rather
//! than after the harness is up.
//!
//! The harness home does not exist yet at that point, and the harness is not
//! running, so its own settings API is unavailable. This writes the credentials
//! document directly. The format is a flat `CredentialRef: value` mapping, which
//! is why a line-based merge is safe; comments and other entries survive.
//!
//! The key is never logged, and the file is written `0600`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

/// The credential name the DeepSeek plugin resolves.
const API_KEY_REF: &str = "DEEPSEEK_API_KEY";

/// Cheapest authenticated endpoint, used only to tell a good key from a bad one.
const MODELS_URL: &str = "https://api.deepseek.com/models";

fn credentials_path(home: &Path) -> std::path::PathBuf {
    home.join(".credentials.yaml")
}

/// Whether a key is already configured in this harness home.
pub fn has_api_key(home: &Path) -> bool {
    let Ok(text) = fs::read_to_string(credentials_path(home)) else {
        return false;
    };
    text.lines().any(|line| {
        line.trim()
            .strip_prefix(API_KEY_REF)
            .and_then(|rest| rest.trim_start().strip_prefix(':'))
            .is_some_and(|value| !value.trim().is_empty())
    })
}

/// The outcome of checking a key, kept separate from saving it so a network
/// problem is never mistaken for a bad key.
pub enum Verdict {
    Valid,
    Rejected,
    Unchecked(String),
}

/// Ask DeepSeek whether the key works.
pub fn verify(key: &str) -> Verdict {
    // Without a deadline a black-holed connection would leave the button
    // spinning for the life of the process. The key can be saved unchecked.
    let request = ureq::get(MODELS_URL)
        .timeout(Duration::from_secs(15))
        .set("Authorization", &format!("Bearer {key}"));
    match request.call() {
        Ok(_) => Verdict::Valid,
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => Verdict::Rejected,
        Err(ureq::Error::Status(code, _)) => {
            Verdict::Unchecked(format!("the server returned HTTP {code}"))
        }
        Err(ureq::Error::Transport(transport)) => {
            Verdict::Unchecked(format!("could not reach DeepSeek ({transport})"))
        }
    }
}

/// Write the key into the credentials document, preserving everything else.
pub fn save(home: &Path, key: &str) -> Result<(), String> {
    let path = credentials_path(home);
    fs::create_dir_all(home).map_err(|e| format!("could not create the harness home: {e}"))?;

    let existing = fs::read_to_string(&path).unwrap_or_default();
    let mut kept: Vec<&str> = existing
        .lines()
        .filter(|line| {
            !line
                .trim()
                .strip_prefix(API_KEY_REF)
                .is_some_and(|rest| rest.trim_start().starts_with(':'))
        })
        .collect();
    while kept.last().is_some_and(|line| line.trim().is_empty()) {
        kept.pop();
    }

    let mut document = kept.join("\n");
    if !document.is_empty() {
        document.push('\n');
    }
    document.push_str(&format!("{API_KEY_REF}: {key}\n"));

    // Created 0600: this file is a secret, and the harness's own writer applies
    // the same mode.
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("could not write the credentials file: {e}"))?;
    file.write_all(document.as_bytes())
        .map_err(|e| format!("could not write the credentials file: {e}"))?;
    // `mode` only applies at creation; tighten an existing file too.
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dsh-cred-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn saves_and_reads_back() {
        let home = scratch("save");
        assert!(!has_api_key(&home), "a fresh home has no key");
        save(&home, "sk-test-123").unwrap();
        assert!(has_api_key(&home));
        let text = fs::read_to_string(credentials_path(&home)).unwrap();
        assert!(text.contains("DEEPSEEK_API_KEY: sk-test-123"), "got: {text}");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn an_empty_value_is_not_a_key() {
        let home = scratch("empty");
        fs::write(credentials_path(&home), "DEEPSEEK_API_KEY:\n").unwrap();
        assert!(!has_api_key(&home));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn the_file_is_private() {
        let home = scratch("mode");
        save(&home, "sk-test").unwrap();
        let mode = fs::metadata(credentials_path(&home))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "credentials must not be readable by others");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn other_credentials_and_comments_survive() {
        let home = scratch("merge");
        fs::write(
            credentials_path(&home),
            "# keep me\nOTHER_KEY: other\nDEEPSEEK_API_KEY: sk-old\n",
        )
        .unwrap();
        save(&home, "sk-new").unwrap();
        let text = fs::read_to_string(credentials_path(&home)).unwrap();
        assert!(text.contains("# keep me"), "comments must survive: {text}");
        assert!(text.contains("OTHER_KEY: other"), "other entries must survive: {text}");
        assert!(text.contains("DEEPSEEK_API_KEY: sk-new"), "got: {text}");
        assert!(!text.contains("sk-old"), "the previous key must be replaced: {text}");
        let _ = fs::remove_dir_all(&home);
    }

    /// Talks to the real API, so it is opt-in:
    ///   DSH_TEST_API_KEY=sk-… cargo test -- --ignored --nocapture live_verify
    ///
    /// Worth running whenever the verdicts change: treating a good key as
    /// rejected is the one failure the user cannot work around.
    #[test]
    #[ignore = "requires network access and a real key"]
    fn live_verify_accepts_a_good_key() {
        let key = std::env::var("DSH_TEST_API_KEY").expect("set DSH_TEST_API_KEY");
        match verify(&key) {
            Verdict::Valid => {}
            Verdict::Rejected => panic!("a working key was reported as rejected"),
            Verdict::Unchecked(reason) => panic!("could not reach DeepSeek: {reason}"),
        }
    }

    #[test]
    fn a_bogus_key_is_rejected() {
        if std::env::var("DSH_TEST_API_KEY").is_err() {
            return; // offline unit run; the live test covers the real call
        }
        match verify("sk-definitely-not-a-real-key") {
            Verdict::Rejected => {}
            Verdict::Valid => panic!("the API accepted a bogus key"),
            Verdict::Unchecked(reason) => println!("skipped, network unavailable: {reason}"),
        }
    }
}
