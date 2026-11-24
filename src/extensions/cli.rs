//! CLI definitions for the extensions vertical slice.

use std::path::PathBuf;

use clap::Subcommand;

use crate::db::Database;
use crate::primitives::SystemClock;

use super::{
    CodexConfig, CodexHarness, ExtensionError, ExtensionName, ExtensionsService, HarnessKind,
};

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

        /// Pre-existing Codex session/thread ID to bind to this extension.
        #[arg(long)]
        session: Option<String>,
    },

    /// Bind an existing extension to a pre-existing Codex session/thread ID.
    Bind {
        /// Registered extension name.
        name: String,

        /// Pre-existing Codex session/thread ID (`switchboard ext sessions` lists them).
        session: String,
    },

    /// List pre-existing Codex sessions available for binding or direct calling.
    Sessions {
        /// Maximum number of recent sessions to display.
        #[arg(long, default_value_t = 20)]
        limit: usize,

        /// Output machine-readable JSON.
        #[arg(long, default_value_t = false)]
        json: bool,
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

/// Executes a `switchboard ext` subcommand against the local extensions store and Codex harness.
pub async fn run_ext_cmd(
    codex_cfg: CodexConfig,
    db: Database,
    cmd: ExtCmd,
) -> Result<(), ExtensionError> {
    let harness = CodexHarness::new(codex_cfg);
    let svc = ExtensionsService::new(db, harness.clone(), SystemClock);
    match cmd {
        ExtCmd::Add {
            name,
            dir,
            desc,
            harness,
            session,
        } => {
            let ext_name = ExtensionName::new(&name)?;
            let kind: HarnessKind = harness.parse()?;
            let ext = svc.add(ext_name, dir, desc, kind, session).await?;
            let bound = ext.session_id.as_deref().unwrap_or("unbound");
            println!(
                "Registered extension '{}' -> {} ({}, session: {})",
                ext.name,
                ext.workspace_dir.display(),
                ext.harness,
                bound
            );
        }
        ExtCmd::Bind { name, session } => {
            let ext_name = ExtensionName::new(&name)?;
            let ext = svc.bind_session(&ext_name, session).await?;
            println!(
                "Bound extension '{}' to existing session {}",
                ext.name,
                ext.session_id.as_deref().unwrap_or("")
            );
        }
        ExtCmd::Sessions { limit, json } => {
            let sessions = harness.list_sessions(limit).await?;
            if json {
                let rendered = serde_json::to_string_pretty(&sessions)
                    .map_err(|e| ExtensionError::Protocol(e.to_string()))?;
                println!("{rendered}");
            } else if sessions.is_empty() {
                println!("No pre-existing Codex sessions found.");
            } else {
                for s in sessions {
                    println!("{}  {}  {}", s.id, s.cwd, s.title);
                }
            }
        }
        ExtCmd::Ls { json } => {
            let exts = svc.list().await?;
            if json {
                let rendered = serde_json::to_string_pretty(&exts)
                    .map_err(|e| ExtensionError::Protocol(e.to_string()))?;
                println!("{rendered}");
            } else {
                for ext in exts {
                    let bound = ext.session_id.as_deref().unwrap_or("unbound");
                    println!(
                        "{:<16} {:<8} [{}] {} — {}",
                        ext.name,
                        ext.harness,
                        bound,
                        ext.workspace_dir.display(),
                        ext.description
                    );
                }
            }
        }
        ExtCmd::Rm { name } => {
            let ext_name = ExtensionName::new(&name)?;
            if svc.remove(&ext_name).await? {
                println!("Removed extension '{ext_name}'.");
            } else {
                println!("Extension '{ext_name}' not found.");
            }
        }
    }
    Ok(())
}
