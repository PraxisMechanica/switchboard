//! Process-level end-to-end test spawning two real `switchboard daemon` binaries
//! and driving the full CLI + Unix IPC + Iroh QUIC + GitPhonebook + Codex App Server flow.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const EXISTING_SESSION_ID: &str = "019a20e4-80cc-7754-b584-33393adcc1f5";

const FAKE_CODEX_SCRIPT: &str = r#"
import json
import sys

threads = {"019a20e4-80cc-7754-b584-33393adcc1f5": []}

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
        print(json.dumps({"jsonrpc": "2.0", "id": req_id, "error": {"code": -32601, "message": "thread/start forbidden"}}), flush=True)
        sys.exit(1)
    elif method == "thread/resume":
        tid = params["threadId"]
        if tid not in threads:
            print(json.dumps({"jsonrpc": "2.0", "id": req_id, "error": {"code": -32004, "message": "unknown thread"}}), flush=True)
            sys.exit(1)
        print(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": {"threadId": tid}}), flush=True)
    elif method == "turn/start":
        tid = params["threadId"]
        text = params["input"][0]["text"]
        turns = threads.setdefault(tid, [])
        turns.append(text)
        reply = f"ok:{tid}:{text.splitlines()[-1]}"
        print(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": {"turnId": f"turn-{len(turns)}"}}), flush=True)
        print(json.dumps({"jsonrpc": "2.0", "method": "item/completed", "params": {"item": {"type": "agentMessage", "text": reply}}}), flush=True)
        print(json.dumps({"jsonrpc": "2.0", "method": "turn/completed", "params": {"status": "completed"}}), flush=True)
    elif method == "turn/interrupt":
        print(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": {}}), flush=True)
        break
"#;

struct DaemonGuard(Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_switchboard")
}

fn run_cli(cfg: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .arg("--config")
        .arg(cfg)
        .args(args)
        .output()
        .expect("spawn switchboard CLI")
}

fn run_cli_ok(cfg: &Path, args: &[&str]) -> String {
    let out = run_cli(cfg, args);
    assert!(
        out.status.success(),
        "CLI {:?} failed (status {:?})\nstdout: {}\nstderr: {}",
        args,
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn write_node_config(cfg_path: &Path, data_dir: &Path, git_remote: &Path, fake_codex: &Path) {
    let toml_body = format!(
        r#"data_dir = "{data_dir}"
transport = "iroh"

[network.VpnOnly]
bind_addr = "127.0.0.1:0"
custom_relays = []

[directory]
git_remote = "{git_remote}"

[codex]
binary = "python3"
args = ["{fake_codex}"]
turn_timeout_secs = 20
"#,
        data_dir = data_dir.display(),
        git_remote = git_remote.display(),
        fake_codex = fake_codex.display(),
    );
    std::fs::write(cfg_path, toml_body).unwrap();
}

fn wait_for_socket(socket_path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if socket_path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "daemon socket {} did not appear within {:?}",
        socket_path.display(),
        timeout
    );
}

#[test]
fn two_real_daemons_complete_full_cli_lifecycle_including_revocation() {
    let tmp = TempDir::new().unwrap();
    let bare_repo = tmp.path().join("phonebook.git");
    std::fs::create_dir_all(&bare_repo).unwrap();
    let git_init = Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(&bare_repo)
        .status()
        .expect("git init --bare");
    assert!(git_init.success());

    let fake_codex = tmp.path().join("fake_codex.py");
    std::fs::write(&fake_codex, FAKE_CODEX_SCRIPT).unwrap();

    let bob_ws = tmp.path().join("bob-api-ws");
    std::fs::create_dir_all(&bob_ws).unwrap();

    let alice_dir = tmp.path().join("alice-data");
    let bob_dir = tmp.path().join("bob-data");
    let alice_cfg = tmp.path().join("alice.toml");
    let bob_cfg = tmp.path().join("bob.toml");

    write_node_config(&alice_cfg, &alice_dir, &bare_repo, &fake_codex);
    write_node_config(&bob_cfg, &bob_dir, &bare_repo, &fake_codex);

    // 1. Initialize both identities and verify whoami --json.
    run_cli_ok(&alice_cfg, &["init", "--name", "alice"]);
    run_cli_ok(&bob_cfg, &["init", "--name", "bob"]);
    let alice_who: serde_json::Value =
        serde_json::from_str(&run_cli_ok(&alice_cfg, &["whoami", "--json"])).unwrap();
    let bob_who: serde_json::Value =
        serde_json::from_str(&run_cli_ok(&bob_cfg, &["whoami", "--json"])).unwrap();
    assert_eq!(alice_who["handle"], "alice");
    assert_eq!(bob_who["handle"], "bob");

    // 2. Register Bob's callable extension bound to a pre-existing session.
    run_cli_ok(
        &bob_cfg,
        &[
            "ext",
            "add",
            "api",
            "--dir",
            bob_ws.to_str().unwrap(),
            "--desc",
            "Orders HTTP API",
            "--session",
            EXISTING_SESSION_ID,
        ],
    );

    // 3. Launch two real `switchboard daemon` child processes.
    let _alice_daemon = DaemonGuard(
        Command::new(bin())
            .env("SWITCHBOARD_HEADLESS", "1")
            .arg("--config")
            .arg(&alice_cfg)
            .arg("daemon")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn alice daemon"),
    );
    let _bob_daemon = DaemonGuard(
        Command::new(bin())
            .env("SWITCHBOARD_HEADLESS", "1")
            .arg("--config")
            .arg(&bob_cfg)
            .arg("daemon")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn bob daemon"),
    );
    wait_for_socket(&alice_dir.join("daemon.sock"), Duration::from_secs(10));
    wait_for_socket(&bob_dir.join("daemon.sock"), Duration::from_secs(10));

    // 4. Both publish signed PeerCards (including live bound direct_addrs) and sync.
    run_cli_ok(&bob_cfg, &["directory", "publish"]);
    run_cli_ok(&alice_cfg, &["directory", "publish"]);
    run_cli_ok(&alice_cfg, &["directory", "sync"]);
    run_cli_ok(&bob_cfg, &["directory", "sync"]);

    let dir_list: serde_json::Value =
        serde_json::from_str(&run_cli_ok(&alice_cfg, &["directory", "ls", "--json"])).unwrap();
    assert_eq!(dir_list.as_array().unwrap().len(), 2);

    // 5. Calling before approval must fail cleanly.
    let pre_approval = run_cli(&alice_cfg, &["call", "bob/api", "hello", "--json"]);
    assert!(!pre_approval.status.success());

    // 6. Alice dials Bob -> Bob sees ringing line -> Bob approves Alice.
    run_cli_ok(&alice_cfg, &["dial", "bob", "-m", "API integration"]);
    let ringing: serde_json::Value =
        serde_json::from_str(&run_cli_ok(&bob_cfg, &["ring", "--json"])).unwrap();
    assert_eq!(ringing.as_array().unwrap().len(), 1);
    assert_eq!(ringing[0]["handle"], "alice");

    run_cli_ok(&bob_cfg, &["approve", "alice"]);

    // 7. Turn 1: Alice calls bob/api over real IPC + Iroh QUIC -> resumes existing Codex session.
    let call_out = run_cli_ok(
        &alice_cfg,
        &[
            "call",
            "bob/api",
            "What fields does POST /orders take?",
            "--json",
        ],
    );
    let turn1: serde_json::Value = serde_json::from_str(&call_out).unwrap();
    assert_eq!(turn1["status"], "replied");
    assert!(turn1["body"]
        .as_str()
        .unwrap()
        .contains(EXISTING_SESSION_ID));
    let call_id = turn1["call_id"].as_str().unwrap().to_string();

    // 8. Turn 2: Alice replies on the same call_id with --done -> resumes the same session.
    let reply_out = run_cli_ok(
        &alice_cfg,
        &["reply", &call_id, "And for EUR?", "--done", "--json"],
    );
    let turn2: serde_json::Value = serde_json::from_str(&reply_out).unwrap();
    assert_eq!(turn2["status"], "replied");
    assert!(turn2["body"]
        .as_str()
        .unwrap()
        .contains(EXISTING_SESSION_ID));

    // 9. History shows both turns on Alice's side.
    let hist: serde_json::Value =
        serde_json::from_str(&run_cli_ok(&alice_cfg, &["history", &call_id, "--json"])).unwrap();
    assert_eq!(hist["call"]["status"], "closed");
    assert_eq!(hist["messages"].as_array().unwrap().len(), 2);

    // 10. Bob revokes Alice -> subsequent call from Alice is rejected immediately.
    run_cli_ok(&bob_cfg, &["revoke", "alice"]);
    let after_revoke = run_cli(&alice_cfg, &["call", "bob/api", "after revoke", "--json"]);
    assert!(!after_revoke.status.success());

    // 11. Verify `init --git-remote` config bootstrap works on a fresh config path (isolated HOME).
    let carol_home = tmp.path().join("carol-home");
    std::fs::create_dir_all(&carol_home).unwrap();
    let boot_cfg = carol_home.join("bootstrap.toml");
    let boot_res = Command::new(bin())
        .env("HOME", &carol_home)
        .arg("--config")
        .arg(&boot_cfg)
        .args([
            "init",
            "--name",
            "carol",
            "--git-remote",
            bare_repo.to_str().unwrap(),
        ])
        .output()
        .expect("spawn switchboard init bootstrap");
    assert!(
        boot_res.status.success(),
        "bootstrap init failed: {}",
        String::from_utf8_lossy(&boot_res.stderr)
    );
    let boot_out = String::from_utf8_lossy(&boot_res.stdout);
    assert!(boot_out.contains("Wrote configuration"));
    assert!(PathBuf::from(&boot_cfg).exists());
}
