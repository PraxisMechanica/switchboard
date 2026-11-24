//! Deterministic in-memory stub harness and shared [`AgentHarness`] conformance suite.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::model::{DeliveryOutcome, DeliveryRequest, ExtensionName};
use super::{AgentHarness, ExtensionError};

/// Prompt marker used by [`verify_harness`] to exercise mid-turn cancellation.
pub const CONFORMANCE_SLOW_MARKER: &str = "CONFORMANCE_SLOW";

#[derive(Debug, Default)]
struct StubState {
    scripted_replies: VecDeque<String>,
    delay: Duration,
    next_session: u64,
    requests: Vec<DeliveryRequest>,
    sessions: Vec<String>,
}

/// Deterministic in-memory [`AgentHarness`] for unit tests and conformance verification.
#[derive(Debug, Clone, Default)]
pub struct StubHarness {
    state: Arc<Mutex<StubState>>,
}

impl StubHarness {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_replies<I, S>(replies: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let scripted_replies = replies.into_iter().map(Into::into).collect();
        Self {
            state: Arc::new(Mutex::new(StubState {
                scripted_replies,
                ..StubState::default()
            })),
        }
    }

    pub fn with_delay(self, delay: Duration) -> Self {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.delay = delay;
        drop(guard);
        self
    }

    pub fn push_reply(&self, reply: impl Into<String>) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.scripted_replies.push_back(reply.into());
    }

    pub fn recorded_requests(&self) -> Vec<DeliveryRequest> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.requests.clone()
    }

    pub fn seen_sessions(&self) -> Vec<String> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.sessions.clone()
    }
}

#[async_trait::async_trait]
impl AgentHarness for StubHarness {
    async fn deliver(
        &self,
        req: DeliveryRequest,
        cancel: CancellationToken,
    ) -> Result<DeliveryOutcome, ExtensionError> {
        if cancel.is_cancelled() {
            return Ok(DeliveryOutcome::Cancelled);
        }

        let configured_delay = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .delay;

        let effective_delay = if req.body.contains(CONFORMANCE_SLOW_MARKER) {
            configured_delay.max(Duration::from_secs(5))
        } else {
            configured_delay
        };

        if !effective_delay.is_zero() {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(DeliveryOutcome::Cancelled),
                () = tokio::time::sleep(effective_delay) => {}
            }
        }

        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let session_id = match req.existing_session_id.clone() {
            Some(existing) => existing,
            None => {
                guard.next_session += 1;
                format!("stub-session-{}", guard.next_session)
            }
        };

        guard.sessions.push(session_id.clone());
        let reply = guard
            .scripted_replies
            .pop_front()
            .unwrap_or_else(|| format!("stub-reply[{session_id}]: {}", req.body));
        guard.requests.push(req);

        Ok(DeliveryOutcome::Replied {
            body: reply,
            session_id: Some(session_id),
        })
    }
}

/// Verifies that any [`AgentHarness`] satisfies initial delivery, session resume, and cancellation.
pub async fn verify_harness<H: AgentHarness>(harness: &H, workspace: &Path) {
    let ext = ExtensionName::new("conformance").expect("valid static extension name");

    // (a) Initial delivery returns Replied with non-empty body and Some(session_id).
    let first = harness
        .deliver(
            DeliveryRequest {
                call_id: Uuid::now_v7(),
                extension: ext.clone(),
                workspace_dir: workspace.to_path_buf(),
                from_handle: "alice".to_string(),
                body: "hello from conformance test".to_string(),
                existing_session_id: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("initial delivery must succeed");

    let session_id = match first {
        DeliveryOutcome::Replied { body, session_id } => {
            assert!(
                !body.trim().is_empty(),
                "initial reply body must be non-empty"
            );
            let sid = session_id.expect("initial reply must return Some(session_id)");
            assert!(!sid.trim().is_empty(), "session_id must be non-empty");
            sid
        }
        other => panic!("expected DeliveryOutcome::Replied, got {other:?}"),
    };

    // (b) Follow-up delivery with existing_session_id resumes that exact session.
    let followup = harness
        .deliver(
            DeliveryRequest {
                call_id: Uuid::now_v7(),
                extension: ext.clone(),
                workspace_dir: workspace.to_path_buf(),
                from_handle: "alice".to_string(),
                body: "follow-up turn".to_string(),
                existing_session_id: Some(session_id.clone()),
            },
            CancellationToken::new(),
        )
        .await
        .expect("follow-up delivery must succeed");

    match followup {
        DeliveryOutcome::Replied {
            body,
            session_id: resumed,
        } => {
            assert!(
                !body.trim().is_empty(),
                "follow-up reply body must be non-empty"
            );
            assert_eq!(
                resumed.as_deref(),
                Some(session_id.as_str()),
                "follow-up must resume exact session_id"
            );
        }
        other => panic!("expected DeliveryOutcome::Replied on resume, got {other:?}"),
    }

    // (c) Cancellation via CancellationToken returns DeliveryOutcome::Cancelled.
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        trigger.cancel();
    });

    let cancelled = harness
        .deliver(
            DeliveryRequest {
                call_id: Uuid::now_v7(),
                extension: ext,
                workspace_dir: workspace.to_path_buf(),
                from_handle: "alice".to_string(),
                body: format!("{CONFORMANCE_SLOW_MARKER}: hold turn for cancellation"),
                existing_session_id: Some(session_id),
            },
            cancel,
        )
        .await
        .expect("cancelled delivery must return Ok(Cancelled)");

    let _ = cancel_task.await;
    assert_eq!(
        cancelled,
        DeliveryOutcome::Cancelled,
        "expected DeliveryOutcome::Cancelled"
    );
}

#[cfg(test)]
mod tests {
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
    return {"counter": 0, "threads": []}

def save_state(state):
    with open(state_path, "w", encoding="utf-8") as f:
        json.dump(state, f)

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
        if not initialized:
            send({"jsonrpc": "2.0", "id": msg_id, "error": {"code": -32002, "message": "not initialized"}})
            sys.exit(1)
        state = load_state()
        state["counter"] += 1
        tid = f"thr_{state['counter']}"
        state["threads"].append(tid)
        save_state(state)
        active_thread = tid
        send({"jsonrpc": "2.0", "id": msg_id, "result": {"thread": {"id": tid}}})
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

    fn make_req(dir: &Path, body: &str, existing_session_id: Option<String>) -> DeliveryRequest {
        DeliveryRequest {
            call_id: Uuid::now_v7(),
            extension: ExtensionName::new("backend").unwrap(),
            workspace_dir: dir.to_path_buf(),
            from_handle: "alice".to_string(),
            body: body.to_string(),
            existing_session_id,
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
            None,
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

        let delta_only = harness
            .deliver(
                make_req(dir.path(), "FORCE_DELTA_ONLY", None),
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
                make_req(dir.path(), "FORCE_CRASH", None),
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
                    None,
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(timed_out, DeliveryOutcome::TimedOut);
    }
}
