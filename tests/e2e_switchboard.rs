//! End-to-end multi-peer integration tests covering real Iroh QUIC P2P,
//! signed Git phone book sync, active human approval, Codex App Server wake-up,
//! multi-turn thread resumption, and idempotent replay / conflict detection.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use switchboard::calls::{
    CallError, CallId, CallRequest, CallResponseStatus, CallStatus, MessageId,
};
use switchboard::daemon::{migrate_all, SwitchboardNode};
use switchboard::db::Database;
use switchboard::directory::{GitPhonebook, MemoryDirectory};
use switchboard::extensions::{CodexConfig, CodexHarness, ExtensionName, HarnessKind, StubHarness};
use switchboard::peers::{PeerStatus, RecordingNotifier};
use switchboard::primitives::{FixedClock, UuidV7Gen};
use switchboard::transport::{
    FrameKind, IrohTransport, MemoryHub, NetworkMode, PeerDialer, WireFrame,
};

const FAKE_CODEX_APP_SERVER: &str = r#"
import json
import sys

threads = {}
counter = 0

for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    req_id = msg.get("id")
    params = msg.get("params") or {}

    if method == "initialize":
        print(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": {"serverInfo": {"name": "fake-codex"}}}), flush=True)
    elif method == "initialized":
        continue
    elif method == "thread/start":
        counter += 1
        tid = f"codex-thread-{counter}"
        threads[tid] = []
        print(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": {"threadId": tid}}), flush=True)
    elif method == "thread/resume":
        tid = params["threadId"]
        threads.setdefault(tid, [])
        print(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": {"threadId": tid}}), flush=True)
    elif method == "turn/start":
        tid = params["threadId"]
        text = params["input"][0]["text"]
        history = threads.setdefault(tid, [])
        history.append(text)
        reply = f"thread={tid} turn={len(history)} ack={text.splitlines()[-1]}"
        print(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": {"turnId": f"t-{len(history)}"}}), flush=True)
        print(json.dumps({"jsonrpc": "2.0", "method": "item/completed", "params": {"item": {"type": "agentMessage", "text": reply}}}), flush=True)
        print(json.dumps({"jsonrpc": "2.0", "method": "turn/completed", "params": {"status": "completed"}}), flush=True)
    elif method == "turn/interrupt":
        print(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": {}}), flush=True)
        break
"#;

fn init_bare_git_repo(dir: &TempDir) -> PathBuf {
    let bare = dir.path().join("phonebook.git");
    std::fs::create_dir_all(&bare).unwrap();
    let status = Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(&bare)
        .status()
        .expect("git init --bare");
    assert!(status.success(), "git init --bare failed");
    bare
}

fn fake_codex_config(dir: &TempDir) -> CodexConfig {
    let script = dir.path().join("fake_codex_app_server.py");
    std::fs::write(&script, FAKE_CODEX_APP_SERVER).unwrap();
    CodexConfig {
        binary: PathBuf::from("python3"),
        args: vec![script.to_string_lossy().to_string()],
        turn_timeout: Duration::from_secs(15),
        codex_home: None,
    }
}

fn vpn_loopback_mode() -> NetworkMode {
    NetworkMode::VpnOnly {
        bind_addr: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
        custom_relays: Vec::new(),
    }
}

#[tokio::test]
async fn full_iroh_git_phonebook_human_approval_and_codex_multi_turn_e2e() {
    let tmp = TempDir::new().unwrap();
    let bare_repo = init_bare_git_repo(&tmp);
    let remote_url = bare_repo.to_string_lossy().to_string();
    let codex_cfg = fake_codex_config(&tmp);
    let workspace = tmp.path().join("bob-api-repo");
    std::fs::create_dir_all(&workspace).unwrap();

    // --- Setup Bob ---
    let bob_db = Database::open_in_memory().unwrap();
    migrate_all(&bob_db).unwrap();
    let bob_clock = FixedClock::new(1_700_000_000_000);
    let bob_id = switchboard::identity::IdentityService::new(bob_db.clone(), bob_clock)
        .init("bob", false)
        .await
        .unwrap();
    let bob_transport = IrohTransport::bind(&bob_id.keypair, vpn_loopback_mode())
        .await
        .unwrap();
    let bob_ep = bob_transport.local_endpoint();
    let bob_addrs: Vec<String> = bob_ep
        .direct_addrs
        .iter()
        .map(ToString::to_string)
        .collect();
    let bob_notifier = RecordingNotifier::new();
    let bob = Arc::new(SwitchboardNode::new(
        bob_db,
        bob_transport,
        GitPhonebook::new(remote_url.clone(), tmp.path().join("bob-pb")),
        CodexHarness::new(codex_cfg.clone()),
        FixedClock::new(1_700_000_000_000),
        UuidV7Gen,
        bob_notifier.clone(),
    ));

    // --- Setup Alice ---
    let alice_db = Database::open_in_memory().unwrap();
    migrate_all(&alice_db).unwrap();
    let alice_clock = FixedClock::new(1_700_000_000_000);
    let alice_id = switchboard::identity::IdentityService::new(alice_db.clone(), alice_clock)
        .init("alice", false)
        .await
        .unwrap();
    let alice_transport = IrohTransport::bind(&alice_id.keypair, vpn_loopback_mode())
        .await
        .unwrap();
    let alice_ep = alice_transport.local_endpoint();
    let alice_addrs: Vec<String> = alice_ep
        .direct_addrs
        .iter()
        .map(ToString::to_string)
        .collect();
    let alice = Arc::new(SwitchboardNode::new(
        alice_db,
        alice_transport,
        GitPhonebook::new(remote_url, tmp.path().join("alice-pb")),
        CodexHarness::new(codex_cfg),
        FixedClock::new(1_700_000_000_000),
        UuidV7Gen,
        RecordingNotifier::new(),
    ));

    // Start both daemon accept loops
    let cancel = CancellationToken::new();
    let bob_loop = tokio::spawn(Arc::clone(&bob).run_accept_loop(cancel.child_token()));
    let alice_loop = tokio::spawn(Arc::clone(&alice).run_accept_loop(cancel.child_token()));

    // 1. Bob registers an extension and both publish signed peer cards to the Git phone book.
    bob.extensions
        .add(
            ExtensionName::new("api").unwrap(),
            workspace,
            "Orders HTTP API".to_string(),
            HarnessKind::Codex,
        )
        .await
        .unwrap();
    bob.publish_directory_card(bob_addrs).await.unwrap();
    alice.publish_directory_card(alice_addrs).await.unwrap();

    // 2. Both sync the Git phone book and discover each other.
    let alice_sync = alice.directory.sync().await.unwrap();
    assert_eq!(alice_sync.synced, 2);
    assert_eq!(alice_sync.rejected, 0);
    let bob_sync = bob.directory.sync().await.unwrap();
    assert_eq!(bob_sync.synced, 2);

    // 3. Unapproved call directly over QUIC MUST be rejected before touching Codex.
    let unauthorized_req = CallRequest {
        call_id: CallId::from_uuid(uuid::Uuid::now_v7()),
        message_id: MessageId::from_uuid(uuid::Uuid::now_v7()),
        extension: ExtensionName::new("api").unwrap(),
        body: "unauthorized probe".to_string(),
        expects_reply: true,
    };
    let rejected_frame = alice
        .transport
        .request(
            &bob_ep,
            WireFrame::new(
                FrameKind::CallRequest,
                serde_json::to_value(&unauthorized_req).unwrap(),
            ),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(rejected_frame.kind, FrameKind::Error);

    // 4. Alice knocks Bob -> Bob receives human approval notification.
    let pending_bob = alice
        .knock_peer(
            "bob",
            "API contract coordination".to_string(),
            None,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(matches!(
        pending_bob.status,
        PeerStatus::PendingOutbound { .. }
    ));
    assert_eq!(bob_notifier.len(), 1);
    assert_eq!(bob_notifier.recorded()[0].handle.as_str(), "alice");

    // 5. Bob's human actively approves Alice -> both sides become Approved.
    let approved_alice = bob
        .approve_peer("alice", None, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(approved_alice.status, PeerStatus::Approved);
    assert!(alice.peers.authorize(bob_ep.peer_id).await.is_ok());

    // 6. Turn 1: Alice calls `bob/api` -> wakes Bob's Codex App Server via `thread/start`.
    let turn1 = alice
        .call_extension(
            "bob/api",
            "What fields does POST /orders require?".to_string(),
            None,
            Duration::from_secs(15),
        )
        .await
        .unwrap();
    assert_eq!(turn1.status, CallResponseStatus::Replied);
    assert!(turn1.body.contains("thread=codex-thread-1"));
    assert!(turn1.body.contains("turn=1"));

    // 7. Turn 2: Alice replies on the same CallId -> Bob's Codex App Server runs `thread/resume`
    //    with the persisted session ID (`codex-thread-1`).
    let turn2 = alice
        .reply_call(
            turn1.call_id,
            "Does that also apply for EUR?".to_string(),
            true,
            None,
            Duration::from_secs(15),
        )
        .await
        .unwrap();
    assert_eq!(turn2.status, CallResponseStatus::Replied);
    assert!(turn2.body.contains("thread=codex-thread-1"));
    assert!(turn2.body.contains("turn=1") || turn2.body.contains("turn=2"));

    // Confirm Bob persisted the exact Codex thread ID on the call record.
    let (bob_call, bob_msgs) = bob.calls.history(turn1.call_id).await.unwrap();
    assert_eq!(
        bob_call.harness_session_id.as_deref(),
        Some("codex-thread-1")
    );
    assert_eq!(bob_msgs.len(), 2);

    let (alice_call, alice_msgs) = alice.calls.history(turn1.call_id).await.unwrap();
    assert_eq!(alice_call.status, CallStatus::Closed);
    assert_eq!(alice_msgs.len(), 2);

    cancel.cancel();
    let _ = bob_loop.await;
    let _ = alice_loop.await;
}

#[tokio::test]
async fn inbound_call_idempotency_replays_cached_reply_and_rejects_mismatched_payload() {
    let hub = MemoryHub::new();

    let bob_db = Database::open_in_memory().unwrap();
    migrate_all(&bob_db).unwrap();
    let bob_id = switchboard::identity::IdentityService::new(bob_db.clone(), FixedClock::new(1))
        .init("bob", false)
        .await
        .unwrap();
    let bob_transport = hub.register(bob_id.keypair.peer_id());
    let stub = StubHarness::with_replies(["cached-reply-once", "must-never-be-used"]);

    let bob = SwitchboardNode::new(
        bob_db,
        bob_transport,
        MemoryDirectory::new(),
        stub.clone(),
        FixedClock::new(100),
        UuidV7Gen,
        RecordingNotifier::new(),
    );

    bob.extensions
        .add(
            ExtensionName::new("api").unwrap(),
            PathBuf::from("/tmp/ws"),
            "API".to_string(),
            HarnessKind::Codex,
        )
        .await
        .unwrap();

    // Approve Alice on Bob's node
    let alice_kp = switchboard::identity::LocalKeypair::generate();
    let signed_knock = switchboard::identity::sign(
        &alice_kp,
        switchboard::peers::Knock {
            from_handle: "alice".to_string(),
            reason: "sync".to_string(),
            sent_at: switchboard::primitives::Timestamp::from_millis(100),
        },
    );
    let verified_knock = switchboard::identity::verify(signed_knock).unwrap();
    bob.peers
        .handle_inbound_knock(verified_knock)
        .await
        .unwrap();
    bob.peers.approve_by_handle("alice").await.unwrap();
    let caller = bob.peers.authorize(alice_kp.peer_id()).await.unwrap();

    let call_id = CallId::from_uuid(uuid::Uuid::now_v7());
    let msg_id = MessageId::from_uuid(uuid::Uuid::now_v7());
    let req = CallRequest {
        call_id,
        message_id: msg_id,
        extension: ExtensionName::new("api").unwrap(),
        body: "exact same question".to_string(),
        expects_reply: true,
    };

    // First delivery wakes the harness once.
    let first = bob
        .calls
        .handle_inbound_call(caller.clone(), req.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(first.body, "cached-reply-once");
    assert_eq!(stub.recorded_requests().len(), 1);

    // Exact replay returns cached receipt without re-invoking the harness.
    let replay = bob
        .calls
        .handle_inbound_call(caller.clone(), req.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(replay, first);
    assert_eq!(stub.recorded_requests().len(), 1);

    // Reusing the same MessageId with a different payload rejects with PayloadMismatch.
    let mismatched = CallRequest {
        body: "different question with same message id".to_string(),
        ..req
    };
    let err = bob
        .calls
        .handle_inbound_call(caller, mismatched, CancellationToken::new())
        .await;
    assert!(matches!(err, Err(CallError::PayloadMismatch(id)) if id == msg_id));
    assert_eq!(stub.recorded_requests().len(), 1);
}
