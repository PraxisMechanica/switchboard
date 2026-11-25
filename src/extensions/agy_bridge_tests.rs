use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    deliver_via_app_bridge, find_reply_in_transcript, install_agy_sidecar_in, max_step_index,
    run_agy_bridge_server,
};
use crate::extensions::model::{DeliveryOutcome, DeliveryRequest, ExtensionName, HarnessKind};

#[test]
fn finds_final_planner_reply_after_call_id_and_expands_truncated_content() {
    let dir = tempfile::tempdir().unwrap();
    let compact = dir.path().join("transcript.jsonl");
    let full = dir.path().join("transcript_full.jsonl");
    let call_id = Uuid::now_v7();

    std::fs::write(
        &compact,
        format!(
            concat!(
                r#"{{"step_index":10,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"old reply"}}"#, "\n",
                r#"{{"step_index":11,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"Incoming call: {call_id}"}}"#, "\n",
                r#"{{"step_index":12,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{{"name":"read_file"}}],"content":"thinking"}}"#, "\n",
                r#"{{"step_index":13,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"short...", "truncated_fields":["content"]}}"#, "\n",
            ),
            call_id = call_id
        ),
    )
    .unwrap();
    std::fs::write(
        &full,
        r#"{"step_index":13,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"Full untruncated reply from Antigravity App."}"#,
    )
    .unwrap();

    assert_eq!(max_step_index(&compact), Some(13));
    let reply = find_reply_in_transcript(&compact, &full, 10, &call_id.to_string());
    assert_eq!(
        reply.as_deref(),
        Some("Full untruncated reply from Antigravity App.")
    );
}

#[test]
fn installs_sidecar_manifest_and_merges_config_json_without_clobbering() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.json");
    std::fs::write(
        &config_path,
        r#"{"plugins":{"praxis-coding":{"enabled":true}},"sidecars":{"other":{"enabled":true}}}"#,
    )
    .unwrap();

    let manifest = install_agy_sidecar_in(
        dir.path(),
        std::path::Path::new("/usr/local/bin/switchboard"),
    )
    .unwrap();
    assert!(manifest.is_file());

    let updated: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(updated["plugins"]["praxis-coding"]["enabled"], true);
    assert_eq!(updated["sidecars"]["other"]["enabled"], true);
    assert_eq!(updated["sidecars"]["switchboard-bridge"]["enabled"], true);
}

#[tokio::test]
async fn bridge_roundtrip_invokes_agentapi_and_reads_transcript_reply() {
    let dir = tempfile::tempdir().unwrap();
    let conv_id = Uuid::now_v7().to_string();
    let logs_dir = dir
        .path()
        .join("brain")
        .join(&conv_id)
        .join(".system_generated")
        .join("logs");
    std::fs::create_dir_all(&logs_dir).unwrap();
    let transcript = logs_dir.join("transcript.jsonl");
    std::fs::write(
        &transcript,
        r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"earlier"}"#,
    )
    .unwrap();

    let fake_agentapi = dir.path().join("agentapi");
    let script = format!(
        "#!/bin/sh\nset -eu\n[ \"$1\" = \"send-message\" ]\nCONV=\"$2\"\nPROMPT=\"$3\"\nTRANSCRIPT=\"{}\"\nprintf '\\n{{\"step_index\":2,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"content\":\"%s\"}}\\n' \"$(printf '%s' \"$PROMPT\" | tr '\\n' ' ' | sed 's/\"/\\\\\"/g')\" >> \"$TRANSCRIPT\"\nprintf '{{\"step_index\":3,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"content\":\"hello back from app\"}}\\n' >> \"$TRANSCRIPT\"\n",
        transcript.display()
    );
    std::fs::write(&fake_agentapi, script).unwrap();
    std::fs::set_permissions(&fake_agentapi, std::fs::Permissions::from_mode(0o755)).unwrap();

    let sock = dir.path().join("agy-bridge.sock");
    let server_sock = sock.clone();
    let server_bin = fake_agentapi.clone();
    let server = tokio::spawn(async move {
        let _ = run_agy_bridge_server(&server_sock, &server_bin).await;
    });

    for _ in 0..40 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let req = DeliveryRequest {
        call_id: Uuid::now_v7(),
        extension: ExtensionName::new(&format!("agy.{conv_id}")).unwrap(),
        harness: HarnessKind::Agy,
        workspace_dir: dir.path().to_path_buf(),
        from_handle: "dante".to_string(),
        body: "ping".to_string(),
        session_id: conv_id.clone(),
    };
    let prompt = format!("Incoming call: {} ping", req.call_id);
    let outcome = deliver_via_app_bridge(
        &sock,
        &dir.path().join("brain"),
        &req,
        &prompt,
        Duration::from_secs(5),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    server.abort();
    assert_eq!(
        outcome,
        DeliveryOutcome::Replied {
            body: "hello back from app".to_string(),
            session_id: Some(conv_id),
        }
    );
}
