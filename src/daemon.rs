//! Generic daemon node composing all vertical domains through static traits.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::calls::{CallError, CallId, CallRequest, CallResponse, CallsService, ReplyCallCommand};
use crate::db::Database;
use crate::directory::{
    AdvertisedExtension, DirectoryError, DirectoryPort, DirectoryService, PeerCard,
};
use crate::extensions::{AgentHarness, ExtensionError, ExtensionName, ExtensionsService};
use crate::identity::{self, IdentityError, IdentityService, PeerId, Signed};
use crate::peers::{
    ApprovalNotifier, Knock, KnockDecision, Peer, PeerEffect, PeerError, PeerHandle, PeerStatus,
    PeersService,
};
use crate::primitives::{Clock, IdGen};
use crate::transport::{
    FrameKind, InboundExchange, PeerDialer, PeerEndpoint, PeerListener, TransportError, WireFrame,
};

/// Runs every vertical domain's own SQLite schema migration at startup.
pub fn migrate_all(db: &Database) -> anyhow::Result<()> {
    db.with_conn_sync(|conn| -> anyhow::Result<()> {
        crate::identity::migrate(conn)?;
        crate::directory::migrate(conn)?;
        crate::peers::migrate(conn)?;
        crate::extensions::migrate(conn)?;
        crate::calls::migrate(conn)?;
        Ok(())
    })
}

#[derive(Debug, Error)]
pub enum NodeError {
    #[error("identity error: {0}")]
    Identity(#[from] IdentityError),

    #[error("directory error: {0}")]
    Directory(#[from] DirectoryError),

    #[error("peer error: {0}")]
    Peer(#[from] PeerError),

    #[error("extension error: {0}")]
    Extension(#[from] ExtensionError),

    #[error("call error: {0}")]
    Call(#[from] CallError),

    #[error("transport error: {0}")]
    Transport(#[from] TransportError),

    #[error("invalid call target '{0}': expected '<peer>.<harness>.<conversation-name-or-id>'")]
    InvalidTarget(String),

    #[error("unknown peer '{0}': sync the directory or supply a hex peer id")]
    UnknownPeer(String),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Composed switchboard node parameterized over concrete transport, directory, harness, clock, and notifier.
pub struct SwitchboardNode<T, D, H, C, G, N>
where
    T: PeerDialer + PeerListener + 'static,
    D: DirectoryPort + 'static,
    H: AgentHarness + 'static,
    C: Clock + Clone + 'static,
    G: IdGen + Clone + 'static,
    N: ApprovalNotifier + 'static,
{
    pub identity: IdentityService<C>,
    pub directory: DirectoryService<D>,
    pub peers: PeersService<C, N>,
    pub extensions: ExtensionsService<H, C>,
    pub calls: CallsService<Arc<T>, H, C, G>,
    pub transport: Arc<T>,
    clock: C,
}

impl<T, D, H, C, G, N> SwitchboardNode<T, D, H, C, G, N>
where
    T: PeerDialer + PeerListener + 'static,
    D: DirectoryPort + 'static,
    H: AgentHarness + Clone + 'static,
    C: Clock + Clone + 'static,
    G: IdGen + Clone + 'static,
    N: ApprovalNotifier + 'static,
{
    pub fn new(
        db: Database,
        transport: T,
        directory_port: D,
        harness: H,
        clock: C,
        id_gen: G,
        notifier: N,
    ) -> Self {
        let transport = Arc::new(transport);
        let extensions = ExtensionsService::new(db.clone(), harness, clock.clone());
        let calls = CallsService::new(
            db.clone(),
            Arc::clone(&transport),
            extensions.clone(),
            clock.clone(),
            id_gen,
        );
        Self {
            identity: IdentityService::new(db.clone(), clock.clone()),
            directory: DirectoryService::new(db.clone(), directory_port),
            peers: PeersService::new(db, clock.clone(), notifier),
            extensions,
            calls,
            transport,
            clock,
        }
    }

    /// Publishes this node's signed [`PeerCard`] with current extensions and direct addresses.
    pub async fn publish_directory_card(
        &self,
        direct_addrs: Vec<String>,
    ) -> Result<PeerCard, NodeError> {
        let local = self.identity.load_required().await?;
        let exts = self.extensions.list().await?;
        let card = PeerCard {
            handle: local.handle,
            peer_id: local.keypair.peer_id(),
            direct_addrs,
            extensions: exts
                .into_iter()
                .map(|e| AdvertisedExtension {
                    name: e.name.to_string(),
                    description: e.description,
                })
                .collect(),
            updated_at: self.clock.now(),
        };
        self.directory
            .publish_local(&local.keypair, card.clone())
            .await?;
        Ok(card)
    }

    /// Sends a signed knock to a peer discovered in the directory (or specified by hex `PeerId`).
    pub async fn knock_peer(
        &self,
        target: &str,
        reason: String,
        endpoint_override: Option<PeerEndpoint>,
        timeout: Duration,
    ) -> Result<Peer, NodeError> {
        let local = self.identity.load_required().await?;
        let (peer_id, handle, endpoint) = self
            .resolve_target_endpoint(target, endpoint_override)
            .await?;

        let (peer, effects) = self
            .peers
            .record_outbound_knock(peer_id, handle, reason.clone())
            .await?;

        let mut peer = peer;
        for effect in effects {
            if let PeerEffect::DispatchKnock { reason, .. } = effect {
                let signed = identity::sign(
                    &local.keypair,
                    Knock {
                        from_handle: local.handle.clone(),
                        reason,
                        sent_at: self.clock.now(),
                    },
                );
                let frame = WireFrame::new(FrameKind::Knock, serde_json::to_value(&signed)?);
                let resp = self.transport.request(&endpoint, frame, timeout).await?;
                if resp.kind == FrameKind::KnockDecision {
                    if let Ok(signed_dec) =
                        serde_json::from_value::<Signed<KnockDecision>>(resp.payload)
                    {
                        if let Ok(verified_dec) = identity::verify(signed_dec) {
                            if verified_dec.signer() == peer_id {
                                peer = self.peers.handle_remote_decision(verified_dec).await?;
                            }
                        }
                    }
                }
            }
        }
        Ok(peer)
    }

    /// Approves an inbound knock (or re-dispatches an existing approval) to the remote peer.
    pub async fn approve_peer(
        &self,
        handle: &str,
        endpoint_override: Option<PeerEndpoint>,
        timeout: Duration,
    ) -> Result<Peer, NodeError> {
        let (peer, effects) = match self.peers.approve_by_handle(handle).await {
            Ok(pair) => pair,
            Err(PeerError::InvalidTransition {
                current: "approved",
                ..
            }) => {
                let approved = self.peers.resolve_approved_by_handle(handle).await?;
                let peer = self
                    .peers
                    .list_all()
                    .await?
                    .into_iter()
                    .find(|p| p.peer_id == approved.peer_id())
                    .ok_or_else(|| PeerError::NotFound(handle.to_string()))?;
                (
                    peer,
                    vec![PeerEffect::DispatchDecision {
                        peer_id: approved.peer_id(),
                        approved: true,
                    }],
                )
            }
            Err(e) => return Err(NodeError::Peer(e)),
        };
        self.dispatch_decision_effects(peer.peer_id, effects, endpoint_override, timeout)
            .await?;
        Ok(peer)
    }

    /// Rejects an inbound knock and dispatches a signed rejection decision to the remote peer.
    pub async fn reject_peer(
        &self,
        handle: &str,
        endpoint_override: Option<PeerEndpoint>,
        timeout: Duration,
    ) -> Result<Peer, NodeError> {
        let (peer, effects) = self.peers.reject_by_handle(handle).await?;
        self.dispatch_decision_effects(peer.peer_id, effects, endpoint_override, timeout)
            .await?;
        Ok(peer)
    }

    /// Places a call to `<peer>.<harness>.<conversation-name-or-id>` (or legacy `<peer>/<ext>`)
    /// after verifying the peer is approved.
    pub async fn call_extension(
        &self,
        target: &str,
        message: String,
        endpoint_override: Option<PeerEndpoint>,
        timeout: Duration,
    ) -> Result<CallResponse, NodeError> {
        let (peer_handle, ext_raw) = parse_call_target(target)
            .ok_or_else(|| NodeError::InvalidTarget(target.to_string()))?;
        let ext = ExtensionName::new(ext_raw)?;
        let local = self.identity.load_required().await?;
        let approved = self.peers.resolve_approved_by_handle(peer_handle).await?;
        let endpoint = self
            .endpoint_for_peer(approved.peer_id(), endpoint_override)
            .await?;
        Ok(self
            .calls
            .place_call(approved, &endpoint, &local.handle, ext, message, timeout)
            .await?)
    }

    /// Sends a follow-up turn on an existing outbound call.
    pub async fn reply_call(
        &self,
        call_id: CallId,
        message: String,
        done: bool,
        endpoint_override: Option<PeerEndpoint>,
        timeout: Duration,
    ) -> Result<CallResponse, NodeError> {
        let local = self.identity.load_required().await?;
        let record = self.calls.get_call(call_id).await?;
        let approved = self.peers.authorize(record.peer_id).await?;
        let endpoint = self
            .endpoint_for_peer(approved.peer_id(), endpoint_override)
            .await?;
        Ok(self
            .calls
            .reply_call(ReplyCallCommand {
                call_id,
                target: approved,
                endpoint: &endpoint,
                local_handle: &local.handle,
                body: message,
                done,
                timeout,
            })
            .await?)
    }

    /// Processes a single authenticated inbound wire exchange.
    pub async fn handle_exchange(&self, exchange: InboundExchange, cancel: CancellationToken) {
        let remote_peer = exchange.remote_peer;
        let reply = match exchange.request.kind {
            FrameKind::Knock => {
                self.handle_inbound_knock_frame(remote_peer, &exchange.request)
                    .await
            }
            FrameKind::KnockDecision => {
                self.handle_inbound_decision_frame(remote_peer, &exchange.request)
                    .await
            }
            FrameKind::CallRequest => {
                self.handle_inbound_call_frame(remote_peer, &exchange.request, cancel)
                    .await
            }
            other => error_frame(&format!("unexpected request frame kind: {other:?}")),
        };
        let _ = exchange.respond(reply);
    }

    /// Runs the background P2P accept loop until `cancel` is triggered.
    pub async fn run_accept_loop(self: Arc<Self>, cancel: CancellationToken) {
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => break,
                res = self.transport.accept() => res,
            };
            match next {
                Ok(exchange) => {
                    let node = Arc::clone(&self);
                    let turn_cancel = cancel.child_token();
                    tokio::spawn(async move {
                        node.handle_exchange(exchange, turn_cancel).await;
                    });
                }
                Err(TransportError::Closed) => break,
                Err(err) => {
                    tracing::warn!(error = %err, "transport accept error");
                }
            }
        }
    }

    async fn dispatch_decision_effects(
        &self,
        peer_id: PeerId,
        effects: Vec<PeerEffect>,
        endpoint_override: Option<PeerEndpoint>,
        timeout: Duration,
    ) -> Result<(), NodeError> {
        let local = self.identity.load_required().await?;
        let endpoint = self.endpoint_for_peer(peer_id, endpoint_override).await?;
        for effect in effects {
            if let PeerEffect::DispatchDecision { approved, .. } = effect {
                let signed = identity::sign(
                    &local.keypair,
                    KnockDecision {
                        approved,
                        decided_at: self.clock.now(),
                    },
                );
                let frame =
                    WireFrame::new(FrameKind::KnockDecision, serde_json::to_value(&signed)?);
                let _ = self.transport.request(&endpoint, frame, timeout).await?;
            }
        }
        Ok(())
    }

    async fn handle_inbound_knock_frame(&self, remote: PeerId, frame: &WireFrame) -> WireFrame {
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
                let Ok(local) = self.identity.load_required().await else {
                    return error_frame("local identity unavailable");
                };
                let dec = identity::sign(
                    &local.keypair,
                    KnockDecision {
                        approved: true,
                        decided_at: self.clock.now(),
                    },
                );
                serde_json::to_value(&dec)
                    .map(|val| WireFrame::new(FrameKind::KnockDecision, val))
                    .unwrap_or_else(|e| error_frame(&e.to_string()))
            }
            Ok(_) => WireFrame::new(FrameKind::KnockDecision, serde_json::json!({ "ack": true })),
            Err(e) => error_frame(&e.to_string()),
        }
    }

    async fn handle_inbound_decision_frame(&self, remote: PeerId, frame: &WireFrame) -> WireFrame {
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

    async fn handle_inbound_call_frame(
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

    async fn resolve_target_endpoint(
        &self,
        target: &str,
        override_ep: Option<PeerEndpoint>,
    ) -> Result<(PeerId, PeerHandle, PeerEndpoint), NodeError> {
        if let Some(card) = self.directory.resolve_handle(target).await? {
            let handle = PeerHandle::new(&card.handle)?;
            let ep = match override_ep {
                Some(ep) => ep,
                None => endpoint_from_card(&card),
            };
            return Ok((card.peer_id, handle, ep));
        }

        if let Ok(peer_id) = target.parse::<PeerId>() {
            let handle = PeerHandle::new(&format!("peer-{}", &peer_id.short()[..8]))?;
            let ep = override_ep.unwrap_or_else(|| PeerEndpoint::from_peer_id(peer_id));
            return Ok((peer_id, handle, ep));
        }

        Err(NodeError::UnknownPeer(target.to_string()))
    }

    async fn endpoint_for_peer(
        &self,
        peer_id: PeerId,
        override_ep: Option<PeerEndpoint>,
    ) -> Result<PeerEndpoint, NodeError> {
        if let Some(ep) = override_ep {
            return Ok(ep);
        }
        if let Some(card) = self.directory.resolve_peer_id(peer_id).await? {
            return Ok(endpoint_from_card(&card));
        }
        Ok(PeerEndpoint::from_peer_id(peer_id))
    }
}

fn endpoint_from_card(card: &PeerCard) -> PeerEndpoint {
    let direct_addrs = card
        .direct_addrs
        .iter()
        .filter_map(|s| s.parse::<SocketAddr>().ok())
        .collect();
    PeerEndpoint {
        peer_id: card.peer_id,
        direct_addrs,
        relay_url: None,
    }
}

/// Parses `<user-or-machine>.<harness>.<conversation-name-or-id>` (including `codex://threads/...`)
/// or legacy `<peer>/<extension>` into `(peer_handle, target_suffix)`.
pub fn parse_call_target(target: &str) -> Option<(&str, &str)> {
    let trimmed = target.trim();
    if let Some((peer, rest)) = trimmed.split_once('.') {
        if !peer.is_empty() && !peer.contains('/') && rest.contains('.') {
            return Some((peer, rest));
        }
    }
    let (peer, ext) = trimmed.split_once('/')?;
    if peer.is_empty() || ext.is_empty() {
        return None;
    }
    Some((peer, ext))
}

fn error_frame(message: &str) -> WireFrame {
    WireFrame::new(FrameKind::Error, serde_json::json!({ "error": message }))
}
