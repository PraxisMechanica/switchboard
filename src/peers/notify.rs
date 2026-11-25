//! Human approval notification port and built-in adapters.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::process::Command;

use crate::identity::PeerId;

use super::state::PeerHandle;

const DIALOG_TIMEOUT: Duration = Duration::from_secs(58);

/// Result of prompting the local human operator via an interactive OS dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPromptOutcome {
    Approve,
    Reject,
    Pending,
}

/// Notification sink invoked whenever a new inbound dial request requires human approval.
#[async_trait::async_trait]
pub trait ApprovalNotifier: Send + Sync {
    async fn notify_inbound_knock(&self, peer_id: PeerId, handle: &PeerHandle, reason: &str);

    /// Optionally prompts the human operator interactively in the desktop UI.
    async fn prompt_inbound_approval(
        &self,
        _peer_id: PeerId,
        _handle: &PeerHandle,
        _reason: &str,
    ) -> ApprovalPromptOutcome {
        ApprovalPromptOutcome::Pending
    }
}

/// Production notifier emitting a concise structured log entry via `tracing`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingNotifier;

#[async_trait::async_trait]
impl ApprovalNotifier for TracingNotifier {
    async fn notify_inbound_knock(&self, peer_id: PeerId, handle: &PeerHandle, reason: &str) {
        tracing::info!(
            peer_id = %peer_id.short(),
            handle = %handle,
            reason = %reason,
            "inbound dial request awaiting human approval"
        );
    }
}

/// Desktop notifier showing a native OS Approve/Reject dialog (macOS `osascript` / Linux `zenity`)
/// unless `SWITCHBOARD_HEADLESS=1` is set.
#[derive(Debug, Default, Clone, Copy)]
pub struct DesktopNotifier;

#[async_trait::async_trait]
impl ApprovalNotifier for DesktopNotifier {
    async fn notify_inbound_knock(&self, peer_id: PeerId, handle: &PeerHandle, reason: &str) {
        tracing::info!(
            peer_id = %peer_id.short(),
            handle = %handle,
            reason = %reason,
            "inbound dial request received; opening desktop approval prompt"
        );
    }

    async fn prompt_inbound_approval(
        &self,
        peer_id: PeerId,
        handle: &PeerHandle,
        reason: &str,
    ) -> ApprovalPromptOutcome {
        if std::env::var("SWITCHBOARD_HEADLESS").is_ok_and(|v| v == "1" || v == "true") {
            return ApprovalPromptOutcome::Pending;
        }
        prompt_native_dialog(peer_id, handle, reason).await
    }
}

#[cfg(target_os = "macos")]
async fn prompt_native_dialog(
    peer_id: PeerId,
    handle: &PeerHandle,
    reason: &str,
) -> ApprovalPromptOutcome {
    let body = format!(
        "@{handle} is dialing your Switchboard.\n\nReason: {reason}\nPeer ID: {}\n\nAllow direct text-only agent messages?",
        peer_id.short()
    );
    let script = format!(
        "display dialog \"{}\" with title \"Switchboard\" buttons {{\"Later\", \"Reject\", \"Approve\"}} default button \"Approve\" giving up after 55",
        escape_applescript_literal(&body)
    );

    let child = match Command::new("osascript")
        .args(["-e", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(error = %err, "failed to launch macOS approval dialog");
            return ApprovalPromptOutcome::Pending;
        }
    };

    match tokio::time::timeout(DIALOG_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => {
            parse_osascript_outcome(&String::from_utf8_lossy(&output.stdout))
        }
        _ => ApprovalPromptOutcome::Pending,
    }
}

#[cfg(not(target_os = "macos"))]
async fn prompt_native_dialog(
    peer_id: PeerId,
    handle: &PeerHandle,
    reason: &str,
) -> ApprovalPromptOutcome {
    let text = format!(
        "@{handle} ({}) is dialing your Switchboard.\nReason: {reason}\n\nAllow direct text-only agent messages?",
        peer_id.short()
    );
    let child = match Command::new("zenity")
        .args([
            "--question",
            "--title=Switchboard",
            &format!("--text={text}"),
            "--ok-label=Approve",
            "--cancel-label=Later",
            "--timeout=55",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return ApprovalPromptOutcome::Pending,
    };

    match tokio::time::timeout(DIALOG_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) if status.success() => ApprovalPromptOutcome::Approve,
        _ => ApprovalPromptOutcome::Pending,
    }
}

pub(crate) fn escape_applescript_literal(raw: &str) -> String {
    raw.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "")
        .replace('\n', "\\n")
}

pub(crate) fn parse_osascript_outcome(stdout: &str) -> ApprovalPromptOutcome {
    if stdout.contains("gave up:true") {
        return ApprovalPromptOutcome::Pending;
    }
    if stdout.contains("button returned:Approve") {
        ApprovalPromptOutcome::Approve
    } else if stdout.contains("button returned:Reject") {
        ApprovalPromptOutcome::Reject
    } else {
        ApprovalPromptOutcome::Pending
    }
}

/// Captured inbound knock notification stored by [`RecordingNotifier`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundKnockNotice {
    pub peer_id: PeerId,
    pub handle: PeerHandle,
    pub reason: String,
}

/// Deterministic in-memory notifier for unit and integration tests.
#[derive(Debug, Default, Clone)]
pub struct RecordingNotifier {
    notices: Arc<Mutex<Vec<InboundKnockNotice>>>,
}

impl RecordingNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn recorded(&self) -> Vec<InboundKnockNotice> {
        self.notices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn len(&self) -> usize {
        self.notices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait::async_trait]
impl ApprovalNotifier for RecordingNotifier {
    async fn notify_inbound_knock(&self, peer_id: PeerId, handle: &PeerHandle, reason: &str) {
        let mut guard = self
            .notices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.push(InboundKnockNotice {
            peer_id,
            handle: handle.clone(),
            reason: reason.to_string(),
        });
    }
}

#[cfg(test)]
#[path = "notify_tests.rs"]
mod tests;
