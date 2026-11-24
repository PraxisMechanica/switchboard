//! Delivery to an actively open Codex Desktop session via `codex queue` + `thread/read`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::Command;
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;

use super::codex::{CodexConfig, CodexSession, RpcStep, CLIENT_NAME, CLIENT_TITLE, CLIENT_VERSION};
use super::codex_policy::{extract_thread_list, format_prompt};
use super::model::{resolve_codex_binary, CodexSessionInfo, DeliveryOutcome, DeliveryRequest};
use super::ExtensionError;

const QUEUE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Lists pre-existing Codex sessions via read-only `codex app-server` `thread/list`.
pub(crate) async fn list_codex_sessions(
    config: &CodexConfig,
    limit: usize,
) -> Result<Vec<CodexSessionInfo>, ExtensionError> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut session = CodexSession::spawn(config, &cwd)?;
    let cancel = CancellationToken::new();
    let deadline = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(deadline);

    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": CLIENT_NAME,
                "title": CLIENT_TITLE,
                "version": CLIENT_VERSION
            }
        }
    });
    if let RpcStep::Outcome(outcome) = session
        .rpc_call(1, &init_req, deadline.as_mut(), &cancel, "initialize")
        .await?
    {
        return Err(ExtensionError::Protocol(format!(
            "codex initialize failed while listing sessions: {outcome:?}"
        )));
    }

    let initialized_note = json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    });
    if session.send_json(&initialized_note).await.is_err() {
        return Err(ExtensionError::Protocol(
            "codex exited before thread/list".into(),
        ));
    }

    let list_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "thread/list",
        "params": { "limit": limit }
    });
    let resp = match session
        .rpc_call(2, &list_req, deadline.as_mut(), &cancel, "thread/list")
        .await?
    {
        RpcStep::Response(msg) => msg,
        RpcStep::Outcome(outcome) => {
            return Err(ExtensionError::Protocol(format!(
                "codex thread/list interrupted: {outcome:?}"
            )));
        }
    };
    session.shutdown().await;
    Ok(extract_thread_list(&resp))
}

/// Returns true when `thread/resume` reports that another Codex runtime (such as Codex Desktop)
/// currently holds the exclusive writer lock for the target pre-existing thread.
pub(crate) fn is_active_writer_error(err_msg: &str) -> bool {
    err_msg.to_ascii_lowercase().contains("active writer")
}

/// Queues a plain-text message onto an already-open pre-existing Codex session via `codex queue`
/// and waits on `thread/read` until the live session finishes answering that turn.
pub(crate) async fn deliver_to_active_session(
    config: &CodexConfig,
    session: &mut CodexSession,
    req: &DeliveryRequest,
    mut deadline: Pin<&mut Sleep>,
    cancel: &CancellationToken,
) -> Result<DeliveryOutcome, ExtensionError> {
    let baseline_turns =
        match read_thread_turns(session, 10, &req.session_id, deadline.as_mut(), cancel).await? {
            TurnRead::Turns(turns) => turns,
            TurnRead::Outcome(outcome) => return Ok(outcome),
        };
    let seen_ids: HashSet<String> = baseline_turns
        .iter()
        .filter_map(|t| t.get("id").and_then(Value::as_str).map(ToOwned::to_owned))
        .collect();

    let prompt = format_prompt(req);
    enqueue_codex_message(config, &req.workspace_dir, &req.session_id, &prompt).await?;

    let call_marker = req.call_id.to_string();
    let mut rpc_id: u64 = 11;
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                session.shutdown().await;
                return Ok(DeliveryOutcome::Cancelled);
            }
            () = &mut deadline => {
                session.shutdown().await;
                return Ok(DeliveryOutcome::TimedOut);
            }
            () = tokio::time::sleep(QUEUE_POLL_INTERVAL) => {}
        }

        let turns =
            match read_thread_turns(session, rpc_id, &req.session_id, deadline.as_mut(), cancel)
                .await?
            {
                TurnRead::Turns(turns) => turns,
                TurnRead::Outcome(outcome) => return Ok(outcome),
            };
        rpc_id += 1;

        if let Some(reply) = find_completed_reply_turn(&turns, &seen_ids, &call_marker) {
            session.shutdown().await;
            return Ok(DeliveryOutcome::Replied {
                body: reply,
                session_id: Some(req.session_id.clone()),
            });
        }
    }
}

enum TurnRead {
    Turns(Vec<Value>),
    Outcome(DeliveryOutcome),
}

async fn read_thread_turns(
    session: &mut CodexSession,
    rpc_id: u64,
    thread_id: &str,
    deadline: Pin<&mut Sleep>,
    cancel: &CancellationToken,
) -> Result<TurnRead, ExtensionError> {
    let read_req = json!({
        "jsonrpc": "2.0",
        "id": rpc_id,
        "method": "thread/read",
        "params": {
            "threadId": thread_id,
            "includeTurns": true
        }
    });
    match session
        .rpc_call(rpc_id, &read_req, deadline, cancel, "thread/read")
        .await?
    {
        RpcStep::Response(msg) => {
            let turns = msg
                .get("result")
                .and_then(|r| r.get("thread"))
                .and_then(|t| t.get("turns"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Ok(TurnRead::Turns(turns))
        }
        RpcStep::Outcome(outcome) => Ok(TurnRead::Outcome(outcome)),
    }
}

async fn enqueue_codex_message(
    config: &CodexConfig,
    workspace_dir: &Path,
    thread_id: &str,
    prompt: &str,
) -> Result<(), ExtensionError> {
    let binary = resolve_codex_binary(&config.binary);
    let mut cmd = Command::new(&binary);
    cmd.args([
        "queue",
        "--disable",
        "computer_use",
        "-c",
        "notify=[]",
        "--thread",
        thread_id,
        "--message",
        prompt,
    ])
    .current_dir(workspace_dir)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    if let Some(home) = &config.codex_home {
        cmd.env("CODEX_HOME", home);
    }

    let output = cmd.output().await.map_err(|err| {
        ExtensionError::Spawn(format!("codex queue '{}': {err}", binary.display()))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(ExtensionError::Protocol(format!(
            "codex queue failed (exit {:?}): {stderr}",
            output.status.code()
        )));
    }
    Ok(())
}

/// Scans turns returned by `thread/read` for a newly completed turn matching `call_marker`
/// and extracts its final `agentMessage` text.
pub(crate) fn find_completed_reply_turn(
    turns: &[Value],
    seen_turn_ids: &HashSet<String>,
    call_marker: &str,
) -> Option<String> {
    for turn in turns.iter().rev() {
        let Some(turn_id) = turn.get("id").and_then(Value::as_str) else {
            continue;
        };
        if seen_turn_ids.contains(turn_id) {
            continue;
        }
        if turn.get("status").and_then(Value::as_str) != Some("completed") {
            continue;
        }
        let Some(items) = turn.get("items").and_then(Value::as_array) else {
            continue;
        };
        let matches_call = items.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("userMessage")
                && user_message_text(item).contains(call_marker)
        });
        if !matches_call {
            continue;
        }
        if let Some(reply) = items
            .iter()
            .rev()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("agentMessage"))
            .find_map(agent_message_text)
        {
            return Some(reply);
        }
    }
    None
}

fn user_message_text(item: &Value) -> String {
    if let Some(text) = item.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    item.get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn agent_message_text(item: &Value) -> Option<String> {
    if let Some(text) = item
        .get("text")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Some(text.to_string());
    }
    item.get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
#[path = "codex_queue_tests.rs"]
mod tests;
