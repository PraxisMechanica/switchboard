//! Peer-to-peer transport abstraction, wire framing, and implementations (`iroh` + in-memory).

pub mod conformance;
pub mod frame;
pub mod iroh;
pub mod memory;

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::oneshot;

use crate::identity::PeerId;

pub use self::iroh::IrohTransport;
pub use frame::{decode_frame, encode_frame, read_frame, write_frame, FrameKind, WireFrame};
pub use memory::{MemoryHub, MemoryTransport};

/// ALPN protocol identifier negotiated over QUIC/TLS for switchboard traffic.
pub const PROTOCOL_ALPN: &[u8] = b"praxis/switchboard/1";

/// Current wire framing protocol version.
pub const PROTOCOL_VERSION: u16 = 1;

/// Maximum permitted serialized JSON body length for a single wire frame (4 MiB).
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Failure modes produced by transport binding, dialing, listening, and framing.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("transport bind failed: {0}")]
    Bind(String),

    #[error("transport connect failed: {0}")]
    Connect(String),

    #[error("invalid peer id")]
    InvalidPeerId,

    #[error("invalid wire frame: {0}")]
    InvalidFrame(String),

    #[error("unsupported wire protocol version: {0}")]
    UnsupportedVersion(u16),

    #[error("wire frame exceeds maximum size ({0} bytes > {MAX_FRAME_BYTES} bytes)")]
    FrameTooLarge(usize),

    #[error("transport operation timed out")]
    TimedOut,

    #[error("peer unavailable: {0}")]
    Unavailable(String),

    #[error("transport closed")]
    Closed,

    #[error("transport I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Network reachability profile for an [`IrohTransport`] endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum NetworkMode {
    /// Use public n0 discovery and relay infrastructure for global P2P reachability.
    #[default]
    GlobalP2p,

    /// Restrict networking to private/VPN paths without public discovery or public relays.
    VpnOnly {
        bind_addr: Option<SocketAddr>,
        custom_relays: Vec<String>,
    },
}

/// Addressing descriptor for reaching a remote switchboard peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerEndpoint {
    pub peer_id: PeerId,
    pub direct_addrs: Vec<SocketAddr>,
    pub relay_url: Option<String>,
}

impl PeerEndpoint {
    /// Creates an endpoint descriptor containing only the peer's cryptographic identity.
    pub fn from_peer_id(peer_id: PeerId) -> Self {
        Self {
            peer_id,
            direct_addrs: Vec::new(),
            relay_url: None,
        }
    }
}

/// Authenticated inbound request paired with a one-shot response channel.
pub struct InboundExchange {
    pub remote_peer: PeerId,
    pub request: WireFrame,
    responder: oneshot::Sender<WireFrame>,
}

impl InboundExchange {
    pub(crate) fn new(
        remote_peer: PeerId,
        request: WireFrame,
        responder: oneshot::Sender<WireFrame>,
    ) -> Self {
        Self {
            remote_peer,
            request,
            responder,
        }
    }

    /// Constructs an inbound exchange and its paired response receiver for unit tests.
    pub fn new_for_test(
        remote_peer: PeerId,
        request: WireFrame,
    ) -> (Self, oneshot::Receiver<WireFrame>) {
        let (tx, rx) = oneshot::channel();
        (Self::new(remote_peer, request, tx), rx)
    }

    /// Sends the single reply frame back to the initiating peer.
    pub fn respond(self, frame: WireFrame) -> Result<(), TransportError> {
        self.responder
            .send(frame)
            .map_err(|_| TransportError::Closed)
    }
}

impl fmt::Debug for InboundExchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InboundExchange")
            .field("remote_peer", &self.remote_peer)
            .field("version", &self.request.version)
            .field("kind", &self.request.kind)
            .finish_non_exhaustive()
    }
}

/// Outbound request/response capability for dialing remote switchboard peers.
#[async_trait::async_trait]
pub trait PeerDialer: Send + Sync {
    fn local_peer_id(&self) -> PeerId;

    async fn request(
        &self,
        target: &PeerEndpoint,
        frame: WireFrame,
        timeout: Duration,
    ) -> Result<WireFrame, TransportError>;
}

/// Inbound request acceptance capability for serving switchboard traffic.
#[async_trait::async_trait]
pub trait PeerListener: Send + Sync {
    async fn accept(&self) -> Result<InboundExchange, TransportError>;
}

#[async_trait::async_trait]
impl<T: PeerDialer + ?Sized> PeerDialer for Arc<T> {
    fn local_peer_id(&self) -> PeerId {
        (**self).local_peer_id()
    }

    async fn request(
        &self,
        target: &PeerEndpoint,
        frame: WireFrame,
        timeout: Duration,
    ) -> Result<WireFrame, TransportError> {
        (**self).request(target, frame, timeout).await
    }
}

#[async_trait::async_trait]
impl<T: PeerListener + ?Sized> PeerListener for Arc<T> {
    async fn accept(&self) -> Result<InboundExchange, TransportError> {
        (**self).accept().await
    }
}
