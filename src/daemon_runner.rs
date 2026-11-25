//! Resident daemon runner wiring `RuntimeTransport`, `GitPhonebook`, `CodexHarness`, and Unix IPC.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use switchboard::config::{SwitchboardConfig, TransportKind};
use switchboard::daemon::SwitchboardNode;
use switchboard::db::Database;
use switchboard::directory::{
    DirectoryError, DirectoryPort, GitPhonebook, MemoryDirectory, PeerCard,
};
use switchboard::extensions::{AgyHarness, CodexHarness, MultiHarness};
use switchboard::identity::{IdentityService, LocalKeypair, PeerId, Signed};
use switchboard::ipc::{self, IpcRequest, IpcResponse};
use switchboard::peers::DesktopNotifier;
use switchboard::primitives::{SystemClock, UuidV7Gen};
use switchboard::transport::{
    InboundExchange, IrohTransport, NetworkMode, PeerDialer, PeerEndpoint, PeerListener,
    TransportError, WireFrame,
};

type ProductionNode = SwitchboardNode<
    RuntimeTransport,
    DaemonDirectory,
    MultiHarness,
    SystemClock,
    UuidV7Gen,
    DesktopNotifier,
>;

pub async fn run_daemon(cfg: SwitchboardConfig, db: Database) -> Result<()> {
    let id_svc = IdentityService::new(db.clone(), SystemClock);
    let local = id_svc.load_required().await?;
    let transport =
        RuntimeTransport::bind(cfg.transport, &local.keypair, cfg.network.clone()).await?;
    let local_ep = transport.local_endpoint();

    let dir_port = match &cfg.directory.git_remote {
        Some(remote) => {
            DaemonDirectory::Git(GitPhonebook::new(remote.clone(), cfg.phonebook_dir()))
        }
        None => DaemonDirectory::Memory(MemoryDirectory::new()),
    };

    let node = Arc::new(SwitchboardNode::new(
        db,
        transport,
        dir_port,
        MultiHarness::new(CodexHarness::new(cfg.codex_config()), AgyHarness::default()),
        SystemClock,
        UuidV7Gen,
        DesktopNotifier,
    ));

    let cancel = CancellationToken::new();
    let ipc_listener = ipc::bind_socket(&cfg.socket_path())?;
    tracing::info!(
        handle = %local.handle,
        peer_id = %local_ep.peer_id,
        socket = %cfg.socket_path().display(),
        "switchboard daemon started"
    );

    let p2p_node = Arc::clone(&node);
    let p2p_cancel = cancel.child_token();
    let p2p_task = tokio::spawn(async move { p2p_node.run_accept_loop(p2p_cancel).await });

    let direct_addrs: Vec<String> = local_ep
        .direct_addrs
        .iter()
        .map(ToString::to_string)
        .collect();
    let peer_id_hex = local_ep.peer_id.to_hex();

    if cfg.directory.git_remote.is_some() {
        let sync_node = Arc::clone(&node);
        let sync_addrs = direct_addrs.clone();
        let sync_cancel = cancel.child_token();
        tokio::spawn(async move {
            let _ = sync_node.publish_directory_card(sync_addrs).await;
            let _ = sync_node.directory.sync().await;
            loop {
                tokio::select! {
                    _ = sync_cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(300)) => {
                        let _ = sync_node.directory.sync().await;
                    }
                }
            }
        });
    }

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutdown signal received");
                cancel.cancel();
                break;
            }
            accepted = ipc_listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let node = Arc::clone(&node);
                let pid = peer_id_hex.clone();
                let addrs = direct_addrs.clone();
                tokio::spawn(async move {
                    ipc::serve_connection(stream, |req| handle_ipc(node, pid, addrs, req)).await;
                });
            }
        }
    }

    let _ = p2p_task.await;
    let _ = std::fs::remove_file(cfg.socket_path());
    Ok(())
}

async fn handle_ipc(
    node: Arc<ProductionNode>,
    peer_id: String,
    direct_addrs: Vec<String>,
    req: IpcRequest,
) -> IpcResponse {
    match req {
        IpcRequest::Ping => IpcResponse::Pong {
            peer_id,
            direct_addrs,
        },
        IpcRequest::Knock {
            target,
            reason,
            timeout_secs,
        } => match node
            .knock_peer(&target, reason, None, Duration::from_secs(timeout_secs))
            .await
        {
            Ok(peer) => IpcResponse::PeerUpdated { peer },
            Err(e) => IpcResponse::Error {
                message: e.to_string(),
            },
        },
        IpcRequest::Approve {
            handle,
            timeout_secs,
        } => match node
            .approve_peer(&handle, None, Duration::from_secs(timeout_secs))
            .await
        {
            Ok(peer) => IpcResponse::PeerUpdated { peer },
            Err(e) => IpcResponse::Error {
                message: e.to_string(),
            },
        },
        IpcRequest::Reject {
            handle,
            timeout_secs,
        } => match node
            .reject_peer(&handle, None, Duration::from_secs(timeout_secs))
            .await
        {
            Ok(peer) => IpcResponse::PeerUpdated { peer },
            Err(e) => IpcResponse::Error {
                message: e.to_string(),
            },
        },
        IpcRequest::Call {
            target,
            message,
            timeout_secs,
        } => match node
            .call_extension(&target, message, None, Duration::from_secs(timeout_secs))
            .await
        {
            Ok(response) => IpcResponse::CallResult { response },
            Err(e) => IpcResponse::Error {
                message: e.to_string(),
            },
        },
        IpcRequest::Reply {
            call_id,
            message,
            done,
            timeout_secs,
        } => match node
            .reply_call(
                call_id,
                message,
                done,
                None,
                Duration::from_secs(timeout_secs),
            )
            .await
        {
            Ok(response) => IpcResponse::CallResult { response },
            Err(e) => IpcResponse::Error {
                message: e.to_string(),
            },
        },
    }
}

enum DaemonDirectory {
    Git(GitPhonebook),
    Memory(MemoryDirectory),
}

#[async_trait::async_trait]
impl DirectoryPort for DaemonDirectory {
    async fn publish(&self, card: Signed<PeerCard>) -> std::result::Result<(), DirectoryError> {
        match self {
            Self::Git(g) => g.publish(card).await,
            Self::Memory(m) => m.publish(card).await,
        }
    }

    async fn fetch_all(&self) -> std::result::Result<Vec<Signed<PeerCard>>, DirectoryError> {
        match self {
            Self::Git(g) => g.fetch_all().await,
            Self::Memory(m) => m.fetch_all().await,
        }
    }
}

enum RuntimeTransport {
    Iroh(IrohTransport),
}

impl RuntimeTransport {
    async fn bind(kind: TransportKind, keypair: &LocalKeypair, mode: NetworkMode) -> Result<Self> {
        match kind {
            TransportKind::Iroh => Ok(Self::Iroh(IrohTransport::bind(keypair, mode).await?)),
        }
    }

    fn local_endpoint(&self) -> PeerEndpoint {
        match self {
            Self::Iroh(transport) => transport.local_endpoint(),
        }
    }
}

#[async_trait::async_trait]
impl PeerDialer for RuntimeTransport {
    fn local_peer_id(&self) -> PeerId {
        match self {
            Self::Iroh(transport) => transport.local_peer_id(),
        }
    }

    async fn request(
        &self,
        target: &PeerEndpoint,
        frame: WireFrame,
        timeout: Duration,
    ) -> std::result::Result<WireFrame, TransportError> {
        match self {
            Self::Iroh(transport) => transport.request(target, frame, timeout).await,
        }
    }
}

#[async_trait::async_trait]
impl PeerListener for RuntimeTransport {
    async fn accept(&self) -> std::result::Result<InboundExchange, TransportError> {
        match self {
            Self::Iroh(transport) => transport.accept().await,
        }
    }
}
