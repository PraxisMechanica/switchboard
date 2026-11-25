//! Calls vertical slice: outbound dialing, compile-time-gated inbound dispatch, and transcripts.

pub mod cli;
pub mod lock;
pub mod model;
pub mod repo;

use std::time::Duration;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::db::{Database, DbError};
use crate::extensions::{
    AgentHarness, DeliveryOutcome, ExtensionError, ExtensionName, ExtensionsService,
};
use crate::identity::PeerId;
use crate::peers::ApprovedPeer;
use crate::primitives::{run_idempotent, Clock, IdGen, IdempotentError};
use crate::transport::{FrameKind, PeerDialer, PeerEndpoint, TransportError, WireFrame};

pub use cli::{CallArgs, HistoryArgs, ReplyArgs};
pub use lock::CallTurnLocks;
pub use model::{
    CallDirection, CallId, CallMessageRecord, CallRecord, CallRequest, CallResponse,
    CallResponseStatus, CallStatus, MessageId,
};
pub use repo::{migrate, CallsRepo};

/// Errors produced by call orchestration and persistence.
#[derive(Debug, Error)]
pub enum CallError {
    #[error("invalid call or message id '{0}'")]
    InvalidId(String),

    #[error("call '{0}' not found")]
    NotFound(CallId),

    #[error("call '{0}' is already closed")]
    CallClosed(CallId),

    #[error("call '{call_id}' belongs to peer {expected}, not {actual}")]
    PeerMismatch {
        call_id: CallId,
        expected: PeerId,
        actual: PeerId,
    },

    #[error("idempotency key {0} reused with mismatched payload")]
    PayloadMismatch(MessageId),

    #[error("unexpected remote response frame: {0}")]
    UnexpectedResponse(String),

    #[error("extension error: {0}")]
    Extension(#[from] ExtensionError),

    #[error("transport error: {0}")]
    Transport(#[from] TransportError),

    #[error("corrupt call record: {0}")]
    CorruptRecord(String),

    #[error("call serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("call storage error: {0}")]
    Storage(#[from] rusqlite::Error),

    #[error("database runtime error: {0}")]
    Database(#[from] DbError),

    #[error("idempotency receipt store error: {0}")]
    ReceiptStore(String),
}

/// Typed command for continuing an existing outbound call.
pub struct ReplyCallCommand<'a> {
    pub call_id: CallId,
    pub target: ApprovedPeer,
    pub endpoint: &'a PeerEndpoint,
    pub local_handle: &'a str,
    pub body: String,
    pub done: bool,
    pub timeout: Duration,
}

/// Application service orchestrating outbound and inbound agent-to-agent calls.
#[derive(Clone)]
pub struct CallsService<D, H, C, G>
where
    D: PeerDialer,
    H: AgentHarness,
    C: Clock,
    G: IdGen,
{
    repo: CallsRepo,
    dialer: D,
    extensions: ExtensionsService<H, C>,
    clock: C,
    id_gen: G,
    locks: CallTurnLocks,
}

impl<D, H, C, G> CallsService<D, H, C, G>
where
    D: PeerDialer,
    H: AgentHarness,
    C: Clock,
    G: IdGen,
{
    pub fn new(
        db: Database,
        dialer: D,
        extensions: ExtensionsService<H, C>,
        clock: C,
        id_gen: G,
    ) -> Self {
        Self {
            repo: CallsRepo::new(db),
            dialer,
            extensions,
            clock,
            id_gen,
            locks: CallTurnLocks::new(),
        }
    }

    /// Places a new call to an [`ApprovedPeer`]'s extension and waits for the remote agent's reply.
    pub async fn place_call(
        &self,
        target: ApprovedPeer,
        endpoint: &PeerEndpoint,
        local_handle: &str,
        extension: ExtensionName,
        body: String,
        timeout: Duration,
    ) -> Result<CallResponse, CallError> {
        let now = self.clock.now();
        let call_id = CallId::from_uuid(self.id_gen.next_uuid());
        let message_id = MessageId::from_uuid(self.id_gen.next_uuid());

        let record = CallRecord {
            call_id,
            peer_id: target.peer_id(),
            peer_handle: target.handle().to_string(),
            extension: extension.clone(),
            direction: CallDirection::Outbound,
            status: CallStatus::Open,
            harness_session_id: None,
            created_at: now,
            updated_at: now,
        };
        self.repo.upsert_call(&record).await?;

        let req = CallRequest {
            call_id,
            message_id,
            extension,
            body,
            expects_reply: true,
        };
        self.dispatch_outbound(endpoint, local_handle, req, timeout, false)
            .await
    }

    /// Continues an existing outbound call with a follow-up turn.
    pub async fn reply_call(&self, cmd: ReplyCallCommand<'_>) -> Result<CallResponse, CallError> {
        let existing = self
            .repo
            .get_call(cmd.call_id)
            .await?
            .ok_or(CallError::NotFound(cmd.call_id))?;

        if existing.status == CallStatus::Closed {
            return Err(CallError::CallClosed(cmd.call_id));
        }
        if existing.peer_id != cmd.target.peer_id() {
            return Err(CallError::PeerMismatch {
                call_id: cmd.call_id,
                expected: existing.peer_id,
                actual: cmd.target.peer_id(),
            });
        }

        let req = CallRequest {
            call_id: cmd.call_id,
            message_id: MessageId::from_uuid(self.id_gen.next_uuid()),
            extension: existing.extension,
            body: cmd.body,
            expects_reply: !cmd.done,
        };
        self.dispatch_outbound(cmd.endpoint, cmd.local_handle, req, cmd.timeout, cmd.done)
            .await
    }

    async fn dispatch_outbound(
        &self,
        endpoint: &PeerEndpoint,
        local_handle: &str,
        req: CallRequest,
        timeout: Duration,
        close_after: bool,
    ) -> Result<CallResponse, CallError> {
        let now = self.clock.now();
        let req_hash = req.payload_digest();
        let msg_record = CallMessageRecord {
            message_id: req.message_id,
            call_id: req.call_id,
            sender_handle: local_handle.to_string(),
            request_hash: req_hash,
            request_body: req.body.clone(),
            response: None,
            created_at: now,
        };
        self.repo.insert_message(&msg_record).await?;

        let wire_req = WireFrame::new(
            FrameKind::CallRequest,
            serde_json::to_value(&req).map_err(CallError::Serialization)?,
        );
        let wire_resp = self.dialer.request(endpoint, wire_req, timeout).await?;

        let response = decode_call_response(wire_resp)?;
        let completed_msg = CallMessageRecord {
            response: Some(response.clone()),
            ..msg_record
        };
        self.repo.insert_message(&completed_msg).await?;

        if close_after {
            self.repo
                .set_status(req.call_id, CallStatus::Closed, self.clock.now())
                .await?;
        }

        Ok(response)
    }

    /// Handles an inbound call turn. Requiring [`ApprovedPeer`] enforces authorization at compile time.
    pub async fn handle_inbound_call(
        &self,
        caller: ApprovedPeer,
        req: CallRequest,
        cancel: CancellationToken,
    ) -> Result<CallResponse, CallError> {
        let _turn_guard = self.locks.acquire(req.call_id).await;
        let request_hash = req.payload_digest();
        let msg_id = req.message_id;

        if let Some(existing_call) = self.repo.get_call(req.call_id).await? {
            if existing_call.peer_id != caller.peer_id() {
                return Err(CallError::PeerMismatch {
                    call_id: req.call_id,
                    expected: existing_call.peer_id,
                    actual: caller.peer_id(),
                });
            }
        }

        let result = run_idempotent(&self.repo, &msg_id, request_hash, || async {
            self.execute_inbound_turn(&caller, &req, request_hash, cancel)
                .await
        })
        .await;

        match result {
            Ok(resp) => Ok(resp),
            Err(IdempotentError::PayloadMismatch { .. }) => Err(CallError::PayloadMismatch(msg_id)),
            Err(IdempotentError::Store(e)) => Err(CallError::ReceiptStore(e)),
            Err(IdempotentError::Execution(e)) => Err(e),
        }
    }

    async fn execute_inbound_turn(
        &self,
        caller: &ApprovedPeer,
        req: &CallRequest,
        request_hash: crate::primitives::Digest,
        cancel: CancellationToken,
    ) -> Result<CallResponse, CallError> {
        let now = self.clock.now();
        let existing_call = self.repo.get_call(req.call_id).await?;
        let existing_session_id = existing_call
            .as_ref()
            .and_then(|c| c.harness_session_id.clone());

        let record = CallRecord {
            call_id: req.call_id,
            peer_id: caller.peer_id(),
            peer_handle: caller.handle().to_string(),
            extension: req.extension.clone(),
            direction: CallDirection::Inbound,
            status: if req.expects_reply {
                CallStatus::Open
            } else {
                CallStatus::Closed
            },
            harness_session_id: existing_session_id.clone(),
            created_at: existing_call.map(|c| c.created_at).unwrap_or(now),
            updated_at: now,
        };
        self.repo.upsert_call(&record).await?;

        self.repo
            .insert_message(&CallMessageRecord {
                message_id: req.message_id,
                call_id: req.call_id,
                sender_handle: caller.handle().to_string(),
                request_hash,
                request_body: req.body.clone(),
                response: None,
                created_at: now,
            })
            .await?;

        let delivery = self
            .extensions
            .deliver(
                &req.extension,
                req.call_id.as_uuid(),
                caller.handle().to_string(),
                req.body.clone(),
                existing_session_id,
                cancel,
            )
            .await;

        let response = match delivery {
            Ok(DeliveryOutcome::Replied { body, session_id }) => {
                if let Some(sid) = session_id {
                    self.repo
                        .update_session_id(req.call_id, sid, self.clock.now())
                        .await?;
                }
                CallResponse {
                    call_id: req.call_id,
                    message_id: req.message_id,
                    status: CallResponseStatus::Replied,
                    body,
                }
            }
            Ok(DeliveryOutcome::TimedOut) => CallResponse {
                call_id: req.call_id,
                message_id: req.message_id,
                status: CallResponseStatus::TimedOut,
                body: "remote agent turn timed out".to_string(),
            },
            Ok(DeliveryOutcome::Cancelled) => CallResponse {
                call_id: req.call_id,
                message_id: req.message_id,
                status: CallResponseStatus::Cancelled,
                body: "remote agent turn cancelled".to_string(),
            },
            Ok(DeliveryOutcome::ProcessFailed { exit_code, detail }) => CallResponse {
                call_id: req.call_id,
                message_id: req.message_id,
                status: CallResponseStatus::HarnessFailed,
                body: format!("agent harness failed (exit {exit_code:?}): {detail}"),
            },
            Err(ExtensionError::NotFound(name)) => CallResponse {
                call_id: req.call_id,
                message_id: req.message_id,
                status: CallResponseStatus::ExtensionNotFound,
                body: format!("target '{name}' not found"),
            },
            Err(ExtensionError::SessionNotFound(query)) => CallResponse {
                call_id: req.call_id,
                message_id: req.message_id,
                status: CallResponseStatus::ExtensionNotFound,
                body: format!("no pre-existing agent session found matching '{query}'"),
            },
            Err(ExtensionError::AmbiguousSession(query, count)) => CallResponse {
                call_id: req.call_id,
                message_id: req.message_id,
                status: CallResponseStatus::HarnessFailed,
                body: format!(
                    "multiple pre-existing agent sessions ({count}) match '{query}'; specify the session UUID"
                ),
            },
            Err(ExtensionError::SessionRequired(name)) => CallResponse {
                call_id: req.call_id,
                message_id: req.message_id,
                status: CallResponseStatus::HarnessFailed,
                body: format!(
                    "extension '{name}' has no pre-existing session bound; call <peer>.<harness>.<conversation-name-or-id> directly"
                ),
            },
            Err(err) => return Err(CallError::Extension(err)),
        };

        Ok(response)
    }

    pub async fn get_call(&self, call_id: CallId) -> Result<CallRecord, CallError> {
        self.repo
            .get_call(call_id)
            .await?
            .ok_or(CallError::NotFound(call_id))
    }

    pub async fn history(
        &self,
        call_id: CallId,
    ) -> Result<(CallRecord, Vec<CallMessageRecord>), CallError> {
        let call = self.get_call(call_id).await?;
        let messages = self.repo.list_messages(call_id).await?;
        Ok((call, messages))
    }
}

fn decode_call_response(frame: WireFrame) -> Result<CallResponse, CallError> {
    match frame.kind {
        FrameKind::CallResponse => {
            serde_json::from_value(frame.payload).map_err(CallError::Serialization)
        }
        FrameKind::Error => {
            let msg = frame
                .payload
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("remote error");
            Err(CallError::UnexpectedResponse(msg.to_string()))
        }
        other => Err(CallError::UnexpectedResponse(format!(
            "expected CallResponse, got {other:?}"
        ))),
    }
}
