use std::path::PathBuf;

use super::super::codex::{CodexConfig, CodexHarness};
use super::*;

const FAKE_CODEX_SCRIPT: &str = r#"
import json, os, select, sys, time

state_path, event_log_path = sys.argv[1], sys.argv[2]
fd, buf = sys.stdin.fileno(), b""
initialized, active_thread = False, None

def load_state():
    if os.path.exists(state_path):
        with open(state_path, "r", encoding="utf-8") as f:
            return json.load(f)
    return {"threads": ["019a20e4-80cc-7754-b584-33393adcc1f5"]}

def log_event(name):
    with open(event_log_path, "a", encoding="utf-8") as f:
        f.write(name + "\n")

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

def read_line(timeout=None):
    global buf
    while True:
        nl = buf.find(b"\n")
        if nl != -1:
            line = buf[:nl].decode("utf-8", errors="replace")
            buf = buf[nl + 1:]
            return line
        if timeout is not None and not select.select([fd], [], [], timeout)[0]:
            return None
        chunk = os.read(fd, 4096)
        if not chunk:
            if buf:
                line, buf = buf.decode("utf-8", errors="replace"), b""
                return line
            return ""
        buf += chunk

while True:
    raw = read_line()
    if raw == "":
        break
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    method, msg_id, params = msg.get("method"), msg.get("id"), msg.get("params") or {}

    if method == "initialize":
        if (params.get("clientInfo") or {}).get("name") != "switchboard":
            send({"jsonrpc": "2.0", "id": msg_id, "error": {"code": -32602, "message": "bad clientInfo"}})
            sys.exit(1)
        send({"jsonrpc": "2.0", "id": msg_id, "result": {"userAgent": "fake-codex/0.1.0"}})
    elif method == "initialized":
        initialized = True
    elif method == "thread/start":
        send({"jsonrpc": "2.0", "id": msg_id, "error": {"code": -32601, "message": "thread/start forbidden"}})
        sys.exit(1)
    elif method == "thread/resume":
        tid = params.get("threadId")
        if not initialized or tid not in load_state()["threads"]:
            send({"jsonrpc": "2.0", "id": msg_id, "error": {"code": -32004, "message": "unknown thread"}})
            sys.exit(1)
        active_thread = tid
        send({"jsonrpc": "2.0", "id": msg_id, "result": {"threadId": tid}})
    elif method == "turn/start":
        tid = params.get("threadId") or active_thread
        items = params.get("input") or []
        prompt = items[0].get("text", "") if items else ""
        send({"jsonrpc": "2.0", "id": msg_id, "result": {"turn": {"id": "turn_1", "status": "inProgress"}}})

        if "FORCE_CRASH" in prompt:
            sys.stderr.write("simulated fatal crash\n")
            sys.stderr.flush()
            sys.exit(42)

        if "CONFORMANCE_SLOW" in prompt:
            log_event("turn_started")
            deadline = time.monotonic() + 10.0
            while time.monotonic() < deadline:
                follow = read_line(timeout=0.05)
                if follow is None:
                    continue
                if follow == "":
                    sys.exit(0)
                if follow.strip():
                    fmsg = json.loads(follow.strip())
                    if fmsg.get("method") == "turn/interrupt":
                        log_event("interrupted")
                        send({"jsonrpc": "2.0", "id": fmsg.get("id"), "result": {}})
                        sys.exit(0)
            sys.exit(0)

        if "FORCE_DELTA_ONLY" in prompt:
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {"threadId": tid, "delta": "part-1 "}})
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {"threadId": tid, "text": "part-2"}})
            send({"jsonrpc": "2.0", "method": "turn/completed", "params": {"threadId": tid, "turn": {"id": "turn_1", "status": "completed"}}})
        else:
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {"threadId": tid, "delta": "draft "}})
            send({"jsonrpc": "2.0", "method": "item/completed", "params": {"threadId": tid, "item": {"id": "item_1", "type": "agentMessage", "text": f"authoritative reply for {tid}"}}})
            send({"jsonrpc": "2.0", "method": "turn/completed", "params": {"threadId": tid, "turn": {"id": "turn_1", "status": "completed"}}})
    elif method == "turn/interrupt":
        log_event("interrupted")
        send({"jsonrpc": "2.0", "id": msg_id, "result": {}})
        sys.exit(0)
"#;

fn write_fake_codex(dir: &Path, timeout: Duration) -> (CodexHarness, PathBuf) {
    let script_path = dir.join("fake_codex_app_server.py");
    let state_path = dir.join("state.json");
    let event_log_path = dir.join("events.log");
    std::fs::write(&script_path, FAKE_CODEX_SCRIPT).expect("write fake codex script");

    let config = CodexConfig {
        binary: PathBuf::from("python3"),
        args: vec![
            script_path.to_string_lossy().into_owned(),
            state_path.to_string_lossy().into_owned(),
            event_log_path.to_string_lossy().into_owned(),
        ],
        turn_timeout: timeout,
        codex_home: None,
    };
    (CodexHarness::new(config), event_log_path)
}

fn make_req(dir: &Path, body: &str, session_id: &str) -> DeliveryRequest {
    DeliveryRequest {
        call_id: Uuid::now_v7(),
        extension: ExtensionName::new("codex.backend").unwrap(),
        harness: HarnessKind::Codex,
        workspace_dir: dir.to_path_buf(),
        from_handle: "alice".to_string(),
        body: body.to_string(),
        session_id: session_id.to_string(),
    }
}

#[tokio::test]
async fn stub_harness_passes_conformance() {
    let dir = tempfile::tempdir().unwrap();
    verify_harness(&StubHarness::new(), dir.path()).await;
}

#[tokio::test]
async fn codex_harness_passes_conformance() {
    let dir = tempfile::tempdir().unwrap();
    let (harness, _) = write_fake_codex(dir.path(), Duration::from_secs(10));
    verify_harness(&harness, dir.path()).await;
}

#[tokio::test]
async fn codex_harness_sends_turn_interrupt_on_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let (harness, event_log) = write_fake_codex(dir.path(), Duration::from_secs(10));

    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let log_for_wait = event_log.clone();
    let waiter = tokio::spawn(async move {
        for _ in 0..100 {
            if std::fs::read_to_string(&log_for_wait)
                .is_ok_and(|contents| contents.contains("turn_started"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        trigger.cancel();
    });

    let req = make_req(
        dir.path(),
        &format!("{CONFORMANCE_SLOW_MARKER}: wait"),
        "019a20e4-80cc-7754-b584-33393adcc1f5",
    );
    let outcome = harness.deliver(req, cancel).await.unwrap();
    let _ = waiter.await;

    assert_eq!(outcome, DeliveryOutcome::Cancelled);
    let events = std::fs::read_to_string(&event_log).unwrap_or_default();
    assert!(
        events.contains("interrupted"),
        "expected turn/interrupt to be delivered, got events: {events:?}"
    );
}

#[tokio::test]
async fn codex_harness_handles_deltas_crash_and_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let (harness, _) = write_fake_codex(dir.path(), Duration::from_secs(10));

    let sid = "019a20e4-80cc-7754-b584-33393adcc1f5";
    let delta_only = harness
        .deliver(
            make_req(dir.path(), "FORCE_DELTA_ONLY", sid),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        delta_only,
        DeliveryOutcome::Replied { ref body, session_id: Some(_) } if body == "part-1 part-2"
    ));

    let crash = harness
        .deliver(
            make_req(dir.path(), "FORCE_CRASH", sid),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    match crash {
        DeliveryOutcome::ProcessFailed { exit_code, detail } => {
            assert_eq!(exit_code, Some(42));
            assert!(detail.contains("simulated fatal crash"), "detail={detail}");
        }
        other => panic!("expected ProcessFailed, got {other:?}"),
    }

    let (short_harness, _) = write_fake_codex(dir.path(), Duration::from_millis(150));
    let timed_out = short_harness
        .deliver(
            make_req(
                dir.path(),
                &format!("{CONFORMANCE_SLOW_MARKER}: timeout"),
                sid,
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(timed_out, DeliveryOutcome::TimedOut);
}
