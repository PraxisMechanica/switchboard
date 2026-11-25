//! Text-only agent-to-agent isolation policy for `codex app-server`.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::model::{CodexSessionInfo, DeliveryRequest, HarnessKind};

/// System instructions injected on every Switchboard Codex turn to keep calls message-only.
pub const TEXT_ONLY_DEVELOPER_INSTRUCTIONS: &str = "You are receiving an agent-to-agent text message delivered via Switchboard. Reply in plain text only. Do not run shell commands, edit files, open browsers, or invoke external tools or MCP servers.";

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

/// Appends `-c mcp_servers.<name>.enabled=false` for every MCP server declared in the active
/// Codex `config.toml` so `codex app-server` can resume pre-existing `~/.codex` sessions without
/// launching desktop computer-use, browser, or third-party MCP processes.
pub fn build_effective_codex_args(
    base_args: &[String],
    codex_home_override: Option<&Path>,
) -> Vec<String> {
    let mut args = base_args.to_vec();
    if args.first().map(String::as_str) != Some("app-server") {
        return args;
    }

    if let Some(config_path) = resolve_codex_config_path(codex_home_override) {
        if let Ok(raw) = fs::read_to_string(config_path) {
            for name in extract_mcp_server_names(&raw) {
                let key = format_toml_key(&name);
                args.push("-c".to_string());
                args.push(format!("mcp_servers.{key}.enabled=false"));
            }
        }
    }

    args
}

fn resolve_codex_config_path(codex_home_override: Option<&Path>) -> Option<PathBuf> {
    if let Some(home) = codex_home_override {
        return Some(home.join("config.toml"));
    }
    if let Some(env_home) = std::env::var_os("CODEX_HOME") {
        return Some(PathBuf::from(env_home).join("config.toml"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex").join("config.toml"))
}

pub(crate) fn extract_mcp_server_names(raw_toml: &str) -> Vec<String> {
    let Ok(parsed) = toml::from_str::<toml::Value>(raw_toml) else {
        return Vec::new();
    };
    let Some(table) = parsed.get("mcp_servers").and_then(toml::Value::as_table) else {
        return Vec::new();
    };
    let mut names: Vec<String> = table.keys().cloned().collect();
    names.sort();
    names
}

fn format_toml_key(name: &str) -> String {
    let is_bare = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if is_bare {
        name.to_string()
    } else {
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    }
}

/// Builds JSON-RPC `thread/resume` params enforcing read-only text-only behavior on an existing
/// Codex session. Switchboard never calls `thread/start`.
pub fn thread_resume_params(thread_id: &str) -> Value {
    json!({
        "threadId": thread_id,
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

pub(crate) fn extract_thread_list(msg: &Value) -> Vec<CodexSessionInfo> {
    let Some(items) = msg
        .get("result")
        .and_then(|r| r.get("data"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    items
        .iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_str)?.to_string();
            let cwd = item
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let title = item
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| item.get("preview").and_then(Value::as_str))
                .unwrap_or("")
                .chars()
                .take(80)
                .collect();
            let updated_at = item
                .get("updatedAt")
                .and_then(Value::as_i64)
                .or_else(|| item.get("createdAt").and_then(Value::as_i64));
            Some(CodexSessionInfo {
                id,
                harness: HarnessKind::Codex,
                cwd,
                title,
                updated_at,
            })
        })
        .collect()
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
