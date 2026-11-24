//! CLI definitions for the identity vertical slice.

use clap::Args;

/// Arguments for `switchboard init`.
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Human handle for this node (lowercase ASCII letters, digits, '-' or '_').
    #[arg(long)]
    pub name: String,

    /// Optional Git remote URL for the team phone book written to config.toml.
    #[arg(long)]
    pub git_remote: Option<String>,

    /// Overwrite an existing identity if one already exists.
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

/// Arguments for `switchboard whoami`.
#[derive(Debug, Args)]
pub struct WhoamiArgs {
    /// Output machine-readable JSON.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}
