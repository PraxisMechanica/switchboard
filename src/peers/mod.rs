//! Peers vertical slice: trust relationships, peering state machine, and active human approval.

pub mod cli;
pub mod notify;
pub mod repo;
pub mod state;

use thiserror::Error;

use crate::db::{Database, DbError};
use crate::identity::{PeerId, Verified};
use crate::primitives::Clock;

pub use notify::{ApprovalNotifier, InboundKnockNotice, RecordingNotifier, TracingNotifier};
pub use repo::{migrate, PeersRepo};
pub use state::{
    apply, ApprovedPeer, Knock, KnockDecision, Peer, PeerCommand, PeerEffect, PeerHandle,
    PeerStatus,
};

/// Typed errors for peer trust management and state transitions.
#[derive(Debug, Error)]
pub enum PeerError {
    #[error("invalid peer handle '{0}': must be 1..=32 lowercase ASCII alphanumeric, '-', or '_'")]
    InvalidHandle(String),

    #[error("peer '{0}' not found")]
    NotFound(String),

    #[error("peer {0} is not approved")]
    NotApproved(PeerId),

    #[error("invalid peer state transition: cannot {action} from {current}")]
    InvalidTransition {
        current: &'static str,
        action: &'static str,
    },

    #[error("peer id mismatch: expected {expected}, got {actual}")]
    PeerIdMismatch { expected: PeerId, actual: PeerId },

    #[error("corrupt peer record in database: {0}")]
    CorruptRecord(String),

    #[error("peer storage error: {0}")]
    Storage(#[from] rusqlite::Error),

    #[error("database runtime error: {0}")]
    Database(#[from] DbError),
}

/// Application service orchestrating peer trust transitions, persistence, and approval notices.
#[derive(Clone)]
pub struct PeersService<C: Clock, N: ApprovalNotifier> {
    repo: PeersRepo,
    clock: C,
    notifier: N,
}

impl<C: Clock, N: ApprovalNotifier> PeersService<C, N> {
    pub fn new(db: Database, clock: C, notifier: N) -> Self {
        Self {
            repo: PeersRepo::new(db),
            clock,
            notifier,
        }
    }

    /// The single authorization gate minting a proof-carrying [`ApprovedPeer`] token.
    pub async fn authorize(&self, peer_id: PeerId) -> Result<ApprovedPeer, PeerError> {
        let peer = self
            .repo
            .get_by_id(peer_id)
            .await?
            .ok_or(PeerError::NotApproved(peer_id))?;
        ApprovedPeer::from_peer(&peer)
    }

    pub async fn record_outbound_knock(
        &self,
        peer_id: PeerId,
        handle: PeerHandle,
        reason: String,
    ) -> Result<(Peer, Vec<PeerEffect>), PeerError> {
        let existing = self.repo.get_by_id(peer_id).await?;
        let (updated, effects) = apply(
            existing.as_ref(),
            PeerCommand::SendKnock {
                peer_id,
                handle,
                reason,
            },
            self.clock.now(),
        )?;
        self.repo.upsert(&updated).await?;
        Ok((updated, effects))
    }

    pub async fn handle_inbound_knock(
        &self,
        verified: Verified<Knock>,
    ) -> Result<(Peer, Vec<PeerEffect>), PeerError> {
        let (peer_id, knock) = verified.into_parts();
        let handle = PeerHandle::new(&knock.from_handle)?;
        let existing = self.repo.get_by_id(peer_id).await?;
        let (updated, effects) = apply(
            existing.as_ref(),
            PeerCommand::ReceiveKnock {
                peer_id,
                handle,
                reason: knock.reason,
            },
            self.clock.now(),
        )?;
        self.repo.upsert(&updated).await?;

        for effect in &effects {
            if let PeerEffect::NotifyHumanInboundKnock {
                peer_id,
                handle,
                reason,
            } = effect
            {
                self.notifier
                    .notify_inbound_knock(*peer_id, handle, reason)
                    .await;
            }
        }

        Ok((updated, effects))
    }

    pub async fn approve_by_handle(
        &self,
        handle: &str,
    ) -> Result<(Peer, Vec<PeerEffect>), PeerError> {
        let existing = self
            .repo
            .get_by_handle(handle)
            .await?
            .ok_or_else(|| PeerError::NotFound(handle.to_string()))?;
        let (updated, effects) = apply(
            Some(&existing),
            PeerCommand::Approve {
                peer_id: existing.peer_id,
            },
            self.clock.now(),
        )?;
        self.repo.upsert(&updated).await?;
        Ok((updated, effects))
    }

    pub async fn reject_by_handle(
        &self,
        handle: &str,
    ) -> Result<(Peer, Vec<PeerEffect>), PeerError> {
        let existing = self
            .repo
            .get_by_handle(handle)
            .await?
            .ok_or_else(|| PeerError::NotFound(handle.to_string()))?;
        let (updated, effects) = apply(
            Some(&existing),
            PeerCommand::Reject {
                peer_id: existing.peer_id,
            },
            self.clock.now(),
        )?;
        self.repo.upsert(&updated).await?;
        Ok((updated, effects))
    }

    pub async fn revoke_by_handle(
        &self,
        handle: &str,
    ) -> Result<(Peer, Vec<PeerEffect>), PeerError> {
        let existing = self
            .repo
            .get_by_handle(handle)
            .await?
            .ok_or_else(|| PeerError::NotFound(handle.to_string()))?;
        let (updated, effects) = apply(
            Some(&existing),
            PeerCommand::Revoke {
                peer_id: existing.peer_id,
            },
            self.clock.now(),
        )?;
        self.repo.upsert(&updated).await?;
        Ok((updated, effects))
    }

    pub async fn handle_remote_decision(
        &self,
        verified: Verified<KnockDecision>,
    ) -> Result<Peer, PeerError> {
        let (peer_id, decision) = verified.into_parts();
        let existing = self.repo.get_by_id(peer_id).await?;
        let (updated, _effects) = apply(
            existing.as_ref(),
            PeerCommand::ReceiveDecision {
                peer_id,
                approved: decision.approved,
            },
            self.clock.now(),
        )?;
        self.repo.upsert(&updated).await?;
        Ok(updated)
    }

    pub async fn list_all(&self) -> Result<Vec<Peer>, PeerError> {
        self.repo.list_all().await
    }

    pub async fn list_pending_inbound(&self) -> Result<Vec<Peer>, PeerError> {
        self.repo.list_pending_inbound().await
    }

    pub async fn list_approved(&self) -> Result<Vec<Peer>, PeerError> {
        self.repo.list_approved().await
    }

    pub async fn resolve_approved_by_handle(
        &self,
        handle: &str,
    ) -> Result<ApprovedPeer, PeerError> {
        let peer = self
            .repo
            .get_by_handle(handle)
            .await?
            .ok_or_else(|| PeerError::NotFound(handle.to_string()))?;
        self.authorize(peer.peer_id).await
    }
}

#[cfg(test)]
mod tests {
    use crate::identity::{sign, verify, LocalKeypair};
    use crate::primitives::{FixedClock, Timestamp};

    use super::*;

    fn test_service() -> (
        PeersService<FixedClock, RecordingNotifier>,
        RecordingNotifier,
    ) {
        let db = Database::open_in_memory().unwrap();
        db.with_conn_sync(|c| migrate(c).unwrap());
        let notifier = RecordingNotifier::new();
        let svc = PeersService::new(db, FixedClock::new(1_700_000_000_000), notifier.clone());
        (svc, notifier)
    }

    #[tokio::test]
    async fn inbound_knock_approval_and_revocation_lifecycle() {
        let (svc, notifier) = test_service();
        let remote = LocalKeypair::generate();

        let knock = Knock {
            from_handle: "alice".to_string(),
            reason: "sync weekly notes".to_string(),
            sent_at: Timestamp::from_millis(1_700_000_000_000),
        };
        let verified = verify(sign(&remote, knock)).unwrap();

        let (peer, effects) = svc.handle_inbound_knock(verified).await.unwrap();
        assert!(matches!(peer.status, PeerStatus::PendingInbound { .. }));
        assert_eq!(effects.len(), 1);
        assert_eq!(notifier.len(), 1);
        assert_eq!(notifier.recorded()[0].handle.as_str(), "alice");

        assert!(matches!(
            svc.authorize(remote.peer_id()).await,
            Err(PeerError::NotApproved(_))
        ));
        assert_eq!(svc.list_pending_inbound().await.unwrap().len(), 1);
        assert!(svc.list_approved().await.unwrap().is_empty());

        let (approved, decision_effects) = svc.approve_by_handle("alice").await.unwrap();
        assert_eq!(approved.status, PeerStatus::Approved);
        assert_eq!(
            decision_effects,
            vec![PeerEffect::DispatchDecision {
                peer_id: remote.peer_id(),
                approved: true,
            }]
        );

        let token = svc.authorize(remote.peer_id()).await.unwrap();
        assert_eq!(token.peer_id(), remote.peer_id());
        assert_eq!(token.handle().as_str(), "alice");

        let by_handle = svc.resolve_approved_by_handle("alice").await.unwrap();
        assert_eq!(by_handle, token);
        assert_eq!(svc.list_approved().await.unwrap().len(), 1);

        let duplicate = verify(sign(
            &remote,
            Knock {
                from_handle: "alice".to_string(),
                reason: "retry".to_string(),
                sent_at: Timestamp::from_millis(1_700_000_001_000),
            },
        ))
        .unwrap();
        let (still_approved, dup_effects) = svc.handle_inbound_knock(duplicate).await.unwrap();
        assert_eq!(still_approved.status, PeerStatus::Approved);
        assert!(dup_effects.is_empty());
        assert_eq!(notifier.len(), 1);

        let (revoked, revoke_effects) = svc.revoke_by_handle("alice").await.unwrap();
        assert_eq!(revoked.status, PeerStatus::Revoked);
        assert!(revoke_effects.is_empty());
        assert!(matches!(
            svc.authorize(remote.peer_id()).await,
            Err(PeerError::NotApproved(_))
        ));
    }

    #[tokio::test]
    async fn outbound_knock_requires_remote_decision_and_blocks_self_approval() {
        let (svc, _) = test_service();
        let remote = LocalKeypair::generate();
        let handle = PeerHandle::new("bob").unwrap();

        let (pending, effects) = svc
            .record_outbound_knock(remote.peer_id(), handle, "let us collaborate".to_string())
            .await
            .unwrap();
        assert!(matches!(pending.status, PeerStatus::PendingOutbound { .. }));
        assert_eq!(effects.len(), 1);

        assert!(matches!(
            svc.approve_by_handle("bob").await,
            Err(PeerError::InvalidTransition {
                current: "pending_outbound",
                action: "approve",
            })
        ));
        assert!(matches!(
            svc.authorize(remote.peer_id()).await,
            Err(PeerError::NotApproved(_))
        ));

        let decision = verify(sign(
            &remote,
            KnockDecision {
                approved: true,
                decided_at: Timestamp::from_millis(1_700_000_002_000),
            },
        ))
        .unwrap();
        let approved = svc.handle_remote_decision(decision).await.unwrap();
        assert_eq!(approved.status, PeerStatus::Approved);
        assert!(svc.authorize(remote.peer_id()).await.is_ok());
    }

    #[tokio::test]
    async fn inbound_rejection_and_simultaneous_knock_preserve_explicit_approval() {
        let (svc, notifier) = test_service();
        let remote = LocalKeypair::generate();
        let handle = PeerHandle::new("carol").unwrap();

        svc.record_outbound_knock(remote.peer_id(), handle, "outbound".to_string())
            .await
            .unwrap();

        let simultaneous = verify(sign(
            &remote,
            Knock {
                from_handle: "carol".to_string(),
                reason: "inbound".to_string(),
                sent_at: Timestamp::from_millis(1_700_000_003_000),
            },
        ))
        .unwrap();
        let (peer, _) = svc.handle_inbound_knock(simultaneous).await.unwrap();
        assert!(matches!(peer.status, PeerStatus::PendingInbound { .. }));
        assert_eq!(notifier.len(), 1);
        assert!(matches!(
            svc.authorize(remote.peer_id()).await,
            Err(PeerError::NotApproved(_))
        ));

        let (rejected, effects) = svc.reject_by_handle("carol").await.unwrap();
        assert_eq!(rejected.status, PeerStatus::Revoked);
        assert_eq!(
            effects,
            vec![PeerEffect::DispatchDecision {
                peer_id: remote.peer_id(),
                approved: false,
            }]
        );
        assert_eq!(svc.list_all().await.unwrap().len(), 1);
    }
}
