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
use std::sync::atomic::{AtomicBool, Ordering};
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
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::adapter::{
    AdapterError, AdapterResult, Engine, EngineAdapter, PermissionMode, RoleConfig, RoleHandle,
    UserMessage,
};
use crate::crep::{CrepEvent, StopReason};
use crate::turn::TurnId;

/// Channel capacity for both inbound user messages and outbound CREP
/// events. Codex's one-shot turn model means at most one outstanding
/// request per role at any time, so a small buffer is fine.
const CHANNEL_CAPACITY: usize = 32;

/// MCP protocol version we initialize with. Confirmed in the L3 spike.
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

// **Stdio protocol watchdog** — how long we wait without seeing ANY
// `codex/event` for our in-flight `tools/call` before declaring the
// MCP server wedged.
//
// Per `docs/v0.2-trust-and-interrupt.md` § B this is **not** a model
// timing decision: codex emits a steady stream of notifications
// during a real turn (`exec_command_begin/end`,
// `agent_message_content_delta`, `token_count`, …), so silence this
// long means the stdio bridge or the codex process itself is stuck —
// not "the model is taking too long to think." A real model
// taking 25 minutes to plan a refactor is fine; we never cap that.
//
// Bumped from 6 to 10 minutes after PR a feedback: real codex turns
// can be quiet for several minutes mid-reasoning before producing
// the next visible event, and 6 minutes was clipping legitimate runs.
const RPC_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

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

        // `codex mcp-server` (the stdio mode codeRoom drives) does not
        // accept a session-id resume flag — `codex resume <id>` is a
        // separate interactive subcommand. So when the wrapper passes
        // `resume_session_id` for a codex role we log it and continue
        // with a fresh session, instead of silently dropping it. The
        // user-facing hint at `spawn_role` already warns about this so
        // they aren't surprised when context isn't carried forward.
        // Tracked as a follow-up — see `docs/proposed-amendments.md`
        // A-006 "codex/gemini resume parity is deferred".
        if config.resume_session_id.is_some() {
            tracing::debug!(
                role = %config.name,
                "ignoring resume_session_id: codex mcp-server has no resume flag"
            );
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
        let (internal_stop_tx, internal_stop_rx) = mpsc::channel::<StopReason>(1);
        let (interrupt_tx, interrupt_rx) =
            mpsc::channel::<TurnId>(crate::adapter::INTERRUPT_CHANNEL_CAPACITY);
        let stopping = Arc::new(AtomicBool::new(false));

        let pending: Arc<Mutex<HashMap<u64, PendingEntry>>> = Arc::new(Mutex::new(HashMap::new()));
        let writer = Arc::new(Mutex::new(stdin));
        // Shared between `write_loop` (publisher) and
        // `drain_codex_interrupts` (consumer): the JSON-RPC id of the
        // in-flight `tools/call`, or `None` between turns. `write_loop`
        // sets it before the request goes out and clears it on
        // response/error so the cancel drainer always targets the
        // *current* turn, not an `initialize` id or a stale id sitting
        // in `pending` during a deferred cleanup window.
        let current_tools_call: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        // The `tools/call` request id that the user halted, if any.
        // `drain_codex_interrupts` sets this to the in-flight id when
        // it sends `notifications/cancelled`; `write_loop` consumes
        // (and clears) the value when that exact id's response/error
        // fires. Bound to the id rather than a free-floating bool so
        // a stale cancel can never be misattributed to a *later*
        // turn's legitimate error.
        let halted_request_id: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

        // Spawn the JSON-RPC reader: parses each line, dispatches
        // responses by id to the matching pending request, and routes
        // server-initiated approval requests through the permission
        // bridge so the user actually gets prompted.
        let runtime = CodexRuntime {
            role: config.name.clone(),
            events: tx_events.clone(),
            stopping: Arc::clone(&stopping),
        };
        tokio::spawn(read_rpc_loop(
            stdout,
            Arc::clone(&pending),
            Arc::clone(&writer),
            runtime.clone(),
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
            priors_text,
            config.budget_usd,
            config.permission_mode,
            rx_user,
            client,
            Arc::clone(&current_tools_call),
            Arc::clone(&halted_request_id),
            CodexWriteRuntime {
                base: runtime,
                internal_stop: internal_stop_tx,
            },
        ));

        // Turn-cancellation drain. On every interrupt request:
        // 1. Look up the in-flight `tools/call` request id from
        //    `current_tools_call`.
        // 2. Stamp it into `halted_request_id` so `write_loop` can
        //    recognise the resulting RPC error as a user halt (not a
        //    real protocol failure) when it sees a matching id.
        // 3. Emit `notifications/cancelled` per MCP 2024-11-05.
        tokio::spawn(drain_codex_interrupts(
            config.name.clone(),
            interrupt_rx,
            Arc::clone(&current_tools_call),
            Arc::clone(&halted_request_id),
            Arc::clone(&writer),
        ));

        tokio::spawn(wait_child(
            config.name.clone(),
            child,
            tx_events,
            stop_rx,
            internal_stop_rx,
            stopping,
        ));

        Ok(RoleHandle::new(
            config.name,
            Engine::Codex,
            tx_user,
            rx_events,
            stop_tx,
            interrupt_tx,
        ))
    }
}

async fn drain_codex_interrupts(
    role: String,
    mut rx: mpsc::Receiver<TurnId>,
    current_tools_call: Arc<Mutex<Option<u64>>>,
    halted_request_id: Arc<Mutex<Option<u64>>>,
    writer: Arc<Mutex<ChildStdin>>,
) {
    while let Some(turn_id) = rx.recv().await {
        let request_id = *current_tools_call.lock().await;
        match request_id {
            Some(id) => {
                debug!(
                    role = %role,
                    turn_id = %turn_id,
                    request_id = id,
                    "codex notifications/cancelled"
                );
                // Stamp the in-flight id BEFORE the cancel goes out
                // so a quick RPC error reaching write_loop already
                // sees the matching id and recognises this as a
                // user halt. write_loop only consumes the marker
                // when the matching id's response/error fires, so a
                // stale cancel cannot leak to a later turn.
                *halted_request_id.lock().await = Some(id);
                if let Err(error) = send_codex_cancellation(&writer, id).await {
                    warn!(role = %role, %error, "codex cancellation notification write failed");
                }
            }
            None => {
                debug!(
                    role = %role,
                    turn_id = %turn_id,
                    "codex cancel requested with no in-flight tools/call"
                );
            }
        }
    }
}

async fn send_codex_cancellation(
    writer: &Arc<Mutex<ChildStdin>>,
    request_id: u64,
) -> std::io::Result<()> {
    let line = codex_cancellation_line(request_id);
    let mut w = writer.lock().await;
    w.write_all(line.as_bytes()).await?;
    w.flush().await
}

/// JSON-RPC envelope CodeRoom sends to codex on `/halt`. Pulled out
/// of the writer path so unit tests can verify the wire shape without
/// constructing an `Arc<Mutex<ChildStdin>>`.
fn codex_cancellation_line(request_id: u64) -> String {
    let envelope = json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {
            "requestId": request_id,
            "reason": "CodeRoom user halted turn",
        },
    });
    format!("{envelope}\n")
}

#[derive(Clone)]
struct CodexRuntime {
    role: String,
    events: mpsc::Sender<CrepEvent>,
    stopping: Arc<AtomicBool>,
}

struct CodexWriteRuntime {
    base: CodexRuntime,
    internal_stop: mpsc::Sender<StopReason>,
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

/// One outstanding JSON-RPC request. Holds the response oneshot and an
/// activity Notify that the read loop pokes whenever it sees a
/// `codex/event` notification carrying our `_meta.requestId`. The idle
/// timer in `RpcClient::request` resets on each notify, so a healthy
/// codex turn that streams events stays alive indefinitely.
struct PendingEntry {
    responder: oneshot::Sender<JsonRpcResponse>,
    activity: Arc<Notify>,
    partial_text: Arc<Mutex<String>>,
}

#[derive(Clone)]
struct RpcClient {
    writer: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, PendingEntry>>>,
    next_id: Arc<Mutex<u64>>,
}

impl RpcClient {
    fn new(
        writer: Arc<Mutex<ChildStdin>>,
        pending: Arc<Mutex<HashMap<u64, PendingEntry>>>,
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
    ///
    /// Uses an **idle** timeout (`RPC_IDLE_TIMEOUT`): the deadline resets
    /// every time the read loop signals activity for our request id. A
    /// codex turn that's actively streaming `codex/event` notifications
    /// will never fire this; only a wedged server (no events at all for
    /// six minutes) gets cut off.
    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.alloc_id().await;
        self.request_with_id(method, params, id).await
    }

    /// Same as [`Self::request`], but uses a caller-allocated `id`. Lets
    /// `write_loop` publish the `tools/call` id into a shared tracker
    /// before the request is in flight, so the cancel drainer can target
    /// the exact id without inferring it from `pending`.
    async fn request_with_id(
        &self,
        method: &str,
        params: Value,
        id: u64,
    ) -> Result<Value, RpcError> {
        self.request_with_id_capture(method, params, id).await.0
    }

    async fn request_with_id_capture(
        &self,
        method: &str,
        params: Value,
        id: u64,
    ) -> (Result<Value, RpcError>, String) {
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let (tx, rx) = oneshot::channel();
        let activity = Arc::new(Notify::new());
        let partial_text = Arc::new(Mutex::new(String::new()));
        self.pending.lock().await.insert(
            id,
            PendingEntry {
                responder: tx,
                activity: Arc::clone(&activity),
                partial_text: Arc::clone(&partial_text),
            },
        );
        let _guard = PendingRequestGuard {
            id,
            pending: Arc::clone(&self.pending),
        };

        let line = format!("{envelope}\n");
        {
            let mut w = self.writer.lock().await;
            if w.write_all(line.as_bytes()).await.is_err() || w.flush().await.is_err() {
                self.pending.lock().await.remove(&id);
                return (Err(RpcError::Closed), String::new());
            }
        }

        let response = match wait_with_idle_timeout(rx, &activity, RPC_IDLE_TIMEOUT).await {
            Ok(response) => response,
            Err(error) => {
                let partial = partial_text.lock().await.clone();
                return (Err(error), partial);
            }
        };
        let partial = partial_text.lock().await.clone();
        if let Some(err) = response.error {
            return (Err(RpcError::Server(err.to_string())), partial);
        }
        (Ok(response.result.unwrap_or(Value::Null)), partial)
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
    pending: Arc<Mutex<HashMap<u64, PendingEntry>>>,
}

/// Wait for a JSON-RPC response, treating the deadline as an **idle**
/// timeout rather than a total cap. Each `activity.notify_one()` from the
/// read loop resets the deadline; only stretches of complete silence
/// trigger `RpcError::Timeout`. The response oneshot still wins races
/// against both activity and the deadline.
async fn wait_with_idle_timeout(
    mut rx: oneshot::Receiver<JsonRpcResponse>,
    activity: &Notify,
    idle: Duration,
) -> Result<JsonRpcResponse, RpcError> {
    let mut deadline = Instant::now() + idle;
    loop {
        tokio::select! {
            biased;
            response = &mut rx => {
                return response.map_err(|_| RpcError::Closed);
            }
            () = activity.notified() => {
                deadline = Instant::now() + idle;
            }
            () = tokio::time::sleep_until(deadline) => {
                return Err(RpcError::Timeout);
            }
        }
    }
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
    pending: Arc<Mutex<HashMap<u64, PendingEntry>>>,
    writer: Arc<Mutex<tokio::process::ChildStdin>>,
    runtime: CodexRuntime,
    permission_socket: Option<PathBuf>,
    permission_policy_path: Option<PathBuf>,
) {
    let mut lines = BufReader::new(stdout).lines();
    let mut fallback_delta_sequence = 0u64;
    while let Ok(Some(line)) = lines.next_line().await {
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(error) => {
                warn!(role = %runtime.role, %error, line = %line, "non-JSON line on codex stdout");
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
                if runtime.stopping.load(Ordering::SeqCst) {
                    continue;
                }
                let writer = Arc::clone(&writer);
                let role_name = runtime.role.clone();
                let socket = permission_socket.clone();
                let policy_path = permission_policy_path.clone();
                let events = runtime.events.clone();
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
                    debug!(role = %runtime.role, line = %line, "non-numeric response id, dropping");
                    continue;
                };
                let response = JsonRpcResponse {
                    result: value.get("result").cloned(),
                    error: value.get("error").cloned(),
                };
                if let Some(entry) = pending.lock().await.remove(&id) {
                    let _ = entry.responder.send(response);
                }
            }
            (false, true) => {
                if runtime.stopping.load(Ordering::SeqCst) {
                    continue;
                }
                // codex/event notifications carry _meta.requestId pointing
                // at the in-flight tools/call. Poke the matching pending
                // entry's activity Notify so the request's idle timer
                // resets and a long, busy turn doesn't get cut off.
                let request_state = if let Some(req_id) = codex_event_request_id(&value) {
                    let pending = pending.lock().await;
                    pending
                        .get(&req_id)
                        .map(|entry| (Arc::clone(&entry.activity), Arc::clone(&entry.partial_text)))
                } else {
                    None
                };
                if let Some((activity, _)) = &request_state {
                    activity.notify_one();
                }
                if let Some(mut event) = codex_notification_to_event(&runtime.role, &value) {
                    if let CrepEvent::RoleOutputDelta {
                        text_delta,
                        sequence,
                        ..
                    } = &mut event
                    {
                        if *sequence == 0 {
                            fallback_delta_sequence = fallback_delta_sequence.saturating_add(1);
                            *sequence = fallback_delta_sequence;
                        } else {
                            fallback_delta_sequence = fallback_delta_sequence.max(*sequence);
                        }
                        if let Some((_, partial_text)) = &request_state {
                            partial_text.lock().await.push_str(text_delta);
                        }
                        let _ = runtime.events.try_send(event);
                    } else {
                        let _ = runtime.events.send(event).await;
                    }
                } else {
                    debug!(role = %runtime.role, line = %line, "codex notification ignored");
                }
            }
            (false, false) => {
                debug!(role = %runtime.role, line = %line, "malformed codex stdout line");
            }
        }
    }
    let mut guard = pending.lock().await;
    for (_, entry) in guard.drain() {
        let _ = entry.responder.send(JsonRpcResponse {
            result: None,
            error: Some(json!({"code": -32000, "message": "codex rpc disconnected"})),
        });
    }
    debug!(role = %runtime.role, "codex rpc loop exiting");
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
                turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
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

/// Pull `params._meta.requestId` from a codex stdout JSON-RPC envelope.
/// Returns `None` if the line isn't a `codex/event` notification or
/// doesn't carry a numeric requestId we can match against `pending`.
fn codex_event_request_id(value: &Value) -> Option<u64> {
    let method = value.get("method").and_then(Value::as_str)?;
    if method != "codex/event" {
        return None;
    }
    value
        .get("params")?
        .get("_meta")?
        .get("requestId")?
        .as_u64()
}

/// Translate one JSON-RPC line from codex stdout into a CodeRoom CREP
/// event. Codex 0.130+ wraps every lifecycle/tool/text update inside a
/// single `codex/event` notification with the actual variant name in
/// `params.msg.type`. Earlier guesses at `notifications/exec_command_*`
/// never matched anything codex actually sent, which is why work-card
/// steps stayed empty for codex roles before this.
///
/// We only translate the events the REPL renders today:
///
/// - `exec_command_begin` → [`CrepEvent::ToolCallProposed`] (Bash)
/// - `exec_command_end`   → [`CrepEvent::ToolCallExecuted`]
/// - `agent_message_content_delta` → [`CrepEvent::RoleOutputDelta`]
///
/// Everything else (token_count, session_configured, …) is intentionally
/// dropped — the activity-poke in `read_rpc_loop` still uses them to keep
/// the idle timer fresh.
fn codex_notification_to_event(role: &str, value: &Value) -> Option<CrepEvent> {
    let method = value.get("method").and_then(Value::as_str)?;
    if method != "codex/event" {
        return None;
    }
    let params = value.get("params")?;
    let msg = params.get("msg")?;
    let msg_type = msg.get("type").and_then(Value::as_str)?;
    let call_id = msg
        .get("call_id")
        .and_then(Value::as_str)
        .unwrap_or(msg_type)
        .to_owned();
    match msg_type {
        "exec_command_begin" => {
            let command_summary = msg
                .get("command")
                .map_or_else(|| Value::Null, codex_command_summary);
            let cwd = msg.get("cwd").cloned();
            let mut tool_input = serde_json::Map::new();
            if !command_summary.is_null() {
                tool_input.insert("command".to_owned(), command_summary);
            }
            if let Some(cwd) = cwd {
                tool_input.insert("cwd".to_owned(), cwd);
            }
            Some(CrepEvent::ToolCallProposed {
                role: role.to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: Value::Object(tool_input),
                tool_use_id: call_id,
                turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
            })
        }
        "exec_command_end" => {
            let exit_code = msg.get("exit_code").and_then(Value::as_i64);
            let ok = exit_code.is_none_or(|code| code == 0);
            // On failure, prefer stderr — that's where the actionable
            // error lives. On success, prefer the unified output streams.
            // Empty fields are skipped; the first non-empty candidate wins.
            let candidates: &[&str] = if ok {
                &["aggregated_output", "formatted_output", "stdout", "stderr"]
            } else {
                &["stderr", "aggregated_output", "formatted_output", "stdout"]
            };
            let summary = candidates
                .iter()
                .find_map(|key| {
                    msg.get(*key)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                })
                .map_or_else(
                    || {
                        if ok {
                            "ok".to_owned()
                        } else {
                            format!("exit {}", exit_code.unwrap_or_default())
                        }
                    },
                    ToOwned::to_owned,
                );
            Some(CrepEvent::ToolCallExecuted {
                role: role.to_owned(),
                tool_use_id: call_id,
                ok,
                output_summary: summary.chars().take(200).collect(),
                turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
            })
        }
        "agent_message_content_delta" => {
            let delta = msg
                .get("delta")
                .or_else(|| msg.get("text"))
                .or_else(|| msg.get("content"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if delta.is_empty() {
                return None;
            }
            Some(CrepEvent::RoleOutputDelta {
                role: role.to_owned(),
                text_delta: delta.to_owned(),
                sequence: msg.get("sequence").and_then(Value::as_u64).unwrap_or(0),
                turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
            })
        }
        _ => None,
    }
}

/// Render codex's `command` array (e.g. `["/bin/bash", "-lc", "ls foo"]`)
/// as a single human-readable string for tool-call summaries. The common
/// `bash -c <script>` / `bash -lc <script>` shape collapses to just the
/// inner script so the WorkCard step shows what the model actually ran,
/// not the wrapper. Anything else is space-joined verbatim.
fn codex_command_summary(value: &Value) -> Value {
    if let Some(s) = value.as_str() {
        return Value::String(s.to_owned());
    }
    let Some(parts) = value.as_array() else {
        return value.clone();
    };
    let strs: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
    if strs.len() != parts.len() {
        return value.clone();
    }
    if strs.len() == 3 {
        let shell = strs[0];
        let flag = strs[1];
        let is_shell =
            shell.ends_with("/bash") || shell.ends_with("/sh") || shell == "bash" || shell == "sh";
        if is_shell && (flag == "-c" || flag == "-lc") {
            return Value::String(strs[2].to_owned());
        }
    }
    Value::String(strs.join(" "))
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

#[allow(
    clippy::too_many_arguments,
    reason = "all 8 are intrinsic state for the loop; bundling them into a struct only moves the noise"
)]
async fn write_loop(
    priors_text: String,
    budget_usd: f64,
    permission_mode: PermissionMode,
    mut rx: mpsc::Receiver<UserMessage>,
    client: RpcClient,
    current_tools_call: Arc<Mutex<Option<u64>>>,
    halted_request_id: Arc<Mutex<Option<u64>>>,
    runtime: CodexWriteRuntime,
) {
    let approval_policy = codex_approval_policy(permission_mode);
    let sandbox = codex_sandbox(permission_mode);
    while let Some(msg) = rx.recv().await {
        if runtime.base.stopping.load(Ordering::SeqCst) {
            break;
        }
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

        // Allocate the tools/call request id up front and publish it
        // into the cancel-tracker before sending. The drainer reads
        // this lock to know exactly which id to target with
        // `notifications/cancelled` — no inference from `pending`,
        // so initialize ids and stale-during-cleanup ids cannot win.
        let request_id = client.alloc_id().await;
        *current_tools_call.lock().await = Some(request_id);
        let (outcome, partial_text) = client
            .request_with_id_capture("tools/call", params, request_id)
            .await;
        *current_tools_call.lock().await = None;

        // Take the halted-id marker if it matches THIS request. A
        // stale marker from a previous turn — or a marker for some
        // future turn — stays in place; we only consume what we
        // know belongs to us. This is the fix for the previous
        // AtomicBool design that could leak a halt signal into the
        // next turn's legitimate error.
        let halted_match = {
            let mut guard = halted_request_id.lock().await;
            if *guard == Some(request_id) {
                *guard = None;
                true
            } else {
                false
            }
        };

        match outcome {
            Ok(result) => {
                if runtime.base.stopping.load(Ordering::SeqCst) {
                    break;
                }
                if halted_match {
                    // Codex acked the cancel after the model already
                    // produced a final result — rare but possible.
                    // Honour the user's halt: emit TurnInterrupted
                    // and discard the late result so the REPL drain
                    // sees the boundary.
                    let _ = runtime
                        .base
                        .events
                        .send(turn_interrupted_event(&runtime.base.role, &partial_text))
                        .await;
                    continue;
                }
                let text = extract_text_from_tool_result(&result);
                for event in
                    crate::adapter::role_spoke_events_from_text(&runtime.base.role, &text, 0.0, 0)
                {
                    let _ = runtime.base.events.send(event).await;
                }
            }
            Err(error) => {
                warn!(role = %runtime.base.role, %error, "codex tools/call failed");
                if runtime.base.stopping.load(Ordering::SeqCst) {
                    break;
                }
                if matches!(&error, RpcError::Timeout) {
                    runtime.base.stopping.store(true, Ordering::SeqCst);
                    let _ = runtime.internal_stop.try_send(StopReason::TimedOut);
                    break;
                }
                // If the user halted *this* turn, the RPC error is
                // expected (codex closed the pending request after
                // our `notifications/cancelled` reached it). Emit
                // `TurnInterrupted` instead of an error `RoleSpoke`.
                // The id-bound marker means a stale halt cannot leak
                // into a later turn's legitimate failure.
                if halted_match {
                    let _ = runtime
                        .base
                        .events
                        .send(turn_interrupted_event(&runtime.base.role, &partial_text))
                        .await;
                    continue;
                }
                let text = format!("[codex error: {error}]");
                let _ = runtime
                    .base
                    .events
                    .send(CrepEvent::RoleSpoke {
                        role: runtime.base.role.clone(),
                        text,
                        mentions: Vec::new(),
                        cost_usd: 0.0,
                        cache_read: 0,
                        turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                        thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                    })
                    .await;
            }
        }
    }
    debug!(role = %runtime.base.role, "codex write loop exiting");
}

fn turn_interrupted_event(role: &str, partial_text: &str) -> CrepEvent {
    let trimmed = partial_text.trim().to_owned();
    let partial_mentions = if trimmed.is_empty() {
        Vec::new()
    } else {
        crate::adapter::cc::parse_mentions(&trimmed)
    };
    CrepEvent::TurnInterrupted {
        role: role.to_owned(),
        turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
        thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
        source: crate::crep::InterruptSource::UserHalt,
        partial_text: if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        },
        partial_mentions,
    }
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
    mut internal_stop_rx: mpsc::Receiver<StopReason>,
    stopping: Arc<AtomicBool>,
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
            stopping.store(true, Ordering::SeqCst);
            terminate_child(&role, &mut child).await;
            reason
        },
        requested = internal_stop_rx.recv() => {
            let reason = requested.unwrap_or(StopReason::Crashed);
            stopping.store(true, Ordering::SeqCst);
            terminate_child(&role, &mut child).await;
            reason
        }
    };
    stopping.store(true, Ordering::SeqCst);
    let _ = events
        .send(CrepEvent::RoleStopped {
            role,
            reason,
            turn_id: None,
        })
        .await;
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
                    resume_session_id: None,
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
                ..
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
    fn codex_event_translates_exec_command_begin_and_end() {
        // Shape captured from real codex 0.130.0 mcp-server output. The
        // wrapper used to listen for `notifications/exec_command_*` and
        // never matched anything codex sent — work-card steps stayed
        // empty for codex roles. Lock the new shape in.
        let begin = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "codex/event",
            "params": {
                "_meta": {"requestId": 2, "threadId": "t-1"},
                "id": "",
                "msg": {
                    "type": "exec_command_begin",
                    "call_id": "call_5TJJ",
                    "command": ["/bin/bash", "-lc", "ls -la"],
                    "cwd": "/home/me/repo",
                    "turn_id": "2",
                    "started_at_ms": 1_778_427_200_345_i64,
                }
            }
        });
        let event = codex_notification_to_event("security", &begin).expect("begin event");
        match event {
            CrepEvent::ToolCallProposed {
                role,
                tool_name,
                tool_input,
                tool_use_id,
                ..
            } => {
                assert_eq!(role, "security");
                assert_eq!(tool_name, "Bash");
                assert_eq!(tool_use_id, "call_5TJJ");
                // bash -lc <script> collapses to just the script.
                assert_eq!(tool_input["command"], json!("ls -la"));
                assert_eq!(tool_input["cwd"], "/home/me/repo");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let end = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "codex/event",
            "params": {
                "_meta": {"requestId": 2, "threadId": "t-1"},
                "msg": {
                    "type": "exec_command_end",
                    "call_id": "call_5TJJ",
                    "exit_code": 0,
                    "stdout": "Cargo.toml\nsrc\n",
                    "stderr": "",
                    "aggregated_output": "Cargo.toml\nsrc\n",
                    "duration": {"secs": 0, "nanos": 11546},
                    "status": "completed",
                }
            }
        });
        let event = codex_notification_to_event("security", &end).expect("end event");
        match event {
            CrepEvent::ToolCallExecuted {
                role,
                tool_use_id,
                ok,
                output_summary,
                ..
            } => {
                assert_eq!(role, "security");
                assert_eq!(tool_use_id, "call_5TJJ");
                assert!(ok);
                // Newlines preserved; trimmed and capped at 200 chars.
                assert!(output_summary.contains("Cargo.toml"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn exec_command_end_marks_failure_when_exit_nonzero() {
        let end = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "codex/event",
            "params": {
                "_meta": {"requestId": 1},
                "msg": {
                    "type": "exec_command_end",
                    "call_id": "c1",
                    "exit_code": 2,
                    "stderr": "oops",
                    "stdout": "",
                    "status": "failed",
                }
            }
        });
        let event = codex_notification_to_event("qa", &end).expect("end event");
        match event {
            CrepEvent::ToolCallExecuted {
                ok, output_summary, ..
            } => {
                assert!(!ok);
                assert_eq!(output_summary, "oops");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn codex_agent_message_delta_becomes_role_output_delta() {
        let value = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "codex/event",
            "params": {
                "_meta": {"requestId": 1},
                "msg": {
                    "type": "agent_message_content_delta",
                    "delta": "partial answer",
                    "sequence": 7
                }
            }
        });
        let event = codex_notification_to_event("qa", &value).expect("delta event");
        match event {
            CrepEvent::RoleOutputDelta {
                role,
                text_delta,
                sequence,
                ..
            } => {
                assert_eq!(role, "qa");
                assert_eq!(text_delta, "partial answer");
                assert_eq!(sequence, 7);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn interrupted_event_carries_partial_text_and_mentions() {
        let event = turn_interrupted_event("security", "partial reply for @backend\n");
        match event {
            CrepEvent::TurnInterrupted {
                role,
                partial_text,
                partial_mentions,
                ..
            } => {
                assert_eq!(role, "security");
                assert_eq!(partial_text.as_deref(), Some("partial reply for @backend"));
                assert_eq!(partial_mentions, vec!["backend"]);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn unrelated_codex_msg_types_are_ignored() {
        // codex emits a flurry of types we don't surface yet (token_count,
        // mcp_startup_update, …). They must
        // not produce CrepEvents — the read loop only uses them as activity
        // signals for the idle timer.
        for msg_type in [
            "task_started",
            "token_count",
            "session_configured",
            "mcp_startup_update",
        ] {
            let value = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "codex/event",
                "params": {"_meta": {"requestId": 1}, "msg": {"type": msg_type}}
            });
            assert!(
                codex_notification_to_event("qa", &value).is_none(),
                "{msg_type} should be ignored"
            );
        }
    }

    #[test]
    fn codex_event_request_id_extracted() {
        let value = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "codex/event",
            "params": {"_meta": {"requestId": 17, "threadId": "t"}, "msg": {"type": "task_started"}}
        });
        assert_eq!(codex_event_request_id(&value), Some(17));

        // Non-codex/event lines (responses, server-initiated requests,
        // legacy notifications) must not return an id — they aren't
        // activity signals for our idle timer.
        let response = serde_json::json!({"jsonrpc": "2.0", "id": 7, "result": {}});
        assert_eq!(codex_event_request_id(&response), None);
        let other = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {}
        });
        assert_eq!(codex_event_request_id(&other), None);
    }

    #[test]
    fn command_summary_collapses_bash_wrapper() {
        let v = json!(["/bin/bash", "-lc", "cargo test --workspace"]);
        assert_eq!(codex_command_summary(&v), json!("cargo test --workspace"));

        let v = json!(["bash", "-c", "ls"]);
        assert_eq!(codex_command_summary(&v), json!("ls"));
    }

    #[test]
    fn command_summary_joins_other_argv() {
        let v = json!(["rg", "--json", "TODO"]);
        assert_eq!(codex_command_summary(&v), json!("rg --json TODO"));
    }

    #[test]
    fn command_summary_passes_strings_through() {
        let v = json!("ls");
        assert_eq!(codex_command_summary(&v), json!("ls"));
    }

    #[tokio::test]
    async fn idle_wait_returns_response_when_received() {
        let activity = Notify::new();
        let (tx, rx) = oneshot::channel();
        tx.send(JsonRpcResponse {
            result: Some(json!("ok")),
            error: None,
        })
        .unwrap();
        let response = wait_with_idle_timeout(rx, &activity, Duration::from_secs(1))
            .await
            .expect("response");
        assert_eq!(response.result, Some(json!("ok")));
    }

    #[tokio::test]
    async fn idle_wait_resets_on_activity() {
        // With a 200ms idle window, ten 50ms activity pokes should keep
        // the wait alive for at least 500ms — much longer than a static
        // timeout of the same window would have allowed.
        let activity = Arc::new(Notify::new());
        let activity_writer = Arc::clone(&activity);
        let (_tx, rx) = oneshot::channel::<JsonRpcResponse>();
        let pokes = tokio::spawn(async move {
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                activity_writer.notify_one();
            }
        });
        let started = Instant::now();
        let err = wait_with_idle_timeout(rx, &activity, Duration::from_millis(200))
            .await
            .expect_err("rx never resolves");
        pokes.await.unwrap();
        let elapsed = started.elapsed();
        assert!(matches!(err, RpcError::Timeout));
        assert!(
            elapsed >= Duration::from_millis(500),
            "expected idle resets to extend the wait, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn idle_wait_times_out_on_silence() {
        let activity = Notify::new();
        let (_tx, rx) = oneshot::channel::<JsonRpcResponse>();
        let started = Instant::now();
        let err = wait_with_idle_timeout(rx, &activity, Duration::from_millis(80))
            .await
            .expect_err("nothing arrives");
        assert!(matches!(err, RpcError::Timeout));
        assert!(started.elapsed() >= Duration::from_millis(70));
    }

    #[test]
    fn cancellation_envelope_matches_mcp_shape() {
        // Per MCP 2024-11-05, the cancellation notification is
        // `notifications/cancelled` with params `{ requestId, reason }`.
        // Codex matches the camelCase requestId on its side; locking
        // both keys + JSON-RPC version in a unit test guards the wire
        // shape against future refactors.
        let line = codex_cancellation_line(42);
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "notifications/cancelled");
        assert_eq!(parsed["params"]["requestId"], 42);
        assert!(parsed["params"]["reason"].is_string());
        // No top-level `id` — this is a notification, not a request.
        assert!(parsed.get("id").is_none());
        // Trailing newline so codex's line-delimited reader sees a
        // complete JSON-RPC frame.
        assert!(line.ends_with('\n'));
    }
}
