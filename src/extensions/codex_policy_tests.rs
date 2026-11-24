use serde_json::json;
use tempfile::tempdir;

use super::*;

#[test]
fn isolated_codex_home_writes_locked_down_text_only_config() {
    let dir = tempdir().unwrap();
    let home = dir.path().join("codex-home");
    prepare_isolated_codex_home(&home).unwrap();

    let cfg_text = std::fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(cfg_text.contains("notify = []"));
    assert!(cfg_text.contains("approval_policy = \"on-request\""));
    assert!(cfg_text.contains("sandbox_mode = \"read-only\""));
    assert!(cfg_text.contains("computer_use = false"));
    assert!(cfg_text.contains("browser_use = false"));
    assert!(cfg_text.contains("plugins = false"));
    assert!(cfg_text.contains("shell_tool = false"));
    assert!(cfg_text.contains("unified_exec = false"));
    assert!(!cfg_text.contains("mcp_servers"));
}

#[test]
fn thread_and_turn_params_enforce_read_only_text_only_mode() {
    let thread = thread_start_params(Path::new("/tmp/workspace"));
    assert_eq!(thread["sandbox"], "read-only");
    assert_eq!(thread["approvalPolicy"], "on-request");
    assert!(thread["developerInstructions"]
        .as_str()
        .unwrap()
        .contains("plain text only"));

    let turn = turn_start_params("thr_1", "hello peer");
    assert_eq!(turn["threadId"], "thr_1");
    assert_eq!(turn["approvalPolicy"], "on-request");
    assert_eq!(turn["sandboxPolicy"]["type"], "readOnly");
    assert_eq!(turn["sandboxPolicy"]["networkAccess"], false);
}

#[test]
fn server_approval_and_tool_requests_are_auto_declined() {
    let cmd_req = json!({
        "jsonrpc": "2.0",
        "id": 99,
        "method": "item/commandExecution/requestApproval",
        "params": { "command": "rm -rf /" }
    });
    let cmd_resp = denial_response_for_server_request(&cmd_req).unwrap();
    assert_eq!(cmd_resp["id"], 99);
    assert_eq!(cmd_resp["result"]["decision"], "decline");

    let file_req = json!({
        "jsonrpc": "2.0",
        "id": "req-2",
        "method": "item/fileChange/requestApproval",
        "params": {}
    });
    let file_resp = denial_response_for_server_request(&file_req).unwrap();
    assert_eq!(file_resp["result"]["decision"], "decline");

    let perm_req = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "item/permissions/requestApproval",
        "params": {}
    });
    let perm_resp = denial_response_for_server_request(&perm_req).unwrap();
    assert_eq!(perm_resp["result"]["permissions"], json!({}));

    let elicitation = json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "mcpServer/elicitation/request",
        "params": {}
    });
    let elicit_resp = denial_response_for_server_request(&elicitation).unwrap();
    assert_eq!(elicit_resp["result"]["action"], "decline");

    let unknown = json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "item/tool/call",
        "params": {}
    });
    let unknown_resp = denial_response_for_server_request(&unknown).unwrap();
    assert_eq!(unknown_resp["error"]["code"], -32601);

    let notification = json!({
        "jsonrpc": "2.0",
        "method": "item/agentMessage/delta",
        "params": { "delta": "hi" }
    });
    assert!(denial_response_for_server_request(&notification).is_none());

    let rpc_response = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": {}
    });
    assert!(denial_response_for_server_request(&rpc_response).is_none());
}
