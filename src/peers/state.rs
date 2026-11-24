//! Pure functional core for peer trust relationships and peering state transitions.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::identity::{validate_handle, CanonicalBytes, PeerId};
use crate::primitives::Timestamp;

use super::PeerError;

/// Validated human handle for a remote peer (`[a-z0-9_-]{1,32}`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PeerHandle(String);

impl PeerHandle {
    pub fn new(raw: &str) -> Result<Self, PeerError> {
        let valid = validate_handle(raw).map_err(|_| PeerError::InvalidHandle(raw.to_string()))?;
        Ok(Self(valid))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PeerHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lifecycle state of a peering relationship with a remote node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerStatus {
    PendingInbound { reason: String },
    PendingOutbound { reason: String },
    Approved,
    Revoked,
}

impl PeerStatus {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::PendingInbound { .. } => "pending_inbound",
            Self::PendingOutbound { .. } => "pending_outbound",
            Self::Approved => "approved",
            Self::Revoked => "revoked",
        }
    }
}

/// Persisted peer trust relationship record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub peer_id: PeerId,
    pub handle: PeerHandle,
    pub status: PeerStatus,
    pub updated_at: Timestamp,
}

/// Proof-carrying authorization token minted solely for peers in [`PeerStatus::Approved`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedPeer {
    peer_id: PeerId,
    handle: PeerHandle,
}

impl ApprovedPeer {
    pub(super) fn from_peer(peer: &Peer) -> Result<Self, PeerError> {
        match peer.status {
            PeerStatus::Approved => Ok(Self {
                peer_id: peer.peer_id,
                handle: peer.handle.clone(),
            }),
            _ => Err(PeerError::NotApproved(peer.peer_id)),
        }
    }

    pub const fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    pub fn handle(&self) -> &PeerHandle {
        &self.handle
    }
}

/// Signed wire payload requesting peering with a remote node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Knock {
    pub from_handle: String,
    pub reason: String,
    pub sent_at: Timestamp,
}

impl CanonicalBytes for Knock {
    const DOMAIN_TAG: &'static [u8] = b"switchboard.peers.knock.v1";

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        write_len_prefixed(&mut buf, self.from_handle.as_bytes());
        write_len_prefixed(&mut buf, self.reason.as_bytes());
        buf.extend_from_slice(&self.sent_at.as_millis().to_be_bytes());
        buf
    }
}

/// Signed wire payload communicating a human's approval or rejection of a knock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnockDecision {
    pub approved: bool,
    pub decided_at: Timestamp,
}

impl CanonicalBytes for KnockDecision {
    const DOMAIN_TAG: &'static [u8] = b"switchboard.peers.knock_decision.v1";

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(9);
        buf.push(u8::from(self.approved));
        buf.extend_from_slice(&self.decided_at.as_millis().to_be_bytes());
        buf
    }
}

/// State-machine input commands for a peer relationship.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerCommand {
    SendKnock {
        peer_id: PeerId,
        handle: PeerHandle,
        reason: String,
    },
    ReceiveKnock {
        peer_id: PeerId,
        handle: PeerHandle,
        reason: String,
    },
    Approve {
        peer_id: PeerId,
    },
    Reject {
        peer_id: PeerId,
    },
    Revoke {
        peer_id: PeerId,
    },
    ReceiveDecision {
        peer_id: PeerId,
        approved: bool,
    },
}

impl PeerCommand {
    const fn peer_id(&self) -> PeerId {
        match self {
            Self::SendKnock { peer_id, .. }
            | Self::ReceiveKnock { peer_id, .. }
            | Self::Approve { peer_id }
            | Self::Reject { peer_id }
            | Self::Revoke { peer_id }
            | Self::ReceiveDecision { peer_id, .. } => *peer_id,
        }
    }
}

/// Pure side-effect descriptors emitted by [`apply`] for the shell to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerEffect {
    NotifyHumanInboundKnock {
        peer_id: PeerId,
        handle: PeerHandle,
        reason: String,
    },
    DispatchKnock {
        peer_id: PeerId,
        reason: String,
    },
    DispatchDecision {
        peer_id: PeerId,
        approved: bool,
    },
}

/// Applies a peering command to the current peer state, enforcing active human approval invariants.
///
/// Key invariants:
/// - `Approve` and `Reject` are valid only from `PendingInbound` (cannot self-approve `PendingOutbound`).
/// - `ReceiveDecision` is valid only from `PendingOutbound`.
/// - `Revoke` is valid from any existing peer state.
/// - `ReceiveKnock` on `Approved` is idempotent and emits no notification; on any other state
///   (including a simultaneous `PendingOutbound`) it transitions to `PendingInbound` and notifies
///   the human so approval remains explicit.
/// - `SendKnock` is valid only from `None` or `Revoked`.
pub fn apply(
    current: Option<&Peer>,
    cmd: PeerCommand,
    now: Timestamp,
) -> Result<(Peer, Vec<PeerEffect>), PeerError> {
    let peer_id = cmd.peer_id();
    if let Some(existing) = current {
        if existing.peer_id != peer_id {
            return Err(PeerError::PeerIdMismatch {
                expected: existing.peer_id,
                actual: peer_id,
            });
        }
    }

    let current_name = current.map_or("none", |p| p.status.as_str());
    let invalid = |action| {
        Err(PeerError::InvalidTransition {
            current: current_name,
            action,
        })
    };
    let next_peer = |handle, status| Peer {
        peer_id,
        handle,
        status,
        updated_at: now,
    };

    let is_approve = matches!(cmd, PeerCommand::Approve { .. });

    match cmd {
        PeerCommand::SendKnock { handle, reason, .. } => match current.map(|p| &p.status) {
            None | Some(PeerStatus::Revoked) => Ok((
                next_peer(
                    handle,
                    PeerStatus::PendingOutbound {
                        reason: reason.clone(),
                    },
                ),
                vec![PeerEffect::DispatchKnock { peer_id, reason }],
            )),
            Some(_) => invalid("send_knock"),
        },

        PeerCommand::ReceiveKnock { handle, reason, .. } => match current {
            Some(existing) if existing.status == PeerStatus::Approved => {
                Ok((existing.clone(), Vec::new()))
            }
            _ => Ok((
                next_peer(
                    handle.clone(),
                    PeerStatus::PendingInbound {
                        reason: reason.clone(),
                    },
                ),
                vec![PeerEffect::NotifyHumanInboundKnock {
                    peer_id,
                    handle,
                    reason,
                }],
            )),
        },

        PeerCommand::Approve { .. } | PeerCommand::Reject { .. } => {
            let action = if is_approve { "approve" } else { "reject" };
            match current {
                Some(existing) if matches!(existing.status, PeerStatus::PendingInbound { .. }) => {
                    let status = if is_approve {
                        PeerStatus::Approved
                    } else {
                        PeerStatus::Revoked
                    };
                    Ok((
                        next_peer(existing.handle.clone(), status),
                        vec![PeerEffect::DispatchDecision {
                            peer_id,
                            approved: is_approve,
                        }],
                    ))
                }
                _ => invalid(action),
            }
        }

        PeerCommand::Revoke { .. } => match current {
            Some(existing) => Ok((
                next_peer(existing.handle.clone(), PeerStatus::Revoked),
                Vec::new(),
            )),
            None => invalid("revoke"),
        },

        PeerCommand::ReceiveDecision { approved, .. } => match current {
            Some(existing) if matches!(existing.status, PeerStatus::PendingOutbound { .. }) => {
                let status = if approved {
                    PeerStatus::Approved
                } else {
                    PeerStatus::Revoked
                };
                Ok((next_peer(existing.handle.clone(), status), Vec::new()))
            }
            _ => invalid("receive_decision"),
        },
    }
}

fn write_len_prefixed(buf: &mut Vec<u8>, bytes: &[u8]) {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use crate::identity::{sign, verify, LocalKeypair};

    use super::*;

    fn arb_status() -> impl Strategy<Value = PeerStatus> {
        prop_oneof![
            any::<String>().prop_map(|reason| PeerStatus::PendingInbound { reason }),
            any::<String>().prop_map(|reason| PeerStatus::PendingOutbound { reason }),
            Just(PeerStatus::Approved),
            Just(PeerStatus::Revoked),
        ]
    }

    fn arb_opt_status() -> impl Strategy<Value = Option<PeerStatus>> {
        prop_oneof![Just(None), arb_status().prop_map(Some)]
    }

    fn peer_from_seed(seed: [u8; 32], status: PeerStatus) -> Peer {
        let peer_id = LocalKeypair::from_seed(seed).peer_id();
        Peer {
            peer_id,
            handle: PeerHandle::new("alice").unwrap(),
            status,
            updated_at: Timestamp::from_millis(1_000),
        }
    }

    proptest! {
        #[test]
        fn approved_peer_only_mints_when_status_is_approved(
            seed in any::<[u8; 32]>(),
            status in arb_status(),
        ) {
            let peer = peer_from_seed(seed, status.clone());
            let result = ApprovedPeer::from_peer(&peer);

            if matches!(status, PeerStatus::Approved) {
                let token = result.expect("approved peer must mint token");
                prop_assert_eq!(token.peer_id(), peer.peer_id);
                prop_assert_eq!(token.handle(), &peer.handle);
            } else {
                prop_assert!(matches!(result, Err(PeerError::NotApproved(id)) if id == peer.peer_id));
            }
        }

        #[test]
        fn all_transitions_enforce_state_machine_invariants(
            seed in any::<[u8; 32]>(),
            opt_status in arb_opt_status(),
            cmd_tag in 0u8..7u8,
            reason in ".*",
        ) {
            let peer_id = LocalKeypair::from_seed(seed).peer_id();
            let handle = PeerHandle::new("bob").unwrap();
            let current = opt_status.as_ref().map(|status| Peer {
                peer_id,
                handle: handle.clone(),
                status: status.clone(),
                updated_at: Timestamp::from_millis(500),
            });
            let now = Timestamp::from_millis(1_000);

            let cmd = match cmd_tag {
                0 => PeerCommand::SendKnock { peer_id, handle: handle.clone(), reason: reason.clone() },
                1 => PeerCommand::ReceiveKnock { peer_id, handle: handle.clone(), reason: reason.clone() },
                2 => PeerCommand::Approve { peer_id },
                3 => PeerCommand::Reject { peer_id },
                4 => PeerCommand::Revoke { peer_id },
                5 => PeerCommand::ReceiveDecision { peer_id, approved: true },
                _ => PeerCommand::ReceiveDecision { peer_id, approved: false },
            };

            let res = apply(current.as_ref(), cmd, now);

            match (cmd_tag, opt_status) {
                (0, None | Some(PeerStatus::Revoked)) => {
                    let (p, fx) = res.expect("SendKnock allowed from None/Revoked");
                    prop_assert_eq!(p.status, PeerStatus::PendingOutbound { reason: reason.clone() });
                    prop_assert_eq!(fx, vec![PeerEffect::DispatchKnock { peer_id, reason }]);
                }
                (0, _) => prop_assert!(is_invalid_action(&res, "send_knock")),
                (1, Some(PeerStatus::Approved)) => {
                    let (p, fx) = res.expect("ReceiveKnock on Approved is idempotent");
                    prop_assert_eq!(p.status, PeerStatus::Approved);
                    prop_assert!(fx.is_empty());
                }
                (1, _) => {
                    let (p, fx) = res.expect("ReceiveKnock creates PendingInbound");
                    prop_assert_eq!(p.status, PeerStatus::PendingInbound { reason: reason.clone() });
                    prop_assert_eq!(fx, vec![PeerEffect::NotifyHumanInboundKnock { peer_id, handle, reason }]);
                }
                (2 | 3, Some(PeerStatus::PendingInbound { .. })) => {
                    let approved = cmd_tag == 2;
                    let (p, fx) = res.expect("Approve/Reject allowed from PendingInbound");
                    let expected = if approved { PeerStatus::Approved } else { PeerStatus::Revoked };
                    prop_assert_eq!(p.status, expected);
                    prop_assert_eq!(fx, vec![PeerEffect::DispatchDecision { peer_id, approved }]);
                }
                (2, _) => prop_assert!(is_invalid_action(&res, "approve")),
                (3, _) => prop_assert!(is_invalid_action(&res, "reject")),
                (4, Some(_)) => {
                    let (p, fx) = res.expect("Revoke allowed from any existing state");
                    prop_assert_eq!(p.status, PeerStatus::Revoked);
                    prop_assert!(fx.is_empty());
                }
                (4, None) => prop_assert!(is_invalid_action(&res, "revoke")),
                (5 | 6, Some(PeerStatus::PendingOutbound { .. })) => {
                    let (p, fx) = res.expect("ReceiveDecision allowed from PendingOutbound");
                    let expected = if cmd_tag == 5 { PeerStatus::Approved } else { PeerStatus::Revoked };
                    prop_assert_eq!(p.status, expected);
                    prop_assert!(fx.is_empty());
                }
                (5 | 6, _) => prop_assert!(is_invalid_action(&res, "receive_decision")),
                _ => unreachable!(),
            }
        }
    }

    fn is_invalid_action(res: &Result<(Peer, Vec<PeerEffect>), PeerError>, expected: &str) -> bool {
        matches!(res, Err(PeerError::InvalidTransition { action, .. }) if *action == expected)
    }

    #[test]
    fn canonical_bytes_prevents_field_boundary_collision_and_verifies() {
        let kp = LocalKeypair::generate();
        let ts = Timestamp::from_millis(42);
        let k1 = Knock {
            from_handle: "ab".into(),
            reason: "c".into(),
            sent_at: ts,
        };
        let k2 = Knock {
            from_handle: "a".into(),
            reason: "bc".into(),
            sent_at: ts,
        };
        assert_ne!(k1.canonical_bytes(), k2.canonical_bytes());
        assert_eq!(verify(sign(&kp, k1.clone())).unwrap().payload(), &k1);

        let decision = KnockDecision {
            approved: true,
            decided_at: ts,
        };
        assert_eq!(
            verify(sign(&kp, decision.clone())).unwrap().payload(),
            &decision
        );
    }

    #[test]
    fn cannot_self_approve_pending_outbound_and_rejects_mismatched_id() {
        let outbound = peer_from_seed(
            [7u8; 32],
            PeerStatus::PendingOutbound {
                reason: "hello".into(),
            },
        );
        let cmd = PeerCommand::Approve {
            peer_id: outbound.peer_id,
        };
        assert!(matches!(
            apply(Some(&outbound), cmd, Timestamp::from_millis(20)),
            Err(PeerError::InvalidTransition {
                current: "pending_outbound",
                action: "approve"
            })
        ));

        let approved = peer_from_seed([8u8; 32], PeerStatus::Approved);
        let other = LocalKeypair::from_seed([9u8; 32]).peer_id();
        let mismatch = apply(
            Some(&approved),
            PeerCommand::Revoke { peer_id: other },
            Timestamp::from_millis(20),
        );
        assert!(matches!(
            mismatch,
            Err(PeerError::PeerIdMismatch { expected, actual })
                if expected == approved.peer_id && actual == other
        ));
    }
}
