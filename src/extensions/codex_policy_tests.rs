use serde_json::json;
use tempfile::tempdir;

use super::*;

#[test]
fn effective_codex_args_dynamically_disable_all_configured_mcp_servers() {
    let dir = tempdir().unwrap();
    let cfg = r#"
notify = ["/Applications/Example/SkyComputerUseClient"]
approval_policy = "never"

[mcp_servers.node_repl]
command = "/bin/echo"

[mcp_servers.linear-local]
command = "/bin/echo"

[mcp_servers."custom.mcp"]
command = "/bin/echo"
"#;
    std::fs::write(dir.path().join("config.toml"), cfg).unwrap();

    let base = default_codex_args();
    let effective = build_effective_codex_args(&base, Some(dir.path()));
    assert!(effective.contains(&"mcp_servers.node_repl.enabled=false".to_string()));
    assert!(effective.contains(&"mcp_servers.linear-local.enabled=false".to_string()));
    assert!(effective.contains(&"mcp_servers.\"custom.mcp\".enabled=false".to_string()));
    assert!(effective.contains(&"computer_use".to_string()));
    assert!(effective.contains(&"plugins".to_string()));
    assert!(effective.contains(&"shell_tool".to_string()));

    // Non-app-server test scripts must not receive injected `-c` flags.
    let script_args = vec!["fake_codex.py".to_string()];
    assert_eq!(
        build_effective_codex_args(&script_args, Some(dir.path())),
        script_args
    );
}

#[test]
fn thread_resume_and_turn_params_enforce_read_only_text_only_mode() {
    let resume = thread_resume_params("01a20e4a-a5b2-77e0-bd3f-2e46d42b8ebd");
    assert_eq!(resume["threadId"], "01a20e4a-a5b2-77e0-bd3f-2e46d42b8ebd");
    assert_eq!(resume["sandbox"], "read-only");
    assert_eq!(resume["approvalPolicy"], "on-request");
    assert!(resume["developerInstructions"]
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
