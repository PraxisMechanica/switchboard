//! Git-backed implementation of [`DirectoryPort`] storing `peers/<handle>.json`.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;

use tokio::process::Command;
use tokio::sync::Mutex;

use crate::identity::{self, Signed};

use super::card::PeerCard;
use super::{DirectoryError, DirectoryPort};

/// Team phone book synchronized via a shared Git repository.
#[derive(Debug, Clone)]
pub struct GitPhonebook {
    remote_url: String,
    local_dir: PathBuf,
    op_lock: Arc<Mutex<()>>,
}

impl GitPhonebook {
    pub fn new(remote_url: impl Into<String>, local_dir: impl Into<PathBuf>) -> Self {
        Self {
            remote_url: remote_url.into(),
            local_dir: local_dir.into(),
            op_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn remote_url(&self) -> &str {
        &self.remote_url
    }

    pub fn local_dir(&self) -> &Path {
        &self.local_dir
    }

    /// Clones the remote repository if absent, otherwise fetches and resets to the default branch.
    pub async fn ensure_repo(&self) -> Result<(), DirectoryError> {
        let _guard = self.op_lock.lock().await;
        self.ensure_repo_locked().await
    }

    async fn ensure_repo_locked(&self) -> Result<(), DirectoryError> {
        let git_dir = self.local_dir.join(".git");
        if !tokio::fs::try_exists(&git_dir).await? {
            if let Some(parent) = self.local_dir.parent() {
                if !parent.as_os_str().is_empty() {
                    tokio::fs::create_dir_all(parent).await?;
                }
            }
            let local_str = self.local_dir.to_string_lossy().to_string();
            run_git_checked(None, &["clone", "--", &self.remote_url, &local_str]).await?;
            return Ok(());
        }

        run_git_checked(Some(&self.local_dir), &["fetch", "origin"]).await?;

        let Some(remote_ref) = resolve_remote_default_ref(&self.local_dir).await? else {
            return Ok(());
        };
        let branch = remote_ref
            .strip_prefix("refs/remotes/origin/")
            .unwrap_or("main");

        run_git_checked(
            Some(&self.local_dir),
            &["checkout", "-f", "-B", branch, &remote_ref],
        )
        .await?;

        Ok(())
    }
}

#[async_trait::async_trait]
impl DirectoryPort for GitPhonebook {
    async fn publish(&self, card: Signed<PeerCard>) -> Result<(), DirectoryError> {
        let handle = identity::validate_handle(&card.payload.handle)?;
        let _guard = self.op_lock.lock().await;
        self.ensure_repo_locked().await?;

        let peers_dir = self.local_dir.join("peers");
        tokio::fs::create_dir_all(&peers_dir).await?;

        let rel_path = format!("peers/{handle}.json");
        let card_path = self.local_dir.join(&rel_path);
        let serialized = serde_json::to_string_pretty(&card)?;
        tokio::fs::write(&card_path, format!("{serialized}\n")).await?;

        run_git_checked(Some(&self.local_dir), &["add", "--", &rel_path]).await?;

        let status = run_git_checked(
            Some(&self.local_dir),
            &["status", "--porcelain", "--", &rel_path],
        )
        .await?;
        if status.trim().is_empty() {
            return Ok(());
        }

        let commit_msg = format!("publish {handle}");
        run_git_checked(
            Some(&self.local_dir),
            &[
                "-c",
                "user.name=switchboard",
                "-c",
                "user.email=switchboard@local",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                &commit_msg,
            ],
        )
        .await?;

        run_git_checked(Some(&self.local_dir), &["push", "origin", "HEAD"]).await?;

        Ok(())
    }

    async fn fetch_all(&self) -> Result<Vec<Signed<PeerCard>>, DirectoryError> {
        let _guard = self.op_lock.lock().await;
        self.ensure_repo_locked().await?;

        let peers_dir = self.local_dir.join("peers");
        if !tokio::fs::try_exists(&peers_dir).await? {
            return Ok(Vec::new());
        }

        let mut entries = tokio::fs::read_dir(&peers_dir).await?;
        let mut json_paths = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_type = entry.file_type().await?;
            if !file_type.is_file() {
                tracing::warn!(path = %path.display(), "skipping non-file entry in peers directory");
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                tracing::warn!(path = %path.display(), "skipping non-json file in peers directory");
                continue;
            }
            json_paths.push(path);
        }

        json_paths.sort();

        let mut cards = Vec::with_capacity(json_paths.len());
        for path in json_paths {
            let contents = tokio::fs::read_to_string(&path).await?;
            let card: Signed<PeerCard> = serde_json::from_str(&contents)?;
            cards.push(card);
        }

        Ok(cards)
    }
}

async fn resolve_remote_default_ref(repo_dir: &Path) -> Result<Option<String>, DirectoryError> {
    if let Some(head_ref) = read_origin_head(repo_dir).await? {
        return Ok(Some(head_ref));
    }

    let _ = run_git(Some(repo_dir), &["remote", "set-head", "origin", "--auto"]).await?;
    if let Some(head_ref) = read_origin_head(repo_dir).await? {
        return Ok(Some(head_ref));
    }

    let refs_raw = run_git_checked(
        Some(repo_dir),
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/remotes/origin/",
        ],
    )
    .await?;

    let refs: Vec<&str> = refs_raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "refs/remotes/origin/HEAD")
        .collect();

    if refs.is_empty() {
        return Ok(None);
    }

    if refs.contains(&"refs/remotes/origin/main") {
        return Ok(Some("refs/remotes/origin/main".to_string()));
    }
    if refs.contains(&"refs/remotes/origin/master") {
        return Ok(Some("refs/remotes/origin/master".to_string()));
    }

    Ok(refs.first().map(|r| (*r).to_string()))
}

async fn read_origin_head(repo_dir: &Path) -> Result<Option<String>, DirectoryError> {
    let output = run_git(
        Some(repo_dir),
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )
    .await?;

    if !output.status.success() {
        return Ok(None);
    }

    let trimmed = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let verify = run_git(
        Some(repo_dir),
        &["rev-parse", "--verify", "--quiet", &trimmed],
    )
    .await?;
    if verify.status.success() {
        Ok(Some(trimmed))
    } else {
        Ok(None)
    }
}

async fn run_git(cwd: Option<&Path>, args: &[&str]) -> Result<Output, DirectoryError> {
    let mut cmd = Command::new("git");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.args(args);

    cmd.output().await.map_err(|err| DirectoryError::Git {
        command: format!("git {}", args.join(" ")),
        details: err.to_string(),
    })
}

async fn run_git_checked(cwd: Option<&Path>, args: &[&str]) -> Result<String, DirectoryError> {
    let output = run_git(cwd, args).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let details = if stderr.is_empty() { stdout } else { stderr };
        return Err(DirectoryError::Git {
            command: format!("git {}", args.join(" ")),
            details,
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
