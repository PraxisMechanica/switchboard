//! CLI definitions for the extensions vertical slice.

use std::path::PathBuf;

use clap::Subcommand;

/// Subcommands for `switchboard ext`.
#[derive(Debug, Subcommand)]
pub enum ExtCmd {
    /// Register or update a local callable workspace extension.
    Add {
        /// Unique extension name (`[a-z0-9_-]{1,48}`).
        name: String,

        /// Local workspace directory path.
        #[arg(long)]
        dir: PathBuf,

        /// Human-readable description of the workspace extension.
        #[arg(long)]
        desc: String,

        /// Agent harness adapter to invoke for deliveries.
        #[arg(long, default_value = "codex")]
        harness: String,
    },

    /// List registered local workspace extensions.
    Ls {
        /// Output machine-readable JSON.
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Remove a registered local workspace extension.
    Rm {
        /// Name of the extension to remove.
        name: String,
    },
}
