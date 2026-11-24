//! Official OpenAI `codex app-server` JSON-RPC 2.0 stdio harness adapter.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;

use super::model::{DeliveryOutcome, DeliveryRequest};
use super::{AgentHarness, ExtensionError};

const CLIENT_NAME: &str = "switchboard";
const CLIENT_TITLE: &str = "Switchboard";
const CLIENT_VERSION: &str = "0.1.0";
const MAX_STDERR_BYTES: usize = 4096;
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_millis(150);

/// Process and timeout configuration for [`CodexHarness`].
#[derive(Debug, Clone)]
pub struct CodexConfig {
    pub binary: PathBuf,
    pub args: Vec<String>,
    pub turn_timeout: Duration,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("codex"),
            args: vec!["app-server".to_string()],
            turn_timeout: Duration::from_secs(180),
        }
    }
}

/// [`AgentHarness`] speaking the newline-delimited JSON-RPC 2.0 `codex app-server` protocol.
#[derive(Debug, Clone, Default)]
pub struct CodexHarness {
    config: CodexConfig,
}

impl CodexHarness {
    pub fn new(config: CodexConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &CodexConfig {
        &self.config
    }
}

#[async_trait::async_trait]
impl AgentHarness for CodexHarness {
    async fn deliver(
        &self,
        req: DeliveryRequest,
        cancel: CancellationToken,
    ) -> Result<DeliveryOutcome, ExtensionError> {
        if cancel.is_cancelled() {
            return Ok(DeliveryOutcome::Cancelled);
        }

        tracing::debug!(
            call_id = %req.call_id,
            extension = %req.extension,
            from_handle = %req.from_handle,
            has_session = req.existing_session_id.is_some(),
            "spawning codex app-server for delivery"
        );

        let mut session = CodexSession::spawn(&self.config, &req.workspace_dir)?;
        let deadline = tokio::time::sleep(self.config.turn_timeout);
        tokio::pin!(deadline);

        // 1. JSON-RPC initialize (id: 1)
        let init_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": CLIENT_NAME,
                    "title": CLIENT_TITLE,
                    "version": CLIENT_VERSION
                }
            }
        });
        if let RpcStep::Outcome(outcome) = session
            .rpc_call(1, &init_req, deadline.as_mut(), &cancel, "initialize")
            .await?
        {
            return Ok(outcome);
        }

        // 2. JSON-RPC initialized notification
        let initialized_note = json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        });
        if session.send_json(&initialized_note).await.is_err() {
            return Ok(session.collect_process_failure().await);
        }

        // 3. Thread setup (id: 2)
        let thread_req = match req.existing_session_id.as_deref() {
            Some(tid) => json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "thread/resume",
                "params": { "threadId": tid }
            }),
            None => json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "thread/start",
                "params": { "cwd": req.workspace_dir.to_string_lossy() }
            }),
        };
        let thread_resp = match session
            .rpc_call(2, &thread_req, deadline.as_mut(), &cancel, "thread setup")
            .await?
        {
            RpcStep::Response(msg) => msg,
            RpcStep::Outcome(outcome) => return Ok(outcome),
        };

        let Some(thread_id) = extract_thread_id(&thread_resp) else {
            session.shutdown().await;
            return Err(ExtensionError::Protocol(
                "missing thread id in codex thread setup response".into(),
            ));
        };

        // 4. Turn execution (id: 3)
        let formatted_prompt = format_prompt(&req);
        let turn_req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "turn/start",
            "params": {
                "threadId": thread_id,
                "input": [{ "type": "text", "text": formatted_prompt }]
            }
        });
        if session.send_json(&turn_req).await.is_err() {
            return Ok(session.collect_process_failure().await);
        }

        // 5. Stream turn events until turn/completed, cancel, timeout, or EOF
        session.stream_turn(thread_id, deadline, &cancel).await
    }
}

enum RpcStep {
    Response(Value),
    Outcome(DeliveryOutcome),
}

struct CodexSession {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
    stderr_task: Option<JoinHandle<String>>,
}

impl CodexSession {
    fn spawn(config: &CodexConfig, workspace_dir: &Path) -> Result<Self, ExtensionError> {
        let mut child = Command::new(&config.binary)
            .args(&config.args)
            .current_dir(workspace_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| {
                ExtensionError::Spawn(format!("binary '{}': {err}", config.binary.display()))
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ExtensionError::Protocol("missing codex stdin pipe".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ExtensionError::Protocol("missing codex stdout pipe".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ExtensionError::Protocol("missing codex stderr pipe".into()))?;

        Ok(Self {
            child,
            stdin: Some(stdin),
            lines: BufReader::new(stdout).lines(),
            stderr_task: Some(spawn_stderr_drain(stderr)),
        })
    }

    async fn send_json(&mut self, payload: &Value) -> Result<(), ()> {
        let Some(writer) = self.stdin.as_mut() else {
            return Err(());
        };
        write_json_line(writer, payload).await.map_err(|_| ())
    }

    async fn next_json_message(
        &mut self,
        mut deadline: Pin<&mut Sleep>,
        cancel: &CancellationToken,
        thread_id: Option<&str>,
    ) -> Result<RpcStep, ExtensionError> {
        loop {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    self.interrupt_and_terminate(thread_id).await;
                    return Ok(RpcStep::Outcome(DeliveryOutcome::Cancelled));
                }
                () = &mut deadline => {
                    self.interrupt_and_terminate(thread_id).await;
                    return Ok(RpcStep::Outcome(DeliveryOutcome::TimedOut));
                }
                line = self.lines.next_line() => line?,
            };

            let Some(raw) = next else {
                return Ok(RpcStep::Outcome(self.collect_process_failure().await));
            };

            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }

            match serde_json::from_str::<Value>(trimmed) {
                Ok(msg) => return Ok(RpcStep::Response(msg)),
                Err(_) => tracing::debug!("ignoring non-json stdout line from codex app-server"),
            }
        }
    }

    async fn rpc_call(
        &mut self,
        expected_id: u64,
        payload: &Value,
        mut deadline: Pin<&mut Sleep>,
        cancel: &CancellationToken,
        context: &str,
    ) -> Result<RpcStep, ExtensionError> {
        if self.send_json(payload).await.is_err() {
            return Ok(RpcStep::Outcome(self.collect_process_failure().await));
        }

        loop {
            let msg = match self
                .next_json_message(deadline.as_mut(), cancel, None)
                .await?
            {
                RpcStep::Response(msg) => msg,
                outcome @ RpcStep::Outcome(_) => return Ok(outcome),
            };

            if msg.get("id").and_then(Value::as_u64) == Some(expected_id) {
                if let Some(err_msg) = rpc_error_message(&msg) {
                    self.shutdown().await;
                    return Err(ExtensionError::Protocol(format!(
                        "{context} failed: {err_msg}"
                    )));
                }
                return Ok(RpcStep::Response(msg));
            }
        }
    }

    async fn stream_turn(
        &mut self,
        thread_id: String,
        mut deadline: Pin<&mut Sleep>,
        cancel: &CancellationToken,
    ) -> Result<DeliveryOutcome, ExtensionError> {
        let mut accumulated = String::new();
        let mut authoritative: Option<String> = None;

        loop {
            let msg = match self
                .next_json_message(deadline.as_mut(), cancel, Some(&thread_id))
                .await?
            {
                RpcStep::Response(msg) => msg,
                RpcStep::Outcome(outcome) => return Ok(outcome),
            };

            if msg.get("id").and_then(Value::as_u64) == Some(3) {
                if let Some(err_msg) = rpc_error_message(&msg) {
                    self.shutdown().await;
                    return Err(ExtensionError::Protocol(format!(
                        "turn/start failed: {err_msg}"
                    )));
                }
                continue;
            }

            let Some(method) = msg.get("method").and_then(Value::as_str) else {
                continue;
            };
            let params = msg.get("params").unwrap_or(&Value::Null);

            match method {
                "item/agentMessage/delta" => {
                    if let Some(delta) = extract_delta_text(params) {
                        accumulated.push_str(delta);
                    }
                }
                "item/completed" => {
                    if let Some(text) = extract_completed_agent_message(params) {
                        authoritative = Some(text);
                    }
                }
                "turn/completed" => {
                    self.shutdown().await;
                    let body = authoritative.unwrap_or(accumulated);
                    return Ok(DeliveryOutcome::Replied {
                        body,
                        session_id: Some(thread_id),
                    });
                }
                other => {
                    tracing::debug!(method = other, "ignoring unhandled codex notification");
                }
            }
        }
    }

    async fn interrupt_and_terminate(&mut self, thread_id: Option<&str>) {
        if let (Some(tid), Some(writer)) = (thread_id, self.stdin.as_mut()) {
            let interrupt = json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "turn/interrupt",
                "params": { "threadId": tid }
            });
            let _ = write_json_line(writer, &interrupt).await;
        }
        self.shutdown().await;
    }

    async fn shutdown(&mut self) {
        let _ = self.stdin.take();
        if tokio::time::timeout(SHUTDOWN_GRACE_PERIOD, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }

    async fn collect_process_failure(&mut self) -> DeliveryOutcome {
        let _ = self.stdin.take();
        let exit_code =
            match tokio::time::timeout(Duration::from_millis(500), self.child.wait()).await {
                Ok(Ok(status)) => status.code(),
                Ok(Err(_)) => None,
                Err(_) => {
                    let _ = self.child.kill().await;
                    None
                }
            };

        let stderr_tail = match self.stderr_task.take() {
            Some(task) => match tokio::time::timeout(Duration::from_millis(250), task).await {
                Ok(Ok(text)) => text,
                _ => String::new(),
            },
            None => String::new(),
        };

        let detail = if stderr_tail.is_empty() {
            "codex app-server exited before turn/completed".to_string()
        } else {
            stderr_tail
        };

        DeliveryOutcome::ProcessFailed { exit_code, detail }
    }
}

fn format_prompt(req: &DeliveryRequest) -> String {
    format!(
        "Incoming switchboard message from peer @{from} (extension: {ext}, call: {call_id}):\n\n{body}",
        from = req.from_handle,
        ext = req.extension,
        call_id = req.call_id,
        body = req.body,
    )
}

async fn write_json_line(stdin: &mut ChildStdin, payload: &Value) -> Result<(), ExtensionError> {
    let mut line = serde_json::to_string(payload).map_err(|err| {
        ExtensionError::Protocol(format!("failed to serialize rpc message: {err}"))
    })?;
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

fn rpc_error_message(msg: &Value) -> Option<String> {
    let err = msg.get("error").filter(|v| !v.is_null())?;
    let text = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown json-rpc error");
    Some(text.to_string())
}

fn extract_thread_id(msg: &Value) -> Option<String> {
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

fn extract_delta_text(params: &Value) -> Option<&str> {
    params
        .get("delta")
        .and_then(Value::as_str)
        .or_else(|| params.get("text").and_then(Value::as_str))
}

fn extract_completed_agent_message(params: &Value) -> Option<String> {
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

fn spawn_stderr_drain(mut stderr: ChildStderr) -> JoinHandle<String> {
    tokio::spawn(async move {
        let mut tail: Vec<u8> = Vec::new();
        let mut buf = [0u8; 1024];
        while let Ok(n) = stderr.read(&mut buf).await {
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > MAX_STDERR_BYTES {
                let excess = tail.len() - MAX_STDERR_BYTES;
                tail.drain(..excess);
            }
        }
        String::from_utf8_lossy(&tail).trim().to_string()
    })
}
