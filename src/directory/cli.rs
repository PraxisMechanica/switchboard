//! CLI definitions for the directory vertical slice.

use clap::Subcommand;

/// Subcommands for `switchboard directory`.
#[derive(Debug, Subcommand)]
pub enum DirectoryCmd {
    /// Fetch and verify peer cards from the configured directory into the local cache.
    Sync,

    /// Sign and publish the local node's peer card to the directory.
    Publish,

    /// List cached peers from the local directory store.
    Ls {
        /// Output machine-readable JSON.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}
