//! CLI definitions for the calls vertical slice.

use clap::Args;

/// Arguments for `switchboard call <peer>/<extension> <message>`.
#[derive(Debug, Args)]
pub struct CallArgs {
    /// Target in `<peer_handle>/<extension>` format (for example `bob/api`).
    pub target: String,

    /// Message payload delivered to the remote agent.
    pub message: String,

    /// Timeout in seconds waiting for the remote agent to respond.
    #[arg(long, default_value_t = 180)]
    pub timeout: u64,

    /// Output machine-readable JSON.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Arguments for `switchboard reply <call_id> <message>`.
#[derive(Debug, Args)]
pub struct ReplyArgs {
    /// Existing call identifier to continue.
    pub call_id: String,

    /// Follow-up message payload.
    pub message: String,

    /// Close the call after sending this turn.
    #[arg(long, default_value_t = false)]
    pub done: bool,

    /// Timeout in seconds waiting for the remote agent to respond.
    #[arg(long, default_value_t = 180)]
    pub timeout: u64,

    /// Output machine-readable JSON.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Arguments for `switchboard history <call_id>`.
#[derive(Debug, Args)]
pub struct HistoryArgs {
    /// Call identifier whose transcript to inspect.
    pub call_id: String,

    /// Output machine-readable JSON.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}
