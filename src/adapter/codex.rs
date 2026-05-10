//! Codex engine adapter.
//!
//! Drives the `codex` CLI in `mcp-server` mode (stdio JSON-RPC). Each
//! user prompt becomes a `tools/call` invocation of the `codex` tool;
//! the tool's final response text becomes a [`CrepEvent::RoleSpoke`].
//!
//! v0.1 scope (intentionally minimal — see `docs/architecture.md`):
//!
//! - Single-turn per user message. No `codex-reply` for follow-ups; each
//!   prompt starts a fresh Codex session. Slower / more expensive than
//!   the CC adapter's long-lived session, but functionally correct.
//! - Codex exec lifecycle notifications are translated into best-effort
//!   CREP tool events when the installed CLI emits them.
//! - Codex runs only with `permission_mode="bypass"` until CodeRoom can
//!   answer Codex approval requests over MCP. `ask` / `auto` fail fast
//!   instead of hanging inside a pending approval request.
//!
//! Multi-turn (resume / `codex-reply`), tool-call event mapping, and
//! wrapper-side approval request responses are scheduled for
//! `feat/adapter-codex-v2`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};

use crate::adapter::{
    AdapterError, AdapterResult, Engine, EngineAdapter, PermissionMode, RoleConfig, RoleHandle,
    UserMessage,
};
use crate::crep::{CrepEvent, StopReason};

/// Channel capacity for both inbound user messages and outbound CREP
/// events. Codex's one-shot turn model means at most one outstanding
/// request per role at any time, so a small buffer is fine.
const CHANNEL_CAPACITY: usize = 32;

/// MCP protocol version we initialize with. Confirmed in the L3 spike.
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

const RPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Adapter that drives Codex's `mcp-server` mode.
#[derive(Debug, Clone)]
pub struct CodexAdapter {
    codex_path: PathBuf,
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexAdapter {
    /// Construct an adapter that resolves `codex` via the user's `PATH`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            codex_path: PathBuf::from("codex"),
        }
    }

    /// Construct an adapter pointing at a specific `codex` binary.
    #[must_use]
    pub fn with_path(codex_path: PathBuf) -> Self {
        Self { codex_path }
    }
}

impl EngineAdapter for CodexAdapter {
    fn engine(&self) -> Engine {
        Engine::Codex
    }

    async fn start(&self, config: RoleConfig) -> AdapterResult<RoleHandle> {
        if config.permission_mode != PermissionMode::Bypass {
            return Err(AdapterError::Engine {
                engine: Engine::Codex.as_str(),
                message: format!(
                    "Codex roles require permission_mode=\"bypass\"; \
                     CodeRoom cannot yet answer Codex approval requests \
                     in permission_mode=\"{}\"",
                    config.permission_mode.as_str()
                ),
            });
        }

        let priors_text = tokio::fs::read_to_string(&config.priors_path)
            .await
            .map_err(|source| AdapterError::PriorsRead {
                path: config.priors_path.clone(),
                source,
            })?;

        let mut cmd = Command::new(&self.codex_path);
        cmd.arg("mcp-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|source| AdapterError::Spawn {
            engine: Engine::Codex.as_str(),
            source,
        })?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (tx_user, rx_user) = mpsc::channel::<UserMessage>(CHANNEL_CAPACITY);
        let (tx_events, rx_events) = mpsc::channel::<CrepEvent>(CHANNEL_CAPACITY);
        let (stop_tx, stop_rx) = oneshot::channel::<StopReason>();

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let writer = Arc::new(Mutex::new(stdin));

        // Spawn the JSON-RPC reader: parses each line, dispatches
        // responses by id to the matching pending request.
        tokio::spawn(read_rpc_loop(
            stdout,
            Arc::clone(&pending),
            config.name.clone(),
            tx_events.clone(),
        ));
        tokio::spawn(drain_stderr(config.name.clone(), stderr));

        // Initialize the MCP connection synchronously before announcing
        // RoleStarted so the role is genuinely ready when CREP says so.
        let client = RpcClient::new(Arc::clone(&writer), Arc::clone(&pending));
        match client.initialize().await {
            Ok(model) => {
                let model = codex_display_model(config.model.as_deref(), model.as_deref());
                let _ = tx_events
                    .send(CrepEvent::RoleStarted {
                        role: config.name.clone(),
                        engine: Engine::Codex.as_str().to_owned(),
                        model,
                        session_id: format!("codex-{}", config.name),
                        priors_hash: crate::adapter::cc::fingerprint(&priors_text),
                    })
                    .await;
            }
            Err(error) => {
                warn!(role = %config.name, %error, "Codex initialize failed");
                return Err(AdapterError::Engine {
                    engine: Engine::Codex.as_str(),
                    message: format!("initialize failed: {error}"),
                });
            }
        }

        tokio::spawn(write_loop(
            config.name.clone(),
            priors_text,
            config.budget_usd,
            rx_user,
            client,
            tx_events.clone(),
        ));

        tokio::spawn(wait_child(config.name.clone(), child, tx_events, stop_rx));

        Ok(RoleHandle::new(
            config.name,
            Engine::Codex,
            tx_user,
            rx_events,
            stop_tx,
        ))
    }
}

#[derive(Debug, thiserror::Error)]
enum RpcError {
    #[error("rpc transport closed")]
    Closed,
    #[error("rpc reported error: {0}")]
    Server(String),
    #[error("rpc request timed out")]
    Timeout,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JsonRpcResponse {
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Clone)]
struct RpcClient {
    writer: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    next_id: Arc<Mutex<u64>>,
}

impl RpcClient {
    fn new(
        writer: Arc<Mutex<ChildStdin>>,
        pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    ) -> Self {
        Self {
            writer,
            pending,
            next_id: Arc::new(Mutex::new(1)),
        }
    }

    async fn alloc_id(&self) -> u64 {
        let mut guard = self.next_id.lock().await;
        let id = *guard;
        *guard += 1;
        id
    }

    /// Send a JSON-RPC request and await the matching response.
    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.alloc_id().await;
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let _guard = PendingRequestGuard {
            id,
            pending: Arc::clone(&self.pending),
        };

        let line = format!("{envelope}\n");
        {
            let mut w = self.writer.lock().await;
            if w.write_all(line.as_bytes()).await.is_err() || w.flush().await.is_err() {
                self.pending.lock().await.remove(&id);
                return Err(RpcError::Closed);
            }
        }

        let response = tokio::time::timeout(RPC_REQUEST_TIMEOUT, rx)
            .await
            .map_err(|_| RpcError::Timeout)?
            .map_err(|_| RpcError::Closed)?;
        if let Some(err) = response.error {
            return Err(RpcError::Server(err.to_string()));
        }
        Ok(response.result.unwrap_or(Value::Null))
    }

    /// Send a JSON-RPC notification (no response expected).
    async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let line = format!("{envelope}\n");
        let mut w = self.writer.lock().await;
        w.write_all(line.as_bytes())
            .await
            .map_err(|_| RpcError::Closed)?;
        w.flush().await.map_err(|_| RpcError::Closed)?;
        Ok(())
    }

    /// Initialize handshake. Returns the engine-reported model name if
    /// it surfaces in the InitializeResult; the wrapper falls back to a
    /// generic label otherwise.
    async fn initialize(&self) -> Result<Option<String>, RpcError> {
        let result = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "coderoom", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await?;
        self.notify("notifications/initialized", Value::Null)
            .await?;
        let model = result
            .get("serverInfo")
            .and_then(|s| s.get("name"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        Ok(model)
    }
}

struct PendingRequestGuard {
    id: u64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        let id = self.id;
        let pending = Arc::clone(&self.pending);
        tokio::spawn(async move {
            pending.lock().await.remove(&id);
        });
    }
}

fn codex_display_model(config_model: Option<&str>, reported_model: Option<&str>) -> String {
    config_model
        .filter(|model| !is_placeholder_model(model))
        .or_else(|| reported_model.filter(|model| !is_placeholder_model(model)))
        .unwrap_or("Codex default")
        .to_owned()
}

fn is_placeholder_model(model: &str) -> bool {
    let normalized = model.trim().to_ascii_lowercase();
    normalized.is_empty() || normalized == "model" || normalized == "codex"
}

async fn read_rpc_loop(
    stdout: ChildStdout,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    role: String,
    events: mpsc::Sender<CrepEvent>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(error) => {
                warn!(role, %error, line = %line, "non-JSON line on codex stdout");
                continue;
            }
        };
        // Responses carry a numeric id matched by request().
        // Notifications have no id; map known exec lifecycle messages
        // into CREP and ignore the rest.
        let Some(id) = value.get("id").and_then(Value::as_u64) else {
            if let Some(event) = codex_notification_to_event(&role, &value) {
                let _ = events.send(event).await;
            } else {
                debug!(role, line = %line, "codex notification ignored");
            }
            continue;
        };
        let response = JsonRpcResponse {
            result: value.get("result").cloned(),
            error: value.get("error").cloned(),
        };
        if let Some(tx) = pending.lock().await.remove(&id) {
            let _ = tx.send(response);
        }
    }
    let mut guard = pending.lock().await;
    for (_, tx) in guard.drain() {
        let _ = tx.send(JsonRpcResponse {
            result: None,
            error: Some(json!({"code": -32000, "message": "codex rpc disconnected"})),
        });
    }
    debug!(role, "codex rpc loop exiting");
}

fn codex_notification_to_event(role: &str, value: &Value) -> Option<CrepEvent> {
    let method = value.get("method").and_then(Value::as_str)?;
    let params = value.get("params").unwrap_or(&Value::Null);
    let tool_use_id = params
        .get("id")
        .or_else(|| params.get("call_id"))
        .or_else(|| params.get("command_id"))
        .and_then(Value::as_str)
        .unwrap_or(method)
        .to_owned();
    match method {
        "notifications/exec_command_started" | "exec_command_started" => {
            let command = params
                .get("command")
                .or_else(|| params.get("cmd"))
                .cloned()
                .unwrap_or(Value::Null);
            Some(CrepEvent::ToolCallProposed {
                role: role.to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: if command.is_null() {
                    params.clone()
                } else {
                    json!({ "command": command })
                },
                tool_use_id,
            })
        }
        "notifications/exec_command_completed" | "exec_command_completed" => {
            let ok = params
                .get("exit_code")
                .and_then(Value::as_i64)
                .is_none_or(|code| code == 0);
            let summary = params
                .get("summary")
                .or_else(|| params.get("stderr"))
                .or_else(|| params.get("stdout"))
                .and_then(Value::as_str)
                .unwrap_or("codex command completed");
            Some(CrepEvent::ToolCallExecuted {
                role: role.to_owned(),
                tool_use_id,
                ok,
                output_summary: summary.chars().take(200).collect(),
            })
        }
        _ => None,
    }
}

async fn drain_stderr(role: String, stderr: ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => {}
            Ok(Some(line)) => warn!(role, line = %line, "codex stderr"),
            Ok(None) => return,
            Err(error) => {
                warn!(role, %error, "error reading codex stderr");
                return;
            }
        }
    }
}

async fn write_loop(
    role: String,
    priors_text: String,
    budget_usd: f64,
    mut rx: mpsc::Receiver<UserMessage>,
    client: RpcClient,
    events: mpsc::Sender<CrepEvent>,
) {
    while let Some(msg) = rx.recv().await {
        let UserMessage::Prompt(prompt) = msg else {
            continue;
        };
        let params = serde_json::json!({
            "name": "codex",
            "arguments": {
                "prompt": prompt,
                "base-instructions": priors_text,
                "approval-policy": "never",
                "sandbox": "workspace-write",
            },
        });
        let _ = budget_usd; // wired in v0.2 with codex's --max-* config

        match client.request("tools/call", params).await {
            Ok(result) => {
                let text = extract_text_from_tool_result(&result);
                let mentions = crate::adapter::cc::parse_mentions(&text);
                let _ = events
                    .send(CrepEvent::RoleSpoke {
                        role: role.clone(),
                        text,
                        mentions,
                        cost_usd: 0.0,
                        cache_read: 0,
                    })
                    .await;
            }
            Err(error) => {
                warn!(role, %error, "codex tools/call failed");
                let _ = events
                    .send(CrepEvent::RoleSpoke {
                        role: role.clone(),
                        text: format!("[codex error: {error}]"),
                        mentions: Vec::new(),
                        cost_usd: 0.0,
                        cache_read: 0,
                    })
                    .await;
            }
        }
    }
    debug!(role, "codex write loop exiting");
}

fn extract_text_from_tool_result(result: &Value) -> String {
    // MCP tool result shape: { content: [{ type: "text", text: "..." }, ...], isError: bool }
    // We concatenate every text block in order.
    let Some(blocks) = result.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n\n")
}

async fn wait_child(
    role: String,
    mut child: Child,
    events: mpsc::Sender<CrepEvent>,
    stop_rx: oneshot::Receiver<StopReason>,
) {
    let reason = tokio::select! {
        status = child.wait() => match status {
            Ok(status) if status.success() => StopReason::Completed,
            Ok(_) => StopReason::Crashed,
            Err(error) => {
                warn!(role, %error, "error waiting on codex subprocess");
                StopReason::Crashed
            }
        },
        requested = stop_rx => {
            let reason = requested.unwrap_or(StopReason::Crashed);
            terminate_child(&role, &mut child).await;
            reason
        }
    };
    let _ = events.send(CrepEvent::RoleStopped { role, reason }).await;
}

async fn terminate_child(role: &str, child: &mut Child) {
    if let Err(error) = child.start_kill() {
        warn!(role, %error, "failed to start codex subprocess kill");
        return;
    }
    match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => warn!(role, %error, "error waiting after codex subprocess kill"),
        Err(_) => {
            warn!(
                role,
                "codex subprocess did not exit promptly after kill signal"
            );
            let _ = child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn extract_text_collects_all_text_blocks() {
        let v = serde_json::json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "data": "..."},
                {"type": "text", "text": "world"},
            ],
        });
        assert_eq!(extract_text_from_tool_result(&v), "hello\n\nworld");
    }

    #[test]
    fn extract_text_handles_missing_content() {
        let v = serde_json::json!({});
        assert_eq!(extract_text_from_tool_result(&v), "");
    }

    #[test]
    fn extract_text_skips_non_string_text() {
        let v = serde_json::json!({
            "content": [
                {"type": "text", "text": 123},
                {"type": "text", "text": "ok"},
            ],
        });
        assert_eq!(extract_text_from_tool_result(&v), "ok");
    }

    #[test]
    fn display_model_uses_configured_model_before_server_name() {
        assert_eq!(
            codex_display_model(Some("gpt-5.1-codex"), Some("model")),
            "gpt-5.1-codex"
        );
    }

    #[test]
    fn display_model_does_not_show_placeholder_model() {
        assert_eq!(codex_display_model(None, Some("model")), "Codex default");
        assert_eq!(codex_display_model(None, Some("codex")), "Codex default");
    }

    #[tokio::test]
    async fn start_rejects_non_bypass_permission_modes_before_spawn() {
        let adapter = CodexAdapter::with_path(PathBuf::from("/definitely/not/codex"));
        for permission_mode in [PermissionMode::Ask, PermissionMode::Auto] {
            let err = adapter
                .start(RoleConfig {
                    name: "qa".into(),
                    engine: Engine::Codex,
                    model: None,
                    priors_path: PathBuf::from("/missing/priors.md"),
                    budget_usd: 0.50,
                    permission_mode,
                    permission_policy_path: None,
                    permission_socket_path: None,
                })
                .await
                .expect_err("codex non-bypass should fail before spawn");
            let text = err.to_string();
            assert!(text.contains("permission_mode=\"bypass\""), "{text}");
            assert!(text.contains(permission_mode.as_str()), "{text}");
        }
    }

    #[test]
    fn exec_notifications_translate_to_tool_events() {
        let started = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/exec_command_started",
            "params": {"id": "cmd-1", "command": "cargo test"}
        });
        let event = codex_notification_to_event("qa", &started).expect("started event");
        match event {
            CrepEvent::ToolCallProposed {
                role,
                tool_name,
                tool_use_id,
                ..
            } => {
                assert_eq!(role, "qa");
                assert_eq!(tool_name, "Bash");
                assert_eq!(tool_use_id, "cmd-1");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let completed = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/exec_command_completed",
            "params": {"id": "cmd-1", "exit_code": 0, "summary": "ok"}
        });
        let event = codex_notification_to_event("qa", &completed).expect("completed event");
        match event {
            CrepEvent::ToolCallExecuted {
                role,
                tool_use_id,
                ok,
                output_summary,
            } => {
                assert_eq!(role, "qa");
                assert_eq!(tool_use_id, "cmd-1");
                assert!(ok);
                assert_eq!(output_summary, "ok");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
