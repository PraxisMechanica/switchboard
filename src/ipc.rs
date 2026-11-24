//! Thin Unix-domain-socket JSON-lines IPC between the stateless CLI and resident daemon.

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::calls::{CallId, CallResponse};
use crate::peers::Peer;

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("daemon is not running (failed to connect to {path}): {source}")]
    Connect {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("ipc I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("ipc serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("daemon returned error: {0}")]
    Remote(String),
}

/// Requests sent from local CLI invocations to the running daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IpcRequest {
    Ping,
    Knock {
        target: String,
        reason: String,
        timeout_secs: u64,
    },
    Approve {
        handle: String,
        timeout_secs: u64,
    },
    Reject {
        handle: String,
        timeout_secs: u64,
    },
    Call {
        target: String,
        message: String,
        timeout_secs: u64,
    },
    Reply {
        call_id: CallId,
        message: String,
        done: bool,
        timeout_secs: u64,
    },
}

/// Responses returned by the resident daemon over the local Unix socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IpcResponse {
    Pong {
        peer_id: String,
        direct_addrs: Vec<String>,
    },
    PeerUpdated {
        peer: Peer,
    },
    CallResult {
        response: CallResponse,
    },
    Error {
        message: String,
    },
}

/// Sends a single IPC request to the local daemon and awaits its response.
pub async fn send_request(socket_path: &Path, req: &IpcRequest) -> Result<IpcResponse, IpcError> {
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(|source| IpcError::Connect {
            path: socket_path.display().to_string(),
            source,
        })?;
    let (read_half, mut write_half) = stream.into_split();
    let mut encoded = serde_json::to_string(req)?;
    encoded.push('\n');
    write_half.write_all(encoded.as_bytes()).await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Err(IpcError::Remote("daemon closed connection".to_string()));
    }
    let resp: IpcResponse = serde_json::from_str(&line)?;
    Ok(resp)
}

/// Binds a Unix socket listener, removing any stale socket file first.
pub fn bind_socket(socket_path: &Path) -> Result<UnixListener, IpcError> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if socket_path.exists() {
        let _ = std::fs::remove_file(socket_path);
    }
    Ok(UnixListener::bind(socket_path)?)
}

/// Reads one JSON-line request, invokes `handler`, and writes the JSON-line response.
pub async fn serve_connection<F, Fut>(stream: UnixStream, handler: F)
where
    F: FnOnce(IpcRequest) -> Fut,
    Fut: std::future::Future<Output = IpcResponse>,
{
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return;
    }

    let response = match serde_json::from_str::<IpcRequest>(&line) {
        Ok(req) => handler(req).await,
        Err(err) => IpcResponse::Error {
            message: format!("invalid ipc request: {err}"),
        },
    };

    if let Ok(mut encoded) = serde_json::to_string(&response) {
        encoded.push('\n');
        let _ = write_half.write_all(encoded.as_bytes()).await;
        let _ = write_half.flush().await;
    }
}
