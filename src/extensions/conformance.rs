//! Deterministic in-memory stub harness and shared [`AgentHarness`] conformance suite.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::model::{CodexSessionInfo, DeliveryOutcome, DeliveryRequest, ExtensionName};
use super::{AgentHarness, ExtensionError};

/// Prompt marker used by [`verify_harness`] to exercise mid-turn cancellation.
pub const CONFORMANCE_SLOW_MARKER: &str = "CONFORMANCE_SLOW";

#[derive(Debug, Default)]
struct StubState {
    scripted_replies: VecDeque<String>,
    scripted_sessions: Vec<CodexSessionInfo>,
    delay: Duration,
    requests: Vec<DeliveryRequest>,
    sessions: Vec<String>,
}

/// Deterministic in-memory [`AgentHarness`] for unit tests and conformance verification.
#[derive(Debug, Clone, Default)]
pub struct StubHarness {
    state: Arc<Mutex<StubState>>,
}

impl StubHarness {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_replies<I, S>(replies: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let scripted_replies = replies.into_iter().map(Into::into).collect();
        Self {
            state: Arc::new(Mutex::new(StubState {
                scripted_replies,
                ..StubState::default()
            })),
        }
    }

    pub fn with_delay(self, delay: Duration) -> Self {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.delay = delay;
        drop(guard);
        self
    }

    pub fn with_sessions(self, sessions: Vec<CodexSessionInfo>) -> Self {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.scripted_sessions = sessions;
        drop(guard);
        self
    }

    pub fn push_reply(&self, reply: impl Into<String>) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.scripted_replies.push_back(reply.into());
    }

    pub fn recorded_requests(&self) -> Vec<DeliveryRequest> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.requests.clone()
    }

    pub fn seen_sessions(&self) -> Vec<String> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.sessions.clone()
    }
}

#[async_trait::async_trait]
impl AgentHarness for StubHarness {
    async fn deliver(
        &self,
        req: DeliveryRequest,
        cancel: CancellationToken,
    ) -> Result<DeliveryOutcome, ExtensionError> {
        if cancel.is_cancelled() {
            return Ok(DeliveryOutcome::Cancelled);
        }

        let configured_delay = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .delay;

        let effective_delay = if req.body.contains(CONFORMANCE_SLOW_MARKER) {
            configured_delay.max(Duration::from_secs(5))
        } else {
            configured_delay
        };

        if !effective_delay.is_zero() {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(DeliveryOutcome::Cancelled),
                () = tokio::time::sleep(effective_delay) => {}
            }
        }

        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let session_id = req.session_id.clone();

        guard.sessions.push(session_id.clone());
        let reply = guard
            .scripted_replies
            .pop_front()
            .unwrap_or_else(|| format!("stub-reply[{session_id}]: {}", req.body));
        guard.requests.push(req);

        Ok(DeliveryOutcome::Replied {
            body: reply,
            session_id: Some(session_id),
        })
    }

    async fn list_sessions(&self, limit: usize) -> Result<Vec<CodexSessionInfo>, ExtensionError> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(guard
            .scripted_sessions
            .iter()
            .take(limit)
            .cloned()
            .collect())
    }
}

/// Verifies that any [`AgentHarness`] resumes a pre-existing session across turns and honors cancellation.
pub async fn verify_harness<H: AgentHarness>(harness: &H, workspace: &Path) {
    let ext = ExtensionName::new("conformance").expect("valid static extension name");
    let pre_existing_session = "019a20e4-80cc-7754-b584-33393adcc1f5".to_string();

    // (a) First delivery resumes the pre-existing session and returns Replied.
    let first = harness
        .deliver(
            DeliveryRequest {
                call_id: Uuid::now_v7(),
                extension: ext.clone(),
                workspace_dir: workspace.to_path_buf(),
                from_handle: "alice".to_string(),
                body: "hello from conformance test".to_string(),
                session_id: pre_existing_session.clone(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("initial delivery must succeed");

    let session_id = match first {
        DeliveryOutcome::Replied { body, session_id } => {
            assert!(
                !body.trim().is_empty(),
                "initial reply body must be non-empty"
            );
            let sid = session_id.expect("initial reply must return Some(session_id)");
            assert_eq!(sid, pre_existing_session);
            sid
        }
        other => panic!("expected DeliveryOutcome::Replied, got {other:?}"),
    };

    // (b) Follow-up delivery continues that exact pre-existing session.
    let followup = harness
        .deliver(
            DeliveryRequest {
                call_id: Uuid::now_v7(),
                extension: ext.clone(),
                workspace_dir: workspace.to_path_buf(),
                from_handle: "alice".to_string(),
                body: "follow-up turn".to_string(),
                session_id: session_id.clone(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("follow-up delivery must succeed");

    match followup {
        DeliveryOutcome::Replied {
            body,
            session_id: resumed,
        } => {
            assert!(
                !body.trim().is_empty(),
                "follow-up reply body must be non-empty"
            );
            assert_eq!(
                resumed.as_deref(),
                Some(session_id.as_str()),
                "follow-up must resume exact session_id"
            );
        }
        other => panic!("expected DeliveryOutcome::Replied on resume, got {other:?}"),
    }

    // (c) Cancellation via CancellationToken returns DeliveryOutcome::Cancelled.
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        trigger.cancel();
    });

    let cancelled = harness
        .deliver(
            DeliveryRequest {
                call_id: Uuid::now_v7(),
                extension: ext,
                workspace_dir: workspace.to_path_buf(),
                from_handle: "alice".to_string(),
                body: format!("{CONFORMANCE_SLOW_MARKER}: hold turn for cancellation"),
                session_id,
            },
            cancel,
        )
        .await
        .expect("cancelled delivery must return Ok(Cancelled)");

    let _ = cancel_task.await;
    assert_eq!(
        cancelled,
        DeliveryOutcome::Cancelled,
        "expected DeliveryOutcome::Cancelled"
    );
}

#[cfg(test)]
#[path = "conformance_tests.rs"]
mod tests;
