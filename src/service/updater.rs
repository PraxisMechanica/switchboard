//! Lightweight SHA256 hash-heartbeat pull updater for `switchboard`.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::Command;
use uuid::Uuid;

use super::ServiceError;

pub const DEFAULT_RELEASE_TAG: &str = "v0.1.0";
pub const DEFAULT_UPDATE_BASE_URL: &str =
    "https://github.com/PraxisMechanica/switchboard/releases/download/v0.1.0";
pub const DEFAULT_HEARTBEAT_SECS: u64 = 600;

/// Configuration for the background SHA256 hash-heartbeat updater.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    #[serde(default = "default_base_url")]
    pub base_url: String,

    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            base_url: default_base_url(),
            interval_secs: default_interval_secs(),
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_base_url() -> String {
    DEFAULT_UPDATE_BASE_URL.to_string()
}

fn default_interval_secs() -> u64 {
    DEFAULT_HEARTBEAT_SECS
}

/// Outcome of a single hash-heartbeat update check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    UpToDate {
        hash: String,
    },
    Updated {
        previous: Option<String>,
        current: String,
    },
    Skipped(String),
}

/// Returns the release target triple for the compiled host OS/arch.
pub const fn current_target_triple() -> Option<&'static str> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Some("aarch64-apple-darwin")
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        Some("x86_64-apple-darwin")
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Some("x86_64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        Some("aarch64-unknown-linux-gnu")
    }
    #[cfg(not(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
    )))]
    {
        None
    }
}

/// Returns true when `exe_path` looks like a local `cargo` dev build (`.../target/debug/...`
/// or `.../target/release/...`) so background auto-updates never overwrite dev binaries.
pub fn is_dev_build_path(exe_path: &Path) -> bool {
    let text = exe_path.to_string_lossy();
    text.contains("/target/debug/") || text.contains("/target/release/")
}

/// Parses a `SHA256SUMS.txt` document and returns the 64-char hex digest for `archive_name`.
pub fn parse_sha256sums(contents: &str, archive_name: &str) -> Option<String> {
    for line in contents.lines() {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else { continue };
        let Some(name) = parts.next() else { continue };
        let clean_name = name.trim_start_matches('*');
        if clean_name == archive_name
            && hash.len() == 64
            && hash.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// Computes the lowercase hex SHA256 digest of a local file.
pub fn sha256_file(path: &Path) -> Result<String, ServiceError> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Fetches remote `SHA256SUMS.txt` (~120 bytes) and updates the binary atomically only when
/// the remote hash differs from the locally recorded installed archive hash (or `force` is true).
pub async fn check_and_apply_update(
    cfg: &UpdateConfig,
    data_dir: &Path,
    target_exe: &Path,
    force: bool,
) -> Result<UpdateOutcome, ServiceError> {
    let Some(target) = current_target_triple() else {
        return Ok(UpdateOutcome::Skipped("unsupported platform".into()));
    };
    let archive_name = format!("switchboard-{DEFAULT_RELEASE_TAG}-{target}.tar.gz");
    let base = cfg.base_url.trim_end_matches('/');
    let sums_url = format!("{base}/SHA256SUMS.txt");

    let sums_text = curl_fetch_text(&sums_url).await?;
    let Some(remote_hash) = parse_sha256sums(&sums_text, &archive_name) else {
        return Ok(UpdateOutcome::Skipped(format!(
            "no checksum entry for {archive_name}"
        )));
    };

    let state_file = data_dir.join("installed_archive.sha256");
    let local_hash = fs::read_to_string(&state_file)
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| s.len() == 64);

    if !force && local_hash.as_deref() == Some(remote_hash.as_str()) {
        return Ok(UpdateOutcome::UpToDate { hash: remote_hash });
    }

    let tmp_dir = std::env::temp_dir().join(format!("switchboard-update-{}", Uuid::now_v7()));
    fs::create_dir_all(&tmp_dir)?;
    let archive_path = tmp_dir.join(&archive_name);

    let result = async {
        let archive_url = format!("{base}/{archive_name}");
        curl_download_file(&archive_url, &archive_path).await?;

        let actual_hash = sha256_file(&archive_path)?;
        if actual_hash != remote_hash {
            return Err(ServiceError::InvalidPath(format!(
                "SHA256 mismatch for {archive_name}: expected {remote_hash}, got {actual_hash}"
            )));
        }

        extract_tar_gz(&archive_path, &tmp_dir).await?;
        let extracted_bin = tmp_dir.join("switchboard");
        if !extracted_bin.is_file() {
            return Err(ServiceError::InvalidPath(
                "downloaded archive missing switchboard binary".into(),
            ));
        }

        replace_binary_atomic(&extracted_bin, target_exe)?;
        install_bundled_skills(&tmp_dir);

        fs::create_dir_all(data_dir)?;
        fs::write(&state_file, format!("{remote_hash}\n"))?;
        Ok(UpdateOutcome::Updated {
            previous: local_hash,
            current: remote_hash,
        })
    }
    .await;

    let _ = fs::remove_dir_all(&tmp_dir);
    result
}

async fn curl_fetch_text(url: &str) -> Result<String, ServiceError> {
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", "10", url])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(ServiceError::InvalidPath(format!(
            "heartbeat fetch failed for {url}: {err}"
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn curl_download_file(url: &str, dest: &Path) -> Result<(), ServiceError> {
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", "60", url, "-o"])
        .arg(dest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(ServiceError::InvalidPath(format!(
            "archive download failed for {url}: {err}"
        )));
    }
    Ok(())
}

async fn extract_tar_gz(archive: &Path, dest_dir: &Path) -> Result<(), ServiceError> {
    let out = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dest_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(ServiceError::InvalidPath(format!(
            "tar extract failed: {err}"
        )));
    }
    Ok(())
}

fn replace_binary_atomic(new_bin: &Path, target_exe: &Path) -> Result<(), ServiceError> {
    let parent = target_exe
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| ServiceError::InvalidPath("target binary missing parent dir".into()))?;
    fs::create_dir_all(parent)?;

    let staged = parent.join(format!(".switchboard.update.{}", Uuid::now_v7()));
    fs::copy(new_bin, &staged)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
    }

    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(&staged)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    if let Err(err) = fs::rename(&staged, target_exe) {
        let _ = fs::remove_file(&staged);
        return Err(ServiceError::Io(err));
    }
    Ok(())
}

fn install_bundled_skills(extracted_dir: &Path) {
    let skill_src = extracted_dir.join("SKILL.md");
    if !skill_src.is_file() {
        return;
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    for rel in [
        ".codex/skills/switchboard-calling",
        ".gemini/antigravity/skills/switchboard-calling",
    ] {
        let dir = home.join(rel);
        if fs::create_dir_all(&dir).is_ok() {
            let _ = fs::copy(&skill_src, dir.join("SKILL.md"));
        }
    }
}

#[cfg(test)]
#[path = "updater_tests.rs"]
mod tests;
