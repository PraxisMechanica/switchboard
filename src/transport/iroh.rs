//! Production QUIC/P2P transport backed by `iroh` 1.5.

use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use ::iroh::endpoint::{
    presets, ConnectError, ConnectWithOptsError, Incoming, PortmapperConfig, RecvStream, SendStream,
};
use ::iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::identity::{LocalKeypair, PeerId};

use super::frame::{encode_frame, read_frame, write_frame, WireFrame};
use super::{
    InboundExchange, NetworkMode, PeerDialer, PeerEndpoint, PeerListener, TransportError,
    PROTOCOL_ALPN,
};

const INBOUND_CHANNEL_CAPACITY: usize = 256;

/// Live `iroh` QUIC transport implementing both [`PeerDialer`] and [`PeerListener`].
pub struct IrohTransport {
    local_peer_id: PeerId,
    endpoint: Endpoint,
    inbound_rx: Mutex<mpsc::Receiver<InboundExchange>>,
    accept_task: Mutex<Option<JoinHandle<()>>>,
    shutdown: CancellationToken,
}

impl fmt::Debug for IrohTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IrohTransport")
            .field("local_peer_id", &self.local_peer_id)
            .finish_non_exhaustive()
    }
}

impl IrohTransport {
    /// Binds a new `iroh` endpoint authenticated with the local Ed25519 keypair.
    pub async fn bind(keypair: &LocalKeypair, mode: NetworkMode) -> Result<Self, TransportError> {
        let secret_key = SecretKey::from_bytes(keypair.expose_secret_seed());
        let expected_peer_id = keypair.peer_id();
        if secret_key.public().as_bytes() != expected_peer_id.as_bytes() {
            return Err(TransportError::InvalidPeerId);
        }

        let builder = match mode {
            NetworkMode::GlobalP2p => Endpoint::builder(presets::N0),
            NetworkMode::VpnOnly {
                bind_addr,
                custom_relays,
            } => {
                let mut builder = Endpoint::builder(presets::Minimal)
                    .portmapper_config(PortmapperConfig::Disabled);

                if let Some(addr) = bind_addr {
                    builder = builder
                        .clear_ip_transports()
                        .bind_addr(addr)
                        .map_err(|err| TransportError::Bind(err.to_string()))?;
                }

                if custom_relays.is_empty() {
                    builder = builder.relay_mode(RelayMode::Disabled);
                } else {
                    let mut parsed = Vec::with_capacity(custom_relays.len());
                    for raw in custom_relays {
                        let url: RelayUrl = raw.parse().map_err(|err| {
                            TransportError::Bind(format!("invalid custom relay url '{raw}': {err}"))
                        })?;
                        parsed.push(url);
                    }
                    builder = builder.relay_mode(RelayMode::custom(parsed));
                }

                builder
            }
        };

        let endpoint = builder
            .secret_key(secret_key)
            .alpns(vec![PROTOCOL_ALPN.to_vec()])
            .bind()
            .await
            .map_err(|err| TransportError::Bind(err.to_string()))?;

        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CHANNEL_CAPACITY);
        let shutdown = CancellationToken::new();

        let accept_task = tokio::spawn(accept_loop(endpoint.clone(), inbound_tx, shutdown.clone()));

        debug!(peer = %expected_peer_id.short(), "iroh transport bound");

        Ok(Self {
            local_peer_id: expected_peer_id,
            endpoint,
            inbound_rx: Mutex::new(inbound_rx),
            accept_task: Mutex::new(Some(accept_task)),
            shutdown,
        })
    }

    /// Returns the currently advertised local endpoint descriptor (direct sockets and relay URL).
    pub fn local_endpoint(&self) -> PeerEndpoint {
        let addr = self.endpoint.addr();
        let mut direct_addrs: Vec<SocketAddr> = addr.ip_addrs().copied().collect();

        for sock in self.endpoint.bound_sockets() {
            if !sock.ip().is_unspecified() && !direct_addrs.contains(&sock) {
                direct_addrs.push(sock);
            }
        }

        if direct_addrs.is_empty() {
            direct_addrs.extend(self.endpoint.bound_sockets());
        }

        let relay_url = addr.relay_urls().next().map(ToString::to_string);

        PeerEndpoint {
            peer_id: self.local_peer_id,
            direct_addrs,
            relay_url,
        }
    }

    /// Gracefully stops accepting new connections and closes the underlying QUIC endpoint.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        self.endpoint.close().await;

        let handle = self.accept_task.lock().await.take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }
}

impl Drop for IrohTransport {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[async_trait::async_trait]
impl PeerDialer for IrohTransport {
    fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    async fn request(
        &self,
        target: &PeerEndpoint,
        frame: WireFrame,
        timeout: Duration,
    ) -> Result<WireFrame, TransportError> {
        if self.shutdown.is_cancelled() || self.endpoint.is_closed() {
            return Err(TransportError::Closed);
        }

        // Validate wire version and maximum frame size before opening a network connection.
        let _ = encode_frame(&frame)?;
        let endpoint_addr = endpoint_addr_from_peer(target)?;
        let endpoint = self.endpoint.clone();

        tokio::time::timeout(timeout, async move {
            let conn = endpoint
                .connect(endpoint_addr, PROTOCOL_ALPN)
                .await
                .map_err(map_connect_error)?;

            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|err| TransportError::Connect(err.to_string()))?;

            write_frame(&mut send, &frame).await?;
            send.finish().map_err(|_| TransportError::Closed)?;

            let response = read_frame(&mut recv).await?;
            conn.close(0u32.into(), b"done");
            Ok(response)
        })
        .await
        .map_err(|_| TransportError::TimedOut)?
    }
}

#[async_trait::async_trait]
impl PeerListener for IrohTransport {
    async fn accept(&self) -> Result<InboundExchange, TransportError> {
        let mut rx = self.inbound_rx.lock().await;
        rx.recv().await.ok_or(TransportError::Closed)
    }
}

fn endpoint_addr_from_peer(target: &PeerEndpoint) -> Result<EndpointAddr, TransportError> {
    let endpoint_id = EndpointId::from_bytes(target.peer_id.as_bytes())
        .map_err(|_| TransportError::InvalidPeerId)?;
    let mut addr = EndpointAddr::new(endpoint_id);

    for &sock in &target.direct_addrs {
        addr = addr.with_ip_addr(sock);
    }

    if let Some(ref raw_relay) = target.relay_url {
        let relay: RelayUrl = raw_relay
            .parse()
            .map_err(|err| TransportError::Connect(format!("invalid relay url: {err}")))?;
        addr = addr.with_relay_url(relay);
    }

    Ok(addr)
}

fn map_connect_error(err: ConnectError) -> TransportError {
    match &err {
        ConnectError::Connect {
            source: ConnectWithOptsError::EndpointClosed { .. },
            ..
        } => TransportError::Closed,
        ConnectError::Connect {
            source: ConnectWithOptsError::NoAddress { .. },
            ..
        } => TransportError::Unavailable(err.to_string()),
        _ => TransportError::Connect(err.to_string()),
    }
}

async fn accept_loop(
    endpoint: Endpoint,
    inbound_tx: mpsc::Sender<InboundExchange>,
    shutdown: CancellationToken,
) {
    loop {
        let incoming = tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            incoming = endpoint.accept() => {
                match incoming {
                    Some(inc) => inc,
                    None => break,
                }
            }
        };

        let tx = inbound_tx.clone();
        let child_shutdown = shutdown.child_token();
        tokio::spawn(async move {
            handle_incoming(incoming, tx, child_shutdown).await;
        });
    }
}

async fn handle_incoming(
    incoming: Incoming,
    inbound_tx: mpsc::Sender<InboundExchange>,
    shutdown: CancellationToken,
) {
    let conn = match incoming.await {
        Ok(conn) => conn,
        Err(err) => {
            debug!(error = %err, "failed to complete incoming QUIC handshake");
            return;
        }
    };

    let remote_peer = match PeerId::from_bytes(*conn.remote_id().as_bytes()) {
        Ok(peer) => peer,
        Err(_) => {
            conn.close(1u32.into(), b"invalid peer id");
            return;
        }
    };

    loop {
        let (send, recv) = tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                conn.close(0u32.into(), b"shutdown");
                break;
            }
            bi = conn.accept_bi() => {
                match bi {
                    Ok(pair) => pair,
                    Err(err) => {
                        debug!(peer = %remote_peer.short(), error = %err, "incoming connection closed");
                        break;
                    }
                }
            }
        };

        let tx = inbound_tx.clone();
        let stream_shutdown = shutdown.child_token();
        tokio::spawn(async move {
            handle_stream(remote_peer, send, recv, tx, stream_shutdown).await;
        });
    }
}

async fn handle_stream(
    remote_peer: PeerId,
    mut send: SendStream,
    mut recv: RecvStream,
    inbound_tx: mpsc::Sender<InboundExchange>,
    shutdown: CancellationToken,
) {
    let request = match read_frame(&mut recv).await {
        Ok(frame) => frame,
        Err(err) => {
            debug!(peer = %remote_peer.short(), error = %err, "failed to decode inbound frame");
            let _ = send.reset(1u32.into());
            return;
        }
    };

    let frame_kind = request.kind;
    let (resp_tx, resp_rx) = oneshot::channel();
    let exchange = InboundExchange::new(remote_peer, request, resp_tx);

    let delivered = tokio::select! {
        biased;
        () = shutdown.cancelled() => false,
        res = inbound_tx.send(exchange) => res.is_ok(),
    };

    if !delivered {
        let _ = send.reset(2u32.into());
        return;
    }

    let response = tokio::select! {
        biased;
        () = shutdown.cancelled() => {
            let _ = send.reset(0u32.into());
            return;
        }
        res = resp_rx => {
            match res {
                Ok(frame) => frame,
                Err(_) => {
                    debug!(peer = %remote_peer.short(), kind = ?frame_kind, "exchange dropped without response");
                    let _ = send.reset(3u32.into());
                    return;
                }
            }
        }
    };

    if let Err(err) = write_frame(&mut send, &response).await {
        debug!(peer = %remote_peer.short(), kind = ?response.kind, error = %err, "failed to send response frame");
        let _ = send.reset(4u32.into());
        return;
    }

    let _ = send.finish();
}
