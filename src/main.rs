//! Composition root and thin `clap` CLI for `switchboard`.

mod daemon_runner;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use switchboard::calls::{CallArgs, CallId, HistoryArgs, ReplyArgs};
use switchboard::config::SwitchboardConfig;
use switchboard::daemon::migrate_all;
use switchboard::db::Database;
use switchboard::directory::{
    AdvertisedExtension, DirectoryCmd, DirectoryPort, DirectoryService, GitPhonebook,
    MemoryDirectory, PeerCard,
};
use switchboard::extensions::{run_ext_cmd, CodexHarness, ExtCmd, ExtensionsService};
use switchboard::identity::cli::{InitArgs, WhoamiArgs};
use switchboard::identity::IdentityService;
use switchboard::ipc::{self, IpcRequest, IpcResponse};
use switchboard::peers::cli::{DialArgs, PeerTargetArgs, PeersListArgs};
use switchboard::peers::{PeersService, TracingNotifier};
use switchboard::primitives::{Clock, SystemClock};
use switchboard::service::cli::{run_service_cmd, ServiceCmd};

#[derive(Debug, Parser)]
#[command(
    name = "switchboard",
    version,
    about = "Decentralized P2P agent switchboard"
)]
struct Cli {
    /// Optional path to switchboard TOML configuration.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initialize this node's Ed25519 identity.
    Init(InitArgs),

    /// Show this node's handle and cryptographic PeerId.
    Whoami(WhoamiArgs),

    /// Run the resident P2P + IPC switchboard daemon.
    Daemon,

    /// Manage team phone book discovery and publication.
    Directory {
        #[command(subcommand)]
        cmd: DirectoryCmd,
    },

    /// Manage callable local agent workspace extensions.
    Ext {
        #[command(subcommand)]
        cmd: ExtCmd,
    },

    /// Dial a teammate to request an approved direct line.
    Dial(DialArgs),

    /// List ringing lines awaiting your approval.
    Ring(PeersListArgs),

    /// List known peers and their approval statuses.
    Peers(PeersListArgs),

    /// Approve a pending incoming request from a teammate.
    Approve(PeerTargetArgs),

    /// Reject a pending incoming request from a teammate.
    Reject(PeerTargetArgs),

    /// Revoke an existing peer relationship locally.
    Revoke(PeerTargetArgs),

    /// Place an agent-to-agent call to `<peer>.<harness>.<conversation-name-or-id>`.
    Call(CallArgs),

    /// List local pre-existing Codex sessions formatted as callable Switchboard addresses.
    Sessions {
        /// Maximum number of recent sessions to display.
        #[arg(long, default_value_t = 20)]
        limit: usize,

        /// Output machine-readable JSON.
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Send a follow-up turn on an existing call.
    Reply(ReplyArgs),

    /// Inspect local transcript history for a call.
    History(HistoryArgs),

    /// Install, remove, or inspect the user-level background daemon service.
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let cfg = match (&cli.command, cli.config.as_deref()) {
        (Command::Init(_), Some(path)) if !path.exists() => SwitchboardConfig::default(),
        _ => SwitchboardConfig::load(cli.config.as_deref())
            .context("failed to load switchboard configuration")?,
    };
    let db = Database::open(&cfg.db_path()).context("failed to open switchboard database")?;
    migrate_all(&db).context("failed to run database migrations")?;

    match cli.command {
        Command::Init(args) => cmd_init(&cfg, cli.config.as_deref(), db, args).await,
        Command::Whoami(args) => cmd_whoami(db, args).await,
        Command::Daemon => daemon_runner::run_daemon(cfg, db).await,
        Command::Directory { cmd } => cmd_directory(&cfg, db, cmd).await,
        Command::Ext { cmd } => cmd_ext(&cfg, db, cmd).await,
        Command::Ring(args) => cmd_ring(db, args).await,
        Command::Peers(args) => cmd_peers(db, args).await,
        Command::Revoke(args) => cmd_revoke(db, args).await,
        Command::Dial(args) => cmd_ipc_dial(&cfg, args).await,
        Command::Approve(args) => cmd_ipc_approve(&cfg, args).await,
        Command::Reject(args) => cmd_ipc_reject(&cfg, args).await,
        Command::Call(args) => cmd_ipc_call(&cfg, args).await,
        Command::Sessions { limit, json } => cmd_sessions(&cfg, db, limit, json).await,
        Command::Reply(args) => cmd_ipc_reply(&cfg, args).await,
        Command::History(args) => cmd_history(db, args).await,
        Command::Service { cmd } => {
            run_service_cmd(cmd, cli.config, cfg.data_dir.join("logs"))?;
            Ok(())
        }
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

async fn cmd_init(
    cfg: &SwitchboardConfig,
    explicit_cfg_path: Option<&std::path::Path>,
    db: Database,
    args: InitArgs,
) -> Result<()> {
    let svc = IdentityService::new(db, SystemClock);
    let id = svc.init(&args.name, args.force).await?;
    println!("Initialized '{}' ({})", id.handle, id.keypair.peer_id());
    if let Some(remote) = args.git_remote {
        let mut updated = cfg.clone();
        updated.directory.git_remote = Some(remote);
        let target = explicit_cfg_path.map(PathBuf::from).unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
                .join("switchboard")
                .join("config.toml")
        });
        updated.write_sample(&target)?;
        println!("Wrote configuration to {}", target.display());
    }
    Ok(())
}

async fn cmd_whoami(db: Database, args: WhoamiArgs) -> Result<()> {
    let svc = IdentityService::new(db, SystemClock);
    let id = svc.load_required().await?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "handle": id.handle,
                "peer_id": id.keypair.peer_id(),
            }))?
        );
    } else {
        println!("handle:  {}", id.handle);
        println!("peer_id: {}", id.keypair.peer_id());
    }
    Ok(())
}

async fn cmd_directory(cfg: &SwitchboardConfig, db: Database, cmd: DirectoryCmd) -> Result<()> {
    match &cfg.directory.git_remote {
        Some(remote) => {
            let port = GitPhonebook::new(remote.clone(), cfg.phonebook_dir());
            run_directory_cmd(cfg, db, port, cmd).await
        }
        None => {
            if matches!(cmd, DirectoryCmd::Sync | DirectoryCmd::Publish) {
                bail!("directory.git_remote is not configured in config.toml");
            }
            run_directory_cmd(cfg, db, MemoryDirectory::new(), cmd).await
        }
    }
}

async fn run_directory_cmd<D: DirectoryPort>(
    cfg: &SwitchboardConfig,
    db: Database,
    port: D,
    cmd: DirectoryCmd,
) -> Result<()> {
    let dir_svc = DirectoryService::new(db.clone(), port);
    match cmd {
        DirectoryCmd::Sync => {
            let report = dir_svc.sync().await?;
            println!(
                "Directory synced: {} verified, {} rejected",
                report.synced, report.rejected
            );
        }
        DirectoryCmd::Publish => {
            let id_svc = IdentityService::new(db.clone(), SystemClock);
            let local = id_svc.load_required().await?;
            let ext_svc =
                ExtensionsService::new(db, CodexHarness::new(cfg.codex_config()), SystemClock);
            let exts = ext_svc.list().await?;
            let direct_addrs = match ipc::send_request(&cfg.socket_path(), &IpcRequest::Ping).await
            {
                Ok(IpcResponse::Pong { direct_addrs, .. }) => direct_addrs,
                _ => Vec::new(),
            };
            let card = PeerCard {
                handle: local.handle,
                peer_id: local.keypair.peer_id(),
                direct_addrs,
                extensions: exts
                    .into_iter()
                    .map(|e| AdvertisedExtension {
                        name: e.name.to_string(),
                        description: e.description,
                    })
                    .collect(),
                updated_at: SystemClock.now(),
            };
            dir_svc.publish_local(&local.keypair, card).await?;
            println!("Published signed peer card to team directory.");
        }
        DirectoryCmd::Ls { json } => {
            let cards = dir_svc.list_cached().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&cards)?);
            } else if cards.is_empty() {
                println!("No cached directory entries. Run `switchboard directory sync`.");
            } else {
                for card in cards {
                    let ext_names: Vec<&str> =
                        card.extensions.iter().map(|e| e.name.as_str()).collect();
                    println!(
                        "{:<16} {}  [{}]",
                        card.handle,
                        card.peer_id.short(),
                        ext_names.join(", ")
                    );
                }
            }
        }
    }
    Ok(())
}

async fn cmd_ext(cfg: &SwitchboardConfig, db: Database, cmd: ExtCmd) -> Result<()> {
    run_ext_cmd(cfg.codex_config(), db, cmd).await?;
    Ok(())
}

async fn cmd_sessions(
    cfg: &SwitchboardConfig,
    db: Database,
    limit: usize,
    json: bool,
) -> Result<()> {
    let handle = IdentityService::new(db, SystemClock)
        .load_required()
        .await
        .map(|id| id.handle)
        .unwrap_or_else(|_| "<you>".to_string());
    let sessions = CodexHarness::new(cfg.codex_config())
        .list_sessions(limit)
        .await?;
    if json {
        let enriched: Vec<_> = sessions
            .into_iter()
            .map(|s| {
                serde_json::json!({
                    "address": format!("{handle}.codex.{}", s.id),
                    "id": s.id,
                    "cwd": s.cwd,
                    "title": s.title,
                    "updated_at": s.updated_at,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&enriched)?);
    } else if sessions.is_empty() {
        println!("No pre-existing Codex sessions found.");
    } else {
        for s in sessions {
            println!("{handle}.codex.{}  {}", s.id, s.title);
        }
    }
    Ok(())
}

async fn cmd_ring(db: Database, args: PeersListArgs) -> Result<()> {
    let svc = PeersService::new(db, SystemClock, TracingNotifier);
    let pending = svc.list_pending_inbound().await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&pending)?);
    } else if pending.is_empty() {
        println!("No ringing lines awaiting approval.");
    } else {
        for p in pending {
            println!("{:<16} {}  {:?}", p.handle, p.peer_id.short(), p.status);
        }
    }
    Ok(())
}

async fn cmd_peers(db: Database, args: PeersListArgs) -> Result<()> {
    let svc = PeersService::new(db, SystemClock, TracingNotifier);
    let peers = svc.list_all().await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&peers)?);
    } else {
        for p in peers {
            println!("{:<16} {}  {:?}", p.handle, p.peer_id.short(), p.status);
        }
    }
    Ok(())
}

async fn cmd_revoke(db: Database, args: PeerTargetArgs) -> Result<()> {
    let svc = PeersService::new(db, SystemClock, TracingNotifier);
    let (peer, _) = svc.revoke_by_handle(&args.handle).await?;
    println!("Revoked peer '{}' ({}).", peer.handle, peer.peer_id.short());
    Ok(())
}

async fn cmd_ipc_dial(cfg: &SwitchboardConfig, args: DialArgs) -> Result<()> {
    let resp = ipc::send_request(
        &cfg.socket_path(),
        &IpcRequest::Knock {
            target: args.target,
            reason: args.reason,
            timeout_secs: 30,
        },
    )
    .await?;
    match resp {
        IpcResponse::PeerUpdated { peer } => {
            println!(
                "Dial request sent to '{}' ({:?}).",
                peer.handle, peer.status
            );
            Ok(())
        }
        IpcResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

async fn cmd_ipc_approve(cfg: &SwitchboardConfig, args: PeerTargetArgs) -> Result<()> {
    let resp = ipc::send_request(
        &cfg.socket_path(),
        &IpcRequest::Approve {
            handle: args.handle,
            timeout_secs: 30,
        },
    )
    .await?;
    match resp {
        IpcResponse::PeerUpdated { peer } => {
            println!(
                "Approved peer '{}' ({}).",
                peer.handle,
                peer.peer_id.short()
            );
            Ok(())
        }
        IpcResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

async fn cmd_ipc_reject(cfg: &SwitchboardConfig, args: PeerTargetArgs) -> Result<()> {
    let resp = ipc::send_request(
        &cfg.socket_path(),
        &IpcRequest::Reject {
            handle: args.handle,
            timeout_secs: 30,
        },
    )
    .await?;
    match resp {
        IpcResponse::PeerUpdated { peer } => {
            println!("Rejected peer '{}'.", peer.handle);
            Ok(())
        }
        IpcResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

async fn cmd_ipc_call(cfg: &SwitchboardConfig, args: CallArgs) -> Result<()> {
    let resp = ipc::send_request(
        &cfg.socket_path(),
        &IpcRequest::Call {
            target: args.target,
            message: args.message,
            timeout_secs: args.timeout,
        },
    )
    .await?;
    print_call_response(resp, args.json)
}

async fn cmd_ipc_reply(cfg: &SwitchboardConfig, args: ReplyArgs) -> Result<()> {
    let call_id: CallId = args.call_id.parse()?;
    let resp = ipc::send_request(
        &cfg.socket_path(),
        &IpcRequest::Reply {
            call_id,
            message: args.message,
            done: args.done,
            timeout_secs: args.timeout,
        },
    )
    .await?;
    print_call_response(resp, args.json)
}

fn print_call_response(resp: IpcResponse, json: bool) -> Result<()> {
    match resp {
        IpcResponse::CallResult { response } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                println!("call_id: {}", response.call_id);
                println!("status:  {:?}", response.status);
                println!("\n{}", response.body);
            }
            Ok(())
        }
        IpcResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

async fn cmd_history(db: Database, args: HistoryArgs) -> Result<()> {
    let call_id: CallId = args.call_id.parse()?;
    let repo = switchboard::calls::CallsRepo::new(db);
    let call = repo
        .get_call(call_id)
        .await?
        .with_context(|| format!("call '{call_id}' not found"))?;
    let messages = repo.list_messages(call_id).await?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "call": call,
                "messages": messages,
            }))?
        );
    } else {
        println!(
            "call {} ({}/{}, {:?})",
            call.call_id, call.peer_handle, call.extension, call.status
        );
        for m in messages {
            println!("\n[{}] -> {}", m.sender_handle, m.request_body);
            if let Some(r) = m.response {
                println!("[reply {:?}] -> {}", r.status, r.body);
            }
        }
    }
    Ok(())
}
