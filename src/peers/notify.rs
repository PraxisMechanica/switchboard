//! Human approval notification port and built-in adapters.

use std::sync::{Arc, Mutex};

use crate::identity::PeerId;

use super::state::PeerHandle;

/// Notification sink invoked whenever a new inbound peering knock requires human approval.
#[async_trait::async_trait]
pub trait ApprovalNotifier: Send + Sync {
    async fn notify_inbound_knock(&self, peer_id: PeerId, handle: &PeerHandle, reason: &str);
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
            "inbound peering knock awaiting human approval"
        );
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
