//! Antigravity (`agy`) CLI headless harness adapter and multi-harness router.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::Value;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::agy_bridge::{
    default_agy_app_brain_root, default_agy_app_conversations_dir, default_agy_bridge_socket,
    deliver_via_app_bridge, is_app_conversation,
};
use super::codex::CodexHarness;
use super::model::{CodexSessionInfo, DeliveryOutcome, DeliveryRequest, HarnessKind};
use super::{AgentHarness, ExtensionError};

/// Process and timeout configuration for [`AgyHarness`].
#[derive(Debug, Clone)]
pub struct AgyConfig {
    pub binary: PathBuf,
    pub turn_timeout: Duration,
    pub brain_dir: Option<PathBuf>,
}

impl Default for AgyConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("agy"),
            turn_timeout: Duration::from_secs(180),
            brain_dir: None,
        }
    }
}

/// Resolves the `agy` binary on `PATH` or `~/.local/bin/agy`.
pub fn resolve_agy_binary(configured: &Path) -> PathBuf {
    if configured != Path::new("agy") {
        return configured.to_path_buf();
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("agy");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let local_bin = PathBuf::from(home).join(".local").join("bin").join("agy");
        if local_bin.is_file() {
            return local_bin;
        }
    }
    configured.to_path_buf()
}

/// [`AgentHarness`] delivering plain-text messages to pre-existing Antigravity conversations
/// via `agy --conversation <id> --mode plan --output-format json -p <prompt>`.
#[derive(Debug, Clone, Default)]
pub struct AgyHarness {
    config: AgyConfig,
}

impl AgyHarness {
    pub fn new(config: AgyConfig) -> Self {
        Self { config }
    }
}

#[async_trait::async_trait]
impl AgentHarness for AgyHarness {
    async fn deliver(
        &self,
        req: DeliveryRequest,
        cancel: CancellationToken,
    ) -> Result<DeliveryOutcome, ExtensionError> {
        if cancel.is_cancelled() {
            return Ok(DeliveryOutcome::Cancelled);
        }
        if req.session_id.trim().is_empty() {
            return Err(ExtensionError::SessionRequired(req.extension.to_string()));
        }

        let prompt = format_agy_prompt(&req);
        if let Some(app_convs) = default_agy_app_conversations_dir() {
            if is_app_conversation(&app_convs, &req.session_id) {
                let brain_root = default_agy_app_brain_root().ok_or_else(|| {
                    ExtensionError::Protocol("HOME is not set for Antigravity App brain".into())
                })?;
                return deliver_via_app_bridge(
                    &default_agy_bridge_socket(),
                    &brain_root,
                    &req,
                    &prompt,
                    self.config.turn_timeout,
                    cancel,
                )
                .await;
            }
        }

        let binary = resolve_agy_binary(&self.config.binary);
        let timeout_arg = format!("{}s", self.config.turn_timeout.as_secs().max(1));

        let child = Command::new(&binary)
            .args([
                "--conversation",
                &req.session_id,
                "--mode",
                "plan",
                "--output-format",
                "json",
                "--print-timeout",
                &timeout_arg,
                "-p",
                &prompt,
            ])
            .current_dir(&req.workspace_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| {
                ExtensionError::Spawn(format!("binary '{}': {err}", binary.display()))
            })?;

        let output = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(DeliveryOutcome::Cancelled),
            () = tokio::time::sleep(self.config.turn_timeout) => return Ok(DeliveryOutcome::TimedOut),
            res = child.wait_with_output() => res?,
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let detail = if stderr.is_empty() {
                "agy exited with non-zero status".to_string()
            } else {
                stderr
            };
            return Ok(DeliveryOutcome::ProcessFailed {
                exit_code: output.status.code(),
                detail,
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let body = parse_agy_json_reply(&stdout);
        Ok(DeliveryOutcome::Replied {
            body,
            session_id: Some(req.session_id),
        })
    }

    async fn list_sessions(&self, limit: usize) -> Result<Vec<CodexSessionInfo>, ExtensionError> {
        let mut sessions = Vec::new();
        if let Some(dir) = self.config.brain_dir.clone() {
            sessions.extend(discover_agy_brain_sessions(
                &dir,
                limit,
                "Antigravity CLI conversation",
            ));
        } else {
            if let Some(app_dir) = default_agy_app_conversations_dir() {
                sessions.extend(discover_agy_brain_sessions(
                    &app_dir,
                    limit,
                    "Antigravity App conversation",
                ));
            }
            if let Some(cli_dir) = default_agy_brain_dir() {
                sessions.extend(discover_agy_brain_sessions(
                    &cli_dir,
                    limit,
                    "Antigravity CLI conversation",
                ));
            }
        }
        sessions.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
        let mut seen = std::collections::HashSet::new();
        sessions.retain(|s| seen.insert(s.id.clone()));
        sessions.truncate(limit);
        Ok(sessions)
    }
}

/// Composite harness dispatching each call to [`CodexHarness`] or [`AgyHarness`] by [`HarnessKind`].
#[derive(Debug, Clone, Default)]
pub struct MultiHarness {
    codex: CodexHarness,
    agy: AgyHarness,
}

impl MultiHarness {
    pub fn new(codex: CodexHarness, agy: AgyHarness) -> Self {
        Self { codex, agy }
    }
}

#[async_trait::async_trait]
impl AgentHarness for MultiHarness {
    async fn deliver(
        &self,
        req: DeliveryRequest,
        cancel: CancellationToken,
    ) -> Result<DeliveryOutcome, ExtensionError> {
        match req.harness {
            HarnessKind::Codex => self.codex.deliver(req, cancel).await,
            HarnessKind::Agy => self.agy.deliver(req, cancel).await,
        }
    }

    async fn list_sessions(&self, limit: usize) -> Result<Vec<CodexSessionInfo>, ExtensionError> {
        let mut sessions = self.codex.list_sessions(limit).await.unwrap_or_default();
        if let Ok(agy_sessions) = self.agy.list_sessions(limit).await {
            sessions.extend(agy_sessions);
        }
        sessions.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
        sessions.truncate(limit);
        Ok(sessions)
    }
}

fn default_agy_brain_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join(".gemini")
            .join("antigravity-cli")
            .join("conversations")
    })
}

pub(crate) fn format_agy_prompt(req: &DeliveryRequest) -> String {
    format!(
        "Incoming switchboard message from peer @{from} (call: {call_id}). Reply in plain text only; do not edit files or run destructive commands.\n\n{body}",
        from = req.from_handle,
        call_id = req.call_id,
        body = req.body,
    )
}

pub(crate) fn parse_agy_json_reply(stdout: &str) -> String {
    for line in stdout.lines().rev() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
            if let Some(resp) = val.get("response").and_then(Value::as_str) {
                return resp.trim().to_string();
            }
        }
    }
    stdout.trim().to_string()
}

pub(crate) fn discover_agy_brain_sessions(
    conversations_dir: &Path,
    limit: usize,
    label: &str,
) -> Vec<CodexSessionInfo> {
    let Ok(entries) = std::fs::read_dir(conversations_dir) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let Some(raw_name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        let Some(stem) = raw_name.strip_suffix(".db") else {
            continue;
        };
        if Uuid::parse_str(stem).is_err() {
            continue;
        }
        let updated_at = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        sessions.push(CodexSessionInfo {
            id: stem.to_string(),
            harness: HarnessKind::Agy,
            cwd: String::new(),
            title: label.to_string(),
            updated_at,
        });
    }
    sessions.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
    sessions.truncate(limit);
    sessions
}

#[cfg(test)]
#[path = "agy_tests.rs"]
mod tests;
