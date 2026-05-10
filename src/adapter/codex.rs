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
//! - Codex `permission_mode="ask"` / `"auto"` is supported only when the
//!   live REPL provides a permission bridge. Headless callers still need
//!   `permission_mode="bypass"` so approval requests cannot hang.
//!
//! Multi-turn (resume / `codex-reply`) is scheduled for
//! `feat/adapter-codex-v2`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

#[cfg(unix)]
use nix::sys::signal::{kill, Signal};
#[cfg(unix)]
use nix::unistd::Pid;
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

// Keep this above the REPL's per-turn timeout so interactive sessions get one
// user-visible timeout surface: the REPL interrupted WorkCard. The adapter
// timeout is still useful for headless tests or future non-REPL consumers.
const RPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(6 * 60);

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
        // Non-bypass modes need a live permission bridge so codex's
        // server-initiated approval requests can reach the user. With
        // no bridge available we fail fast rather than hang the MCP
        // call, mirroring the gemini contract.
        if config.permission_mode != PermissionMode::Bypass
            && config.permission_socket_path.is_none()
        {
            return Err(AdapterError::Engine {
                engine: Engine::Codex.as_str(),
                message: format!(
                    "Codex permission_mode=\"{}\" needs a live CodeRoom REPL with \
                     a permission bridge; none was supplied. Use permission_mode=\"bypass\" \
                     for headless contexts (smoke tests, `cr show`).",
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
        isolate_process_group(&mut cmd);
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
        // responses by id to the matching pending request, and routes
        // server-initiated approval requests through the permission
        // bridge so the user actually gets prompted.
        tokio::spawn(read_rpc_loop(
            stdout,
            Arc::clone(&pending),
            Arc::clone(&writer),
            config.name.clone(),
            tx_events.clone(),
            config.permission_socket_path.clone(),
            config.permission_policy_path.clone(),
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
            config.permission_mode,
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
    writer: Arc<Mutex<tokio::process::ChildStdin>>,
    role: String,
    events: mpsc::Sender<CrepEvent>,
    permission_socket: Option<PathBuf>,
    permission_policy_path: Option<PathBuf>,
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
        // JSON-RPC dispatch: three message shapes.
        //   id +  method  → server-initiated request (e.g. approval)
        //   id  – method  → response to one of our pending requests
        //  –id  + method  → notification (lifecycle, exec_command_*, …)
        //
        // Per JSON-RPC 2.0 §4.2 ids may be strings OR numbers; codex
        // happens to use numbers today but we keep the original Value
        // so a future string-id rev can't deadlock us.
        let id_value = value.get("id").cloned();
        let has_method = value.get("method").is_some();
        let id_present = id_value.as_ref().is_some_and(|v| !v.is_null());

        match (id_present, has_method) {
            (true, true) => {
                let writer = Arc::clone(&writer);
                let role_name = role.clone();
                let socket = permission_socket.clone();
                let policy_path = permission_policy_path.clone();
                let events = events.clone();
                let request = value.clone();
                let id = RpcId(id_value.unwrap_or(Value::Null));
                tokio::spawn(async move {
                    handle_server_request(
                        role_name,
                        id,
                        request,
                        socket,
                        policy_path,
                        events,
                        writer,
                    )
                    .await;
                });
            }
            (true, false) => {
                // Only numeric ids are used by our outbound request()
                // path; if codex echoes back a non-numeric id here it
                // doesn't match any of our pending senders so drop it.
                let Some(id) = id_value.as_ref().and_then(Value::as_u64) else {
                    debug!(role, line = %line, "non-numeric response id, dropping");
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
            (false, true) => {
                if let Some(event) = codex_notification_to_event(&role, &value) {
                    let _ = events.send(event).await;
                } else {
                    debug!(role, line = %line, "codex notification ignored");
                }
            }
            (false, false) => {
                debug!(role, line = %line, "malformed codex stdout line");
            }
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

/// Map CodeRoom's permission_mode to codex's `approval-policy`.
/// `ask` becomes `untrusted` (codex asks for ~everything that isn't
/// trivially safe), `auto` becomes `on-request` (codex asks only for
/// genuinely risky calls), `bypass` becomes `never` (codex never asks).
fn codex_approval_policy(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Ask => "untrusted",
        PermissionMode::Auto => "on-request",
        PermissionMode::Bypass => "never",
    }
}

/// Map CodeRoom's permission_mode to codex's command sandbox.
/// `bypass` is CodeRoom's yolo mode, so it must disable both approvals
/// and Codex's own sandbox. Keeping `workspace-write` here still invokes
/// Codex's Linux sandbox and can fail in hosts where bubblewrap networking
/// setup is unavailable.
fn codex_sandbox(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Ask | PermissionMode::Auto => "workspace-write",
        PermissionMode::Bypass => "danger-full-access",
    }
}

/// Codex approval method names per
/// <https://github.com/openai/codex/blob/main/codex-rs/docs/codex_mcp_interface.md#approvals-server---client>
/// (verified 2026-05-10). Anything else codex sends as a server-initiated
/// request gets a `-32601` reply so codex doesn't hang on an unanswered id.
const EXEC_APPROVAL_METHOD: &str = "execCommandApproval";
const PATCH_APPROVAL_METHOD: &str = "applyPatchApproval";

/// One codex JSON-RPC id. Codex echoes ids verbatim in responses, and
/// per the JSON-RPC 2.0 spec ids may be numbers OR strings, so we keep
/// the original `Value` rather than coercing to `u64`.
#[derive(Debug, Clone)]
struct RpcId(Value);

/// Handle one server-initiated JSON-RPC request. Approval methods route
/// through the permission bridge; everything else gets a Method-Not-Found
/// reply so codex doesn't deadlock on an outstanding id.
async fn handle_server_request(
    role: String,
    id: RpcId,
    request: Value,
    permission_socket: Option<PathBuf>,
    permission_policy_path: Option<PathBuf>,
    events: mpsc::Sender<CrepEvent>,
    writer: Arc<Mutex<tokio::process::ChildStdin>>,
) {
    use tokio::io::AsyncWriteExt;

    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    let response_value = match method.as_str() {
        EXEC_APPROVAL_METHOD | PATCH_APPROVAL_METHOD => {
            handle_approval_request(
                &role,
                &id,
                &method,
                &params,
                permission_socket,
                permission_policy_path,
                &events,
            )
            .await
        }
        other => json!({
            "jsonrpc": "2.0",
            "id": id.0,
            "error": {
                "code": -32601,
                "message": format!("method not supported by CodeRoom wrapper: {other}"),
            },
        }),
    };

    let mut bytes = match serde_json::to_vec(&response_value) {
        Ok(b) => b,
        Err(error) => {
            warn!(role, %error, "serializing codex response");
            return;
        }
    };
    bytes.push(b'\n');
    let mut w = writer.lock().await;
    if let Err(error) = w.write_all(&bytes).await {
        warn!(role, %error, "writing codex approval response");
        return;
    }
    if let Err(error) = w.flush().await {
        warn!(role, %error, "flushing codex approval response");
    }
}

async fn handle_approval_request(
    role: &str,
    id: &RpcId,
    method: &str,
    params: &Value,
    permission_socket: Option<PathBuf>,
    permission_policy_path: Option<PathBuf>,
    events: &mpsc::Sender<CrepEvent>,
) -> Value {
    let (tool, summary) = approval_request_preview(method, params);

    if let Some(decision) =
        codex_session_policy_decision(role, permission_policy_path.as_deref(), &tool)
    {
        return approval_response_with_event(
            role,
            id,
            matches!(decision, crate::permissions::PermissionDecision::Allow),
            "CodeRoom session policy",
            &tool,
            &summary,
            events,
        )
        .await;
    }

    let deny_without_socket = "no permission bridge available";
    let Some(socket) = permission_socket else {
        // Defensive: should be unreachable thanks to start()'s
        // socket-required check, but a caller wiring us without one
        // must still get a deny instead of a hang.
        return approval_response_with_event(
            role,
            id,
            false,
            deny_without_socket,
            &tool,
            &summary,
            events,
        )
        .await;
    };

    let bridge_reason = match method {
        PATCH_APPROVAL_METHOD => format!("{tool} requires approval (codex applyPatchApproval)"),
        _ => format!("{tool} requires approval (codex execCommandApproval)"),
    };

    match crate::permissions::bridge::request_decision_async(
        &socket,
        role,
        &tool,
        &summary,
        &bridge_reason,
    )
    .await
    {
        Ok(verdict) => {
            if let Some(path) = permission_policy_path.as_deref() {
                if let Err(error) =
                    crate::permissions::update_policy_from_bridge_response(path, &tool, &verdict)
                {
                    warn!(
                        role,
                        path = %path.display(),
                        %error,
                        "persisting codex permission decision"
                    );
                }
            }
            approval_response_with_event(
                role,
                id,
                matches!(
                    verdict.decision,
                    crate::permissions::PermissionDecision::Allow
                ),
                &verdict.reason,
                &tool,
                &summary,
                events,
            )
            .await
        }
        Err(error) => {
            warn!(role, %error, "permission bridge failed for codex approval");
            approval_response_with_event(
                role,
                id,
                false,
                &format!("CodeRoom permission bridge failed: {error}"),
                &tool,
                &summary,
                events,
            )
            .await
        }
    }
}

async fn approval_response_with_event(
    role: &str,
    id: &RpcId,
    allow: bool,
    reason: &str,
    tool: &str,
    input: &Value,
    events: &mpsc::Sender<CrepEvent>,
) -> Value {
    if !allow {
        let _ = events
            .send(CrepEvent::PermissionDenied {
                role: role.to_owned(),
                tool_name: tool.to_owned(),
                tool_input: input.clone(),
                reason: reason.to_owned(),
            })
            .await;
    }
    approval_response(id, allow, reason)
}

fn codex_session_policy_decision(
    role: &str,
    policy_path: Option<&Path>,
    tool: &str,
) -> Option<crate::permissions::PermissionDecision> {
    let path = policy_path?;
    match crate::permissions::PermissionPolicy::load(path) {
        Ok(policy) => policy.decision_for_tool(tool),
        Err(error) => {
            warn!(
                role,
                path = %path.display(),
                %error,
                "loading codex permission policy"
            );
            None
        }
    }
}

/// Build the JSON-RPC response in codex's documented shape:
///
/// ```text
/// {"jsonrpc":"2.0","id":<echo>,"result":{"decision":"allow"|"deny"}}
/// ```
///
/// `reason` is logged on our side but not part of the on-the-wire
/// response — codex doesn't read it back.
fn approval_response(id: &RpcId, allow: bool, _reason: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.0,
        "result": {
            "decision": if allow { "allow" } else { "deny" },
        },
    })
}

/// Pull the user-facing tool name and a JSON summary out of a codex
/// approval request, per the documented shapes:
///
/// - `execCommandApproval { conversationId, callId, approvalId?, command, cwd, reason? }`
/// - `applyPatchApproval { conversationId, callId, fileChanges, reason?, grantRoot? }`
fn approval_request_preview(method: &str, params: &Value) -> (String, Value) {
    if method == PATCH_APPROVAL_METHOD {
        let summary = params
            .get("fileChanges")
            .cloned()
            .unwrap_or_else(|| params.clone());
        return ("Apply patch".to_owned(), summary);
    }
    // execCommandApproval
    let command_value = params.get("command").cloned();
    let working_dir = params.get("cwd").cloned();
    let summary = match (command_value, working_dir) {
        (Some(command), Some(directory)) => json!({"command": command, "cwd": directory}),
        (Some(command), None) => json!({"command": command}),
        _ => params.clone(),
    };
    ("Bash".to_owned(), summary)
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
    permission_mode: PermissionMode,
    mut rx: mpsc::Receiver<UserMessage>,
    client: RpcClient,
    events: mpsc::Sender<CrepEvent>,
) {
    let approval_policy = codex_approval_policy(permission_mode);
    let sandbox = codex_sandbox(permission_mode);
    while let Some(msg) = rx.recv().await {
        let UserMessage::Prompt(prompt) = msg else {
            continue;
        };
        let params = serde_json::json!({
            "name": "codex",
            "arguments": {
                "prompt": prompt,
                "base-instructions": priors_text,
                // Wire the requested mode through to codex so its server
                // actually emits applyPatchApproval / execCommandApproval
                // requests our bridge can answer. Earlier code hardcoded
                // "never" here, which made the entire approval pipe
                // unreachable regardless of permission_mode.
                "approval-policy": approval_policy,
                "sandbox": sandbox,
            },
        });
        let _ = budget_usd; // wired in v0.2 with codex's --max-* config

        match client.request("tools/call", params).await {
            Ok(result) => {
                let text = extract_text_from_tool_result(&result);
                for event in crate::adapter::role_spoke_events_from_text(&role, &text, 0.0, 0) {
                    let _ = events.send(event).await;
                }
            }
            Err(error) => {
                warn!(role, %error, "codex tools/call failed");
                let text = match error {
                    RpcError::Timeout => format!(
                        "[codex timeout: tools/call did not respond after {}s]",
                        RPC_REQUEST_TIMEOUT.as_secs()
                    ),
                    other => format!("[codex error: {other}]"),
                };
                let _ = events
                    .send(CrepEvent::RoleSpoke {
                        role: role.clone(),
                        text,
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
    if !signal_process_group(role, child, SignalKind::Terminate) {
        if let Err(error) = child.start_kill() {
            warn!(role, %error, "failed to start codex subprocess kill");
            return;
        }
    }
    match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => warn!(role, %error, "error waiting after codex subprocess kill"),
        Err(_) => {
            warn!(
                role,
                "codex subprocess did not exit promptly after kill signal"
            );
            signal_process_group(role, child, SignalKind::Kill);
            let _ = child.kill().await;
        }
    }
}

#[cfg(unix)]
fn isolate_process_group(cmd: &mut Command) {
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn isolate_process_group(_cmd: &mut Command) {}

#[derive(Debug, Clone, Copy)]
enum SignalKind {
    Terminate,
    Kill,
}

#[cfg(unix)]
fn signal_process_group(role: &str, child: &Child, signal: SignalKind) -> bool {
    let Some(pid) = child.id() else {
        return false;
    };
    let Ok(pid) = i32::try_from(pid) else {
        warn!(role, pid, "codex subprocess pid does not fit i32");
        return false;
    };
    let signal = match signal {
        SignalKind::Terminate => Signal::SIGTERM,
        SignalKind::Kill => Signal::SIGKILL,
    };
    match kill(Pid::from_raw(-pid), signal) {
        Ok(()) => true,
        Err(error) => {
            warn!(
                role,
                %error,
                ?signal,
                "failed to signal codex subprocess group"
            );
            false
        }
    }
}

#[cfg(not(unix))]
fn signal_process_group(_role: &str, _child: &Child, _signal: SignalKind) -> bool {
    false
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
    fn permission_mode_maps_to_codex_approval_policy() {
        assert_eq!(codex_approval_policy(PermissionMode::Bypass), "never");
        assert_eq!(codex_approval_policy(PermissionMode::Ask), "untrusted");
        assert_eq!(codex_approval_policy(PermissionMode::Auto), "on-request");
    }

    #[test]
    fn permission_mode_maps_to_codex_sandbox() {
        assert_eq!(codex_sandbox(PermissionMode::Bypass), "danger-full-access");
        assert_eq!(codex_sandbox(PermissionMode::Ask), "workspace-write");
        assert_eq!(codex_sandbox(PermissionMode::Auto), "workspace-write");
    }

    #[test]
    fn approval_response_shape_matches_codex_docs() {
        // Per codex_mcp_interface.md: result must be {"decision":"allow"|"deny"}.
        let id = RpcId(json!(42));
        let allow = approval_response(&id, true, "user said yes");
        assert_eq!(allow["jsonrpc"], "2.0");
        assert_eq!(allow["id"], 42);
        assert_eq!(allow["result"]["decision"], "allow");

        let deny = approval_response(&id, false, "user said no");
        assert_eq!(deny["result"]["decision"], "deny");
    }

    #[test]
    fn approval_response_echoes_string_id() {
        // JSON-RPC ids may be strings; codex doesn't currently use this
        // but the wrapper must echo whatever it received.
        let id = RpcId(json!("call-7"));
        let response = approval_response(&id, true, "");
        assert_eq!(response["id"], "call-7");
    }

    #[test]
    fn approval_request_preview_extracts_exec_command() {
        let params = json!({
            "conversationId": "conv-1",
            "callId": "call-1",
            "command": ["bash", "-c", "ls"],
            "cwd": "/tmp",
        });
        let (tool, summary) = approval_request_preview("execCommandApproval", &params);
        assert_eq!(tool, "Bash");
        assert_eq!(summary["command"], json!(["bash", "-c", "ls"]));
        assert_eq!(summary["cwd"], "/tmp");
    }

    #[test]
    fn approval_request_preview_extracts_patch_changes() {
        let params = json!({
            "conversationId": "conv-1",
            "callId": "call-1",
            "fileChanges": [{"path": "src/foo.rs", "kind": "modify"}],
        });
        let (tool, summary) = approval_request_preview("applyPatchApproval", &params);
        assert_eq!(tool, "Apply patch");
        assert_eq!(summary, json!([{"path": "src/foo.rs", "kind": "modify"}]));
    }

    #[tokio::test]
    async fn approval_request_uses_session_policy_before_bridge() {
        let tmp = tempfile::tempdir().unwrap();
        let policy_path = tmp.path().join("policy.json");
        crate::permissions::update_policy(&policy_path, |policy| {
            policy.allow_tool("Bash");
        })
        .unwrap();

        let id = RpcId(json!("call-1"));
        let (events, _rx_events) = tokio::sync::mpsc::channel(1);
        let params = json!({
            "conversationId": "conv-1",
            "callId": "call-1",
            "command": ["bash", "-c", "ls"],
            "cwd": "/tmp",
        });
        let response = handle_approval_request(
            "qa",
            &id,
            EXEC_APPROVAL_METHOD,
            &params,
            None,
            Some(policy_path),
            &events,
        )
        .await;

        assert_eq!(response["id"], "call-1");
        assert_eq!(response["result"]["decision"], "allow");
    }

    #[tokio::test]
    async fn denied_approval_emits_permission_denied_event() {
        let tmp = tempfile::tempdir().unwrap();
        let policy_path = tmp.path().join("policy.json");
        crate::permissions::update_policy(&policy_path, |policy| {
            policy.deny_tool("Bash");
        })
        .unwrap();
        let (events, mut rx_events) = tokio::sync::mpsc::channel(1);

        let id = RpcId(json!("call-1"));
        let params = json!({
            "conversationId": "conv-1",
            "callId": "call-1",
            "command": ["bash", "-c", "ls"],
            "cwd": "/tmp",
        });
        let response = handle_approval_request(
            "qa",
            &id,
            EXEC_APPROVAL_METHOD,
            &params,
            None,
            Some(policy_path),
            &events,
        )
        .await;

        assert_eq!(response["result"]["decision"], "deny");
        match rx_events.recv().await.expect("permission denial event") {
            CrepEvent::PermissionDenied {
                role,
                tool_name,
                tool_input,
                reason,
            } => {
                assert_eq!(role, "qa");
                assert_eq!(tool_name, "Bash");
                assert_eq!(tool_input["command"], json!(["bash", "-c", "ls"]));
                assert_eq!(reason, "CodeRoom session policy");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn approval_request_persists_session_bridge_decision() {
        let tmp = tempfile::tempdir().unwrap();
        let policy_path = tmp.path().join("policy.json");
        let socket_path = tmp.path().join("permission.sock");
        let (_handle, mut rx) = crate::permissions::bridge::start(socket_path.clone()).unwrap();

        let listener = tokio::spawn(async move {
            let sink = rx.recv().await.expect("codex approval request");
            assert_eq!(sink.request.role, "qa");
            assert_eq!(sink.request.tool, "Bash");
            sink.responder.respond(crate::permissions::BridgeResponse {
                v: 1,
                decision: crate::permissions::PermissionDecision::Allow,
                scope: crate::permissions::DecisionScope::Session,
                reason: "approved".into(),
            });
        });

        let id = RpcId(json!(7));
        let (events, _rx_events) = tokio::sync::mpsc::channel(1);
        let params = json!({
            "conversationId": "conv-1",
            "callId": "call-1",
            "command": ["bash", "-c", "pwd"],
            "cwd": "/tmp",
        });
        let response = handle_approval_request(
            "qa",
            &id,
            EXEC_APPROVAL_METHOD,
            &params,
            Some(socket_path),
            Some(policy_path.clone()),
            &events,
        )
        .await;

        listener.await.unwrap();
        assert_eq!(response["result"]["decision"], "allow");

        let policy = crate::permissions::PermissionPolicy::load(&policy_path).unwrap();
        assert_eq!(
            policy.decision_for_tool("Bash"),
            Some(crate::permissions::PermissionDecision::Allow)
        );
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
