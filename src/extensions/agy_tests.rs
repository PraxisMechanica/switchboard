use tempfile::tempdir;

use super::*;

#[test]
fn parses_agy_json_response_and_falls_back_to_plain_text() {
    let json_stdout = r#"{"conversation_id":"50daf777-b8cf-4e72-83ce-f0c7ad712630","status":"SUCCESS","response":"QUARTZ\n"}"#;
    assert_eq!(parse_agy_json_reply(json_stdout), "QUARTZ");
    assert_eq!(parse_agy_json_reply("plain answer\n"), "plain answer");
}

#[test]
fn discovers_uuid_directories_in_agy_brain_dir() {
    let dir = tempdir().unwrap();
    std::fs::create_dir(dir.path().join("tempmediaStorage")).unwrap();
    std::fs::create_dir(dir.path().join("2a61c58a-8e52-46df-8bb0-784f67dd5758")).unwrap();
    std::fs::create_dir(dir.path().join("974c7cae-541e-4bd8-8bcd-7204bfa77330")).unwrap();

    let found = discover_agy_brain_sessions(dir.path(), 10);
    assert_eq!(found.len(), 2);
    assert!(found.iter().all(|s| s.harness == HarnessKind::Agy));
    assert!(found
        .iter()
        .any(|s| s.id == "2a61c58a-8e52-46df-8bb0-784f67dd5758"));
}
