//! Inbound authenticated P2P wire frame handlers for [`SwitchboardNode`].

use tokio_util::sync::CancellationToken;

use crate::calls::CallRequest;
use crate::directory::DirectoryPort;
use crate::extensions::AgentHarness;
use crate::identity::{self, PeerId, Signed};
use crate::peers::{ApprovalNotifier, ApprovalPromptOutcome, Knock, KnockDecision, PeerStatus};
use crate::primitives::{Clock, IdGen};
use crate::transport::{FrameKind, PeerDialer, PeerListener, WireFrame};

use super::{error_frame, SwitchboardNode};

impl<T, D, H, C, G, N> SwitchboardNode<T, D, H, C, G, N>
where
    T: PeerDialer + PeerListener + 'static,
    D: DirectoryPort + 'static,
    H: AgentHarness + Clone + 'static,
    C: Clock + Clone + 'static,
    G: IdGen + Clone + 'static,
    N: ApprovalNotifier + 'static,
{
    pub(super) async fn handle_inbound_knock_frame(
        &self,
        remote: PeerId,
        frame: &WireFrame,
    ) -> WireFrame {
        let signed: Signed<Knock> = match serde_json::from_value(frame.payload.clone()) {
            Ok(v) => v,
            Err(e) => return error_frame(&format!("invalid knock payload: {e}")),
        };
        let verified = match identity::verify(signed) {
            Ok(v) if v.signer() == remote => v,
            Ok(_) => return error_frame("knock signer does not match authenticated peer"),
            Err(e) => return error_frame(&format!("knock verification failed: {e}")),
        };
        match self.peers.handle_inbound_knock(verified).await {
            Ok((peer, _)) if peer.status == PeerStatus::Approved => {
                self.signed_decision_frame(true).await
            }
            Ok((peer, _)) => {
                let reason = match &peer.status {
                    PeerStatus::PendingInbound { reason } => reason.as_str(),
                    _ => "",
                };
                match self
                    .peers
                    .prompt_inbound_approval(peer.peer_id, &peer.handle, reason)
                    .await
                {
                    ApprovalPromptOutcome::Approve => {
                        match self.peers.approve_by_handle(peer.handle.as_str()).await {
                            Ok(_) => self.signed_decision_frame(true).await,
                            Err(e) => error_frame(&e.to_string()),
                        }
                    }
                    ApprovalPromptOutcome::Reject => {
                        match self.peers.reject_by_handle(peer.handle.as_str()).await {
                            Ok(_) => self.signed_decision_frame(false).await,
                            Err(e) => error_frame(&e.to_string()),
                        }
                    }
                    ApprovalPromptOutcome::Pending => {
                        WireFrame::new(FrameKind::KnockDecision, serde_json::json!({ "ack": true }))
                    }
                }
            }
            Err(e) => error_frame(&e.to_string()),
        }
    }

    pub(super) async fn signed_decision_frame(&self, approved: bool) -> WireFrame {
        let Ok(local) = self.identity.load_required().await else {
            return error_frame("local identity unavailable");
        };
        let dec = identity::sign(
            &local.keypair,
            KnockDecision {
                approved,
                decided_at: self.clock.now(),
            },
        );
        serde_json::to_value(&dec)
            .map(|val| WireFrame::new(FrameKind::KnockDecision, val))
            .unwrap_or_else(|e| error_frame(&e.to_string()))
    }

    pub(super) async fn handle_inbound_decision_frame(
        &self,
        remote: PeerId,
        frame: &WireFrame,
    ) -> WireFrame {
        let signed: Signed<KnockDecision> = match serde_json::from_value(frame.payload.clone()) {
            Ok(v) => v,
            Err(e) => return error_frame(&format!("invalid decision payload: {e}")),
        };
        let verified = match identity::verify(signed) {
            Ok(v) if v.signer() == remote => v,
            Ok(_) => return error_frame("decision signer does not match authenticated peer"),
            Err(e) => return error_frame(&format!("decision verification failed: {e}")),
        };
        match self.peers.handle_remote_decision(verified).await {
            Ok(_) => WireFrame::new(FrameKind::KnockDecision, serde_json::json!({ "ack": true })),
            Err(e) => error_frame(&e.to_string()),
        }
    }

    pub(super) async fn handle_inbound_call_frame(
        &self,
        remote: PeerId,
        frame: &WireFrame,
        cancel: CancellationToken,
    ) -> WireFrame {
        let caller = match self.peers.authorize(remote).await {
            Ok(approved) => approved,
            Err(e) => return error_frame(&e.to_string()),
        };
        let req: CallRequest = match serde_json::from_value(frame.payload.clone()) {
            Ok(r) => r,
            Err(e) => return error_frame(&format!("invalid call request: {e}")),
        };
        match self.calls.handle_inbound_call(caller, req, cancel).await {
            Ok(resp) => match serde_json::to_value(&resp) {
                Ok(val) => WireFrame::new(FrameKind::CallResponse, val),
                Err(e) => error_frame(&format!("response encode error: {e}")),
            },
            Err(e) => error_frame(&e.to_string()),
        }
    }
}
