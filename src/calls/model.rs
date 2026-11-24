//! Data models and deterministic payload digest for the calls vertical slice.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::extensions::ExtensionName;
use crate::identity::PeerId;
use crate::primitives::{Digest, Timestamp};

use super::CallError;

/// Unique identifier for a multi-turn agent-to-agent call thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CallId(Uuid);

impl CallId {
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for CallId {
    type Err = CallError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s.trim())
            .map(Self)
            .map_err(|_| CallError::InvalidId(s.to_string()))
    }
}

/// Unique identifier for a single turn inside a call, serving as its idempotency key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(Uuid);

impl MessageId {
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for MessageId {
    type Err = CallError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s.trim())
            .map(Self)
            .map_err(|_| CallError::InvalidId(s.to_string()))
    }
}

/// Lifecycle state of a call thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallStatus {
    Open,
    Closed,
}

impl CallStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }

    pub fn parse_str(raw: &str) -> Result<Self, CallError> {
        match raw {
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            other => Err(CallError::CorruptRecord(format!("invalid status: {other}"))),
        }
    }
}

/// Direction of a call relative to the local node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallDirection {
    Outbound,
    Inbound,
}

impl CallDirection {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Outbound => "outbound",
            Self::Inbound => "inbound",
        }
    }

    pub fn parse_str(raw: &str) -> Result<Self, CallError> {
        match raw {
            "outbound" => Ok(Self::Outbound),
            "inbound" => Ok(Self::Inbound),
            other => Err(CallError::CorruptRecord(format!(
                "invalid direction: {other}"
            ))),
        }
    }
}

/// Wire request for starting or continuing an agent-to-agent call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallRequest {
    pub call_id: CallId,
    pub message_id: MessageId,
    pub extension: ExtensionName,
    pub body: String,
    pub expects_reply: bool,
}

impl CallRequest {
    /// Deterministic SHA-256 fingerprint used to detect mismatched idempotency replays.
    pub fn payload_digest(&self) -> Digest {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"switchboard.calls.request.v1\0");
        buf.extend_from_slice(self.call_id.as_uuid().as_bytes());
        let ext = self.extension.as_str().as_bytes();
        buf.extend_from_slice(&(ext.len() as u32).to_be_bytes());
        buf.extend_from_slice(ext);
        let body = self.body.as_bytes();
        buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
        buf.extend_from_slice(body);
        buf.push(u8::from(self.expects_reply));
        Digest::sha256(&buf)
    }
}

/// Normalized outcome of handling an inbound call turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallResponseStatus {
    Replied,
    TimedOut,
    Cancelled,
    HarnessFailed,
    ExtensionNotFound,
}

/// Wire response returned to the calling node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallResponse {
    pub call_id: CallId,
    pub message_id: MessageId,
    pub status: CallResponseStatus,
    pub body: String,
}

/// Persisted metadata for a call thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallRecord {
    pub call_id: CallId,
    pub peer_id: PeerId,
    pub peer_handle: String,
    pub extension: ExtensionName,
    pub direction: CallDirection,
    pub status: CallStatus,
    pub harness_session_id: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Persisted single turn in a call's transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallMessageRecord {
    pub message_id: MessageId,
    pub call_id: CallId,
    pub sender_handle: String,
    pub request_hash: Digest,
    pub request_body: String,
    pub response: Option<CallResponse>,
    pub created_at: Timestamp,
}
