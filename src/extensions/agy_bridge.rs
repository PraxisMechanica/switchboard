//! Antigravity App sidecar bridge delivering peer messages via official `agentapi send-message`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::model::{DeliveryOutcome, DeliveryRequest};
use super::ExtensionError;

const BRIDGE_POLL_INTERVAL: Duration = Duration::from_millis(250);
const AGENTAPI_SEND_TIMEOUT: Duration = Duration::from_secs(30);
const SIDECAR_ID: &str = "switchboard-bridge";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BridgeRequest {
    conversation_id: String,
    prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BridgeResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

/// Default local Unix socket path shared between the Antigravity sidecar and Switchboard daemon.
pub fn default_agy_bridge_socket() -> PathBuf {
    if let Ok(custom) = std::env::var("SWITCHBOARD_AGY_BRIDGE_SOCK") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local")
        .join("share")
        .join("switchboard")
        .join("agy-bridge.sock")
}

/// Default Antigravity App conversations directory (`~/.gemini/antigravity/conversations`).
pub fn default_agy_app_conversations_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join(".gemini")
            .join("antigravity")
            .join("conversations")
    })
}

/// Default Antigravity App brain directory (`~/.gemini/antigravity/brain`).
pub fn default_agy_app_brain_root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join(".gemini")
            .join("antigravity")
            .join("brain")
    })
}

/// Returns true when `conversation_id` exists as an Antigravity App conversation `.db` file.
pub fn is_app_conversation(conversations_dir: &Path, conversation_id: &str) -> bool {
    if Uuid::parse_str(conversation_id).is_err() {
        return false;
    }
    conversations_dir
        .join(format!("{conversation_id}.db"))
        .is_file()
}

/// CLI entrypoint for `switchboard agy-bridge [--install] [--socket <path>]`.
pub async fn run_agy_bridge_cmd(
    install: bool,
    socket: Option<PathBuf>,
) -> Result<(), ExtensionError> {
    if install {
        let binary = std::env::current_exe().map_err(ExtensionError::Io)?;
        let config_root = default_gemini_config_dir()?;
        let sidecar_json = install_agy_sidecar_in(&config_root, &binary)?;
        println!(
            "Installed Antigravity sidecar at {} (enabled in config.json).",
            sidecar_json.display()
        );
        return Ok(());
    }
    let sock = socket.unwrap_or_else(default_agy_bridge_socket);
    run_agy_bridge_server(&sock, Path::new("agentapi")).await
}

fn default_gemini_config_dir() -> Result<PathBuf, ExtensionError> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        ExtensionError::Protocol("HOME is not set; cannot locate ~/.gemini/config".to_string())
    })?;
    Ok(PathBuf::from(home).join(".gemini").join("config"))
}

/// Installs `~/.gemini/config/sidecars/switchboard-bridge/sidecar.json` and enables it in `config.json`.
pub fn install_agy_sidecar_in(
    gemini_config_dir: &Path,
    binary_path: &Path,
) -> Result<PathBuf, ExtensionError> {
    let sidecar_dir = gemini_config_dir.join("sidecars").join(SIDECAR_ID);
    std::fs::create_dir_all(&sidecar_dir)?;

    let sidecar_manifest = serde_json::json!({
        "display_name": "Switchboard Bridge",
        "description": "Delivers Switchboard peer messages into existing Antigravity App conversations via agentapi",
        "command": binary_path.to_string_lossy(),
        "args": ["agy-bridge"],
        "restart_policy": "always"
    });
    let sidecar_path = sidecar_dir.join("sidecar.json");
    std::fs::write(
        &sidecar_path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&sidecar_manifest).unwrap()
        ),
    )?;

    let config_path = gemini_config_dir.join("config.json");
    let mut root: Value = if config_path.is_file() {
        let raw = std::fs::read_to_string(&config_path)?;
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if !root.is_object() {
        root = serde_json::json!({});
    }
    let sidecars = root
        .as_object_mut()
        .unwrap()
        .entry("sidecars")
        .or_insert_with(|| serde_json::json!({}));
    if !sidecars.is_object() {
        *sidecars = serde_json::json!({});
    }
    let entry = sidecars
        .as_object_mut()
        .unwrap()
        .entry(SIDECAR_ID)
        .or_insert_with(|| serde_json::json!({}));
    if let Some(obj) = entry.as_object_mut() {
        obj.insert("enabled".to_string(), Value::Bool(true));
    } else {
        *entry = serde_json::json!({ "enabled": true });
    }
    std::fs::write(
        &config_path,
        format!("{}\n", serde_json::to_string_pretty(&root).unwrap()),
    )?;
    Ok(sidecar_path)
}

/// Runs the sidecar Unix-socket server that dispatches requests to `agentapi send-message`.
pub async fn run_agy_bridge_server(
    socket_path: &Path,
    agentapi_bin: &Path,
) -> Result<(), ExtensionError> {
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let _ = tokio::fs::remove_file(socket_path).await;
    let listener = UnixListener::bind(socket_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600));
    }
    tracing::info!(socket = %socket_path.display(), "switchboard agy-bridge sidecar listening");

    loop {
        let (stream, _) = listener.accept().await?;
        let bin = agentapi_bin.to_path_buf();
        tokio::spawn(async move {
            if let Err(err) = handle_bridge_client(stream, &bin).await {
                tracing::warn!(error = %err, "agy-bridge request failed");
            }
        });
    }
}

async fn handle_bridge_client(
    stream: UnixStream,
    agentapi_bin: &Path,
) -> Result<(), ExtensionError> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };

    let response = match serde_json::from_str::<BridgeRequest>(&line) {
        Ok(req) => execute_agentapi_send(agentapi_bin, &req).await,
        Err(err) => BridgeResponse {
            ok: false,
            error: Some(format!("invalid bridge request: {err}")),
        },
    };
    let encoded = serde_json::to_string(&response)
        .map_err(|err| ExtensionError::Protocol(err.to_string()))?;
    writer.write_all(encoded.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn execute_agentapi_send(agentapi_bin: &Path, req: &BridgeRequest) -> BridgeResponse {
    if Uuid::parse_str(&req.conversation_id).is_err() {
        return BridgeResponse {
            ok: false,
            error: Some(format!(
                "invalid conversation UUID '{}'",
                req.conversation_id
            )),
        };
    }

    let child = match Command::new(agentapi_bin)
        .args(["send-message", &req.conversation_id, &req.prompt])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            return BridgeResponse {
                ok: false,
                error: Some(format!(
                    "failed to spawn '{}': {err}",
                    agentapi_bin.display()
                )),
            };
        }
    };

    match tokio::time::timeout(AGENTAPI_SEND_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) if out.status.success() => BridgeResponse {
            ok: true,
            error: None,
        },
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let detail = if !stderr.is_empty() { stderr } else { stdout };
            BridgeResponse {
                ok: false,
                error: Some(if detail.is_empty() {
                    format!("agentapi exited with status {}", out.status)
                } else {
                    detail
                }),
            }
        }
        Ok(Err(err)) => BridgeResponse {
            ok: false,
            error: Some(format!("agentapi wait failed: {err}")),
        },
        Err(_) => BridgeResponse {
            ok: false,
            error: Some("agentapi send-message timed out".to_string()),
        },
    }
}

/// Sends `prompt` into an existing Antigravity App conversation via the sidecar bridge
/// and waits for the final assistant reply in `transcript.jsonl`.
pub async fn deliver_via_app_bridge(
    socket_path: &Path,
    brain_root: &Path,
    req: &DeliveryRequest,
    prompt: &str,
    turn_timeout: Duration,
    cancel: CancellationToken,
) -> Result<DeliveryOutcome, ExtensionError> {
    if !socket_path.exists() {
        return Ok(DeliveryOutcome::ProcessFailed {
            exit_code: None,
            detail: format!(
                "Antigravity App bridge sidecar is not running (missing {}). Run `switchboard agy-bridge --install` and ensure Antigravity App has the switchboard-bridge sidecar enabled.",
                socket_path.display()
            ),
        });
    }

    let logs_dir = brain_root
        .join(&req.session_id)
        .join(".system_generated")
        .join("logs");
    let transcript_path = logs_dir.join("transcript.jsonl");
    let transcript_full_path = logs_dir.join("transcript_full.jsonl");
    let baseline_step = max_step_index(&transcript_path).unwrap_or(0);

    let send_res = send_bridge_request(
        socket_path,
        &BridgeRequest {
            conversation_id: req.session_id.clone(),
            prompt: prompt.to_string(),
        },
    )
    .await;
    match send_res {
        Ok(resp) if resp.ok => {}
        Ok(resp) => {
            return Ok(DeliveryOutcome::ProcessFailed {
                exit_code: None,
                detail: resp
                    .error
                    .unwrap_or_else(|| "agy-bridge rejected send-message".to_string()),
            });
        }
        Err(err) => {
            return Ok(DeliveryOutcome::ProcessFailed {
                exit_code: None,
                detail: format!("agy-bridge connection failed: {err}"),
            });
        }
    }

    let marker = req.call_id.to_string();
    let deadline = Instant::now() + turn_timeout;
    loop {
        if let Some(reply) = find_reply_in_transcript(
            &transcript_path,
            &transcript_full_path,
            baseline_step,
            &marker,
        ) {
            return Ok(DeliveryOutcome::Replied {
                body: reply,
                session_id: Some(req.session_id.clone()),
            });
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(DeliveryOutcome::TimedOut);
        }
        let sleep_for = BRIDGE_POLL_INTERVAL.min(deadline - now);
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(DeliveryOutcome::Cancelled),
            () = tokio::time::sleep(sleep_for) => {}
        }
    }
}

async fn send_bridge_request(
    socket_path: &Path,
    req: &BridgeRequest,
) -> Result<BridgeResponse, ExtensionError> {
    let mut stream = UnixStream::connect(socket_path).await?;
    let encoded =
        serde_json::to_string(req).map_err(|err| ExtensionError::Protocol(err.to_string()))?;
    stream.write_all(encoded.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;

    let mut lines = BufReader::new(stream).lines();
    let Some(line) = lines.next_line().await? else {
        return Err(ExtensionError::Protocol(
            "agy-bridge closed connection without response".to_string(),
        ));
    };
    serde_json::from_str(&line)
        .map_err(|err| ExtensionError::Protocol(format!("invalid bridge response: {err}")))
}

pub(crate) fn max_step_index(transcript_path: &Path) -> Option<u64> {
    let raw = std::fs::read_to_string(transcript_path).ok()?;
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|val| val.get("step_index").and_then(Value::as_u64))
        .max()
}

/// Scans `transcript.jsonl` after `baseline_step` for the inbound message matching `call_id_marker`
/// and returns the subsequent completed final `PLANNER_RESPONSE` text (expanding from
/// `transcript_full.jsonl` when `truncated_fields` includes `"content"`).
pub(crate) fn find_reply_in_transcript(
    transcript_path: &Path,
    transcript_full_path: &Path,
    baseline_step: u64,
    call_id_marker: &str,
) -> Option<String> {
    let raw = std::fs::read_to_string(transcript_path).ok()?;
    let mut user_step: Option<u64> = None;

    for line in raw.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(step_index) = row.get("step_index").and_then(Value::as_u64) else {
            continue;
        };
        if step_index <= baseline_step {
            continue;
        }
        let content = row.get("content").and_then(Value::as_str).unwrap_or("");
        if user_step.is_none() {
            if content.contains(call_id_marker) {
                user_step = Some(step_index);
            }
            continue;
        }

        let is_model = row.get("source").and_then(Value::as_str) == Some("MODEL");
        let is_planner = row.get("type").and_then(Value::as_str) == Some("PLANNER_RESPONSE");
        let is_done = row.get("status").and_then(Value::as_str) == Some("DONE");
        let has_tools = row
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|arr| !arr.is_empty());

        if is_model && is_planner && is_done && !has_tools && !content.trim().is_empty() {
            let content_truncated = row
                .get("truncated_fields")
                .and_then(Value::as_array)
                .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some("content")));
            if content_truncated {
                if let Some(full) = read_full_step_content(transcript_full_path, step_index) {
                    return Some(full.trim().to_string());
                }
            }
            return Some(content.trim().to_string());
        }
    }
    None
}

fn read_full_step_content(transcript_full_path: &Path, target_step: u64) -> Option<String> {
    let raw = std::fs::read_to_string(transcript_full_path).ok()?;
    for line in raw.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("step_index").and_then(Value::as_u64) == Some(target_step) {
            return row
                .get("content")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }
    }
    None
}

#[cfg(test)]
#[path = "agy_bridge_tests.rs"]
mod tests;
