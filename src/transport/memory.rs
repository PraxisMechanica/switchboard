//! Deterministic in-memory transport hub for fast unit and conformance testing.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, Mutex};

use crate::identity::PeerId;

use super::frame::{encode_frame, WireFrame};
use super::{InboundExchange, PeerDialer, PeerEndpoint, PeerListener, TransportError};

const MEMORY_CHANNEL_CAPACITY: usize = 256;

struct PeerSlot {
    registration_id: u64,
    tx: mpsc::Sender<InboundExchange>,
}

/// Shared in-memory switchboard router connecting registered [`MemoryTransport`] peers.
#[derive(Clone, Default)]
pub struct MemoryHub {
    peers: Arc<RwLock<HashMap<PeerId, PeerSlot>>>,
    next_id: Arc<AtomicU64>,
}

impl fmt::Debug for MemoryHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self
            .peers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len();
        f.debug_struct("MemoryHub")
            .field("registered_peers", &count)
            .finish()
    }
}

impl MemoryHub {
    /// Creates an empty in-memory transport hub.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a peer with the hub and returns its paired [`MemoryTransport`].
    pub fn register(&self, peer_id: PeerId) -> MemoryTransport {
        let (tx, rx) = mpsc::channel(MEMORY_CHANNEL_CAPACITY);
        let registration_id = self.next_id.fetch_add(1, Ordering::Relaxed);

        self.peers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                peer_id,
                PeerSlot {
                    registration_id,
                    tx,
                },
            );

        MemoryTransport {
            local_peer_id: peer_id,
            registration_id,
            hub: self.clone(),
            inbound_rx: Mutex::new(rx),
            closed: AtomicBool::new(false),
        }
    }

    /// Removes a peer from the hub so subsequent dials fail as unavailable.
    pub fn unregister(&self, peer_id: PeerId) {
        self.peers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&peer_id);
    }

    fn unregister_matching(&self, peer_id: PeerId, registration_id: u64) {
        let mut guard = self
            .peers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard
            .get(&peer_id)
            .is_some_and(|slot| slot.registration_id == registration_id)
        {
            guard.remove(&peer_id);
        }
    }

    fn lookup(&self, peer_id: PeerId) -> Option<mpsc::Sender<InboundExchange>> {
        self.peers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&peer_id)
            .map(|slot| slot.tx.clone())
    }
}

/// Deterministic in-memory peer transport attached to a [`MemoryHub`].
pub struct MemoryTransport {
    local_peer_id: PeerId,
    registration_id: u64,
    hub: MemoryHub,
    inbound_rx: Mutex<mpsc::Receiver<InboundExchange>>,
    closed: AtomicBool,
}

impl fmt::Debug for MemoryTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryTransport")
            .field("local_peer_id", &self.local_peer_id)
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .finish()
    }
}

impl MemoryTransport {
    /// Returns this transport's addressable [`PeerEndpoint`].
    pub fn local_endpoint(&self) -> PeerEndpoint {
        PeerEndpoint::from_peer_id(self.local_peer_id)
    }

    /// Closes this transport and unregisters it from its [`MemoryHub`].
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.hub
            .unregister_matching(self.local_peer_id, self.registration_id);
    }
}

impl Drop for MemoryTransport {
    fn drop(&mut self) {
        self.close();
    }
}

#[async_trait::async_trait]
impl PeerDialer for MemoryTransport {
    fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    async fn request(
        &self,
        target: &PeerEndpoint,
        frame: WireFrame,
        timeout: Duration,
    ) -> Result<WireFrame, TransportError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }

        // Enforce identical wire framing invariants as the QUIC transport.
        let _ = encode_frame(&frame)?;

        let target_tx = self.hub.lookup(target.peer_id).ok_or_else(|| {
            TransportError::Unavailable(format!(
                "peer {} is not registered in memory hub",
                target.peer_id.short()
            ))
        })?;

        let (resp_tx, resp_rx) = oneshot::channel();
        let exchange = InboundExchange::new(self.local_peer_id, frame, resp_tx);

        tokio::time::timeout(timeout, async move {
            target_tx.send(exchange).await.map_err(|_| {
                TransportError::Unavailable(format!(
                    "peer {} listener closed",
                    target.peer_id.short()
                ))
            })?;

            let response = resp_rx.await.map_err(|_| TransportError::Closed)?;
            let _ = encode_frame(&response)?;
            Ok(response)
        })
        .await
        .map_err(|_| TransportError::TimedOut)?
    }
}

#[async_trait::async_trait]
impl PeerListener for MemoryTransport {
    async fn accept(&self) -> Result<InboundExchange, TransportError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }

        let mut rx = self.inbound_rx.lock().await;
        rx.recv().await.ok_or(TransportError::Closed)
    }
}
