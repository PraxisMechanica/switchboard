//! Text-only agent-to-agent isolation policy for `codex app-server`.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::model::DeliveryRequest;
use super::ExtensionError;

/// System instructions injected on every Switchboard Codex thread to keep calls message-only.
pub const TEXT_ONLY_DEVELOPER_INSTRUCTIONS: &str = "You are receiving an agent-to-agent text message delivered via Switchboard. Reply in plain text only. Do not run shell commands, edit files, open browsers, or invoke external tools or MCP servers.";

const ISOLATED_CODEX_CONFIG: &str = r#"# Managed by Switchboard for isolated text-only agent-to-agent messaging.
notify = []
approval_policy = "on-request"
sandbox_mode = "read-only"

[features]
computer_use = false
browser_use = false
browser_use_external = false
browser_use_full_cdp_access = false
in_app_browser = false
in_app_local_automation = false
apps = false
plugins = false
hooks = false
shell_tool = false
unified_exec = false
code_mode_host = false
image_generation = false
"#;

/// Defense-in-depth CLI arguments for headless text-only `codex app-server` messaging.
pub fn default_codex_args() -> Vec<String> {
    [
        "app-server",
        "--disable",
        "computer_use",
        "--disable",
        "browser_use",
        "--disable",
        "browser_use_external",
        "--disable",
        "browser_use_full_cdp_access",
        "--disable",
        "in_app_browser",
        "--disable",
        "in_app_local_automation",
        "--disable",
        "apps",
        "--disable",
        "plugins",
        "--disable",
        "hooks",
        "--disable",
        "shell_tool",
        "--disable",
        "unified_exec",
        "--disable",
        "code_mode_host",
        "-c",
        "notify=[]",
        "-c",
        "approval_policy=\"on-request\"",
        "-c",
        "sandbox_mode=\"read-only\"",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// Prepares an isolated `CODEX_HOME` directory with zero MCP servers, zero plugins, and
/// read-only message-only config while reusing the user's existing `auth.json` when present.
pub fn prepare_isolated_codex_home(home_dir: &Path) -> Result<(), ExtensionError> {
    fs::create_dir_all(home_dir)?;

    let config_path = home_dir.join("config.toml");
    fs::write(&config_path, ISOLATED_CODEX_CONFIG)?;

    if let Some(source_auth) = default_user_codex_auth_path() {
        link_auth_file(&source_auth, &home_dir.join("auth.json"))?;
    }

    Ok(())
}

fn default_user_codex_auth_path() -> Option<PathBuf> {
    let user_home = std::env::var_os("HOME")?;
    let auth = PathBuf::from(user_home).join(".codex").join("auth.json");
    auth.is_file().then_some(auth)
}

fn link_auth_file(source: &Path, dest: &Path) -> Result<(), ExtensionError> {
    if dest.exists() || dest.symlink_metadata().is_ok() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        if std::os::unix::fs::symlink(source, dest).is_ok() {
            return Ok(());
        }
    }
    fs::copy(source, dest)?;
    Ok(())
}

/// Builds JSON-RPC `thread/start` params enforcing read-only text-only behavior.
pub fn thread_start_params(workspace_dir: &Path) -> Value {
    json!({
        "cwd": workspace_dir.to_string_lossy(),
        "sandbox": "read-only",
        "approvalPolicy": "on-request",
        "developerInstructions": TEXT_ONLY_DEVELOPER_INSTRUCTIONS
    })
}

/// Builds JSON-RPC `turn/start` params enforcing read-only offline sandbox policy.
pub fn turn_start_params(thread_id: &str, prompt: &str) -> Value {
    json!({
        "threadId": thread_id,
        "approvalPolicy": "on-request",
        "sandboxPolicy": {
            "type": "readOnly",
            "networkAccess": false
        },
        "input": [{ "type": "text", "text": prompt }]
    })
}

/// Builds an immediate refusal response for any server-initiated approval, tool, or elicitation
/// request so inbound peer messages cannot execute commands, write files, or prompt desktop tools.
pub fn denial_response_for_server_request(msg: &Value) -> Option<Value> {
    let id = msg.get("id").filter(|v| !v.is_null())?;
    let method = msg.get("method").and_then(Value::as_str)?;

    let response = match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "decision": "decline" }
        }),
        "execCommandApproval" | "applyPatchApproval" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "decision": "denied" }
        }),
        "item/permissions/requestApproval" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "permissions": {} }
        }),
        "mcpServer/elicitation/request" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "action": "decline" }
        }),
        "item/tool/requestUserInput" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "answers": {} }
        }),
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": "switchboard text-only messaging declines server tool and permission requests"
            }
        }),
    };

    Some(response)
}

pub(crate) fn format_prompt(req: &DeliveryRequest) -> String {
    format!(
        "Incoming switchboard message from peer @{from} (extension: {ext}, call: {call_id}):\n\n{body}",
        from = req.from_handle,
        ext = req.extension,
        call_id = req.call_id,
        body = req.body,
    )
}

pub(crate) fn rpc_error_message(msg: &Value) -> Option<String> {
    let err = msg.get("error").filter(|v| !v.is_null())?;
    let text = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown json-rpc error");
    Some(text.to_string())
}

pub(crate) fn extract_thread_id(msg: &Value) -> Option<String> {
    let result = msg.get("result")?;
    result
        .get("threadId")
        .and_then(Value::as_str)
        .or_else(|| {
            result
                .get("thread")
                .and_then(|t| t.get("id"))
                .and_then(Value::as_str)
        })
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn extract_delta_text(params: &Value) -> Option<&str> {
    params
        .get("delta")
        .and_then(Value::as_str)
        .or_else(|| params.get("text").and_then(Value::as_str))
}

pub(crate) fn extract_completed_agent_message(params: &Value) -> Option<String> {
    let item = params.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("agentMessage") {
        return None;
    }

    if let Some(text) = item
        .get("text")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Some(text.to_string());
    }

    if let Some(parts) = item.get("content").and_then(Value::as_array) {
        let joined: String = parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect();
        if !joined.is_empty() {
            return Some(joined);
        }
    }

    None
}

#[cfg(test)]
#[path = "codex_policy_tests.rs"]
mod tests;
