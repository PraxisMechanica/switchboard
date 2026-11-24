use std::collections::HashSet;

use serde_json::json;

use super::*;

#[test]
fn detects_active_writer_error_messages() {
    assert!(is_active_writer_error(
        "thread/resume failed: thread 01a2 already has an active writer"
    ));
    assert!(!is_active_writer_error(
        "thread/resume failed: unknown thread"
    ));
}

#[test]
fn finds_only_new_completed_turn_matching_call_id() {
    let seen: HashSet<String> = ["turn-old".to_string()].into_iter().collect();
    let turns = vec![
        json!({
            "id": "turn-old",
            "status": "completed",
            "items": [
                {"type": "userMessage", "content": [{"type": "text", "text": "call: abc-123"}]},
                {"type": "agentMessage", "text": "stale reply"}
            ]
        }),
        json!({
            "id": "turn-in-progress",
            "status": "inProgress",
            "items": [
                {"type": "userMessage", "content": [{"type": "text", "text": "call: abc-123"}]},
                {"type": "agentMessage", "text": "partial"}
            ]
        }),
        json!({
            "id": "turn-other-call",
            "status": "completed",
            "items": [
                {"type": "userMessage", "content": [{"type": "text", "text": "call: zzz-999"}]},
                {"type": "agentMessage", "text": "wrong call"}
            ]
        }),
        json!({
            "id": "turn-match",
            "status": "completed",
            "items": [
                {"type": "userMessage", "content": [{"type": "text", "text": "Incoming switchboard message (call: abc-123)"}]},
                {"type": "agentMessage", "text": "QUEUE_DELIVERED_OK"}
            ]
        }),
    ];

    assert_eq!(
        find_completed_reply_turn(&turns, &seen, "abc-123").as_deref(),
        Some("QUEUE_DELIVERED_OK")
    );
    assert!(find_completed_reply_turn(&turns, &seen, "missing-call").is_none());
}
