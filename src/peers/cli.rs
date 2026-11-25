//! CLI definitions for the peers vertical slice.

use clap::Args;

/// Arguments for `switchboard dial`.
#[derive(Debug, Args)]
pub struct DialArgs {
    /// Target peer handle, address, or peer ID.
    pub target: String,

    /// Human-readable reason for requesting a direct line.
    #[arg(short = 'm', long)]
    pub reason: String,
}

/// Arguments for `switchboard approve`, `switchboard reject`, and `switchboard revoke`.
#[derive(Debug, Args)]
pub struct PeerTargetArgs {
    /// Peer handle to act upon.
    pub handle: String,
}

/// Arguments for `switchboard ring` and `switchboard peers`.
#[derive(Debug, Args)]
pub struct PeersListArgs {
    /// Output machine-readable JSON.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}
