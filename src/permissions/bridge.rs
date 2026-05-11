//! Permission decision IPC between hook subprocesses and the live REPL.
//!
//! When a hook-backed adapter (today only Claude Code) sees an `ask`
//! verdict it would normally have to translate that to either `deny`
//! (the safe default with no UI) or `ask` written to the engine — which
//! claude can't surface because we run with `--dangerously-skip-permissions`.
//!
//! Instead, the cc adapter exports a Unix-domain-socket path through the
//! `CODEROOM_PERMISSION_SOCKET` environment variable, and the hook
//! subprocess (which is `cr __coderoom-hook-decision` re-exec'd by claude)
//! connects to that socket and asks the REPL to surface a real prompt.
//!
//! The protocol is one JSON object per line in each direction. Each TCP-
//! style connection is one request/response cycle; the hook subprocess
//! does not stay connected.
//!
//! ```text
//!   hook  →  cr REPL    {"v":1,"role":"backend","tool":"Bash","input":...,"reason":"..."}
//!   cr REPL →  hook     {"v":1,"decision":"allow","scope":"session","reason":"..."}
//! ```
//!
//! Failure modes are explicit and **always default to deny**: if the
//! socket env var is absent, if the connection refuses, if the response
//! is malformed, the hook returns deny. A broken bridge cannot silently
//! authorize a tool.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Environment variable carrying the socket path into the hook subprocess.
pub const BRIDGE_ENV_VAR: &str = "CODEROOM_PERMISSION_SOCKET";

/// Wire-format protocol version. Bumped when the JSON shape changes in
/// a way that older clients/servers can't ignore.
const PROTOCOL_VERSION: u8 = 1;

/// How long the hook waits on a socket connect / write / read before
/// giving up and returning [`BridgeError::Io`]. The user might be
/// genuinely thinking about the prompt, so we allow plenty of time on
/// the read; the connect/write side is generous in case the REPL was
/// painting the splash and the listener task hadn't accepted yet.
const READ_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Failure modes for the synchronous bridge client used by the hook
/// subprocess.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// `CODEROOM_PERMISSION_SOCKET` was unset — there is no live REPL.
    #[error("no live CodeRoom REPL bridge socket")]
    NoSocket,
    /// Failed to connect or talk over the socket.
    #[error("permission bridge IO error: {0}")]
    Io(String),
    /// Server returned a malformed or version-incompatible response.
    #[error("permission bridge protocol error: {0}")]
    Protocol(String),
}

/// User decision surfaced through the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionDecision {
    /// Let the engine run the proposed tool.
    Allow,
    /// Block the proposed tool.
    Deny,
}

/// Whether the user wants the decision remembered for the rest of the
/// session (persisted into the project policy file) or only applied to
/// this one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionScope {
    /// Apply only to this one tool invocation.
    Once,
    /// Persist into the session policy file so the same tool name
    /// is auto-handled the same way for the rest of the project.
    Session,
}

/// One inbound request as received by the REPL listener.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeRequest {
    /// Protocol version; today always [`PROTOCOL_VERSION`].
    pub v: u8,
    /// Role whose engine triggered the prompt.
    pub role: String,
    /// Tool name (e.g. `Bash`, `Edit`).
    pub tool: String,
    /// Engine-provided tool input. Free-form JSON; the REPL renders a
    /// best-effort summary line from this.
    pub input: Value,
    /// Human-readable reason from the policy decider.
    pub reason: String,
}

/// One outbound response from REPL to hook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeResponse {
    /// Protocol version; today always [`PROTOCOL_VERSION`].
    pub v: u8,
    /// Whether to allow or deny the proposed tool.
    pub decision: PermissionDecision,
    /// Whether to remember the decision for the rest of the session.
    pub scope: DecisionScope,
    /// Optional explanation written into the engine-side reason field.
    pub reason: String,
}

impl BridgeResponse {
    /// Conventional deny used by the listener when it can't surface a
    /// prompt (e.g. shutdown was requested while a hook was waiting).
    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            decision: PermissionDecision::Deny,
            scope: DecisionScope::Once,
            reason: reason.into(),
        }
    }
}

/// One bridge transaction surfaced to the REPL: a request paired with
/// the channel the listener expects the response on.
#[derive(Debug)]
pub struct BridgeRequestSink {
    /// The decoded request the hook subprocess sent us.
    pub request: BridgeRequest,
    /// Single-fire response channel. Dropping it without sending makes
    /// the listener return a deny to the hook so a panicked prompt
    /// path can never be silently approved.
    pub responder: BridgeResponder,
}

/// Type-safe wrapper around the `std::sync::mpsc::SyncSender` so the
/// REPL prompt code can't accidentally reuse it or send the wrong
/// message type.
#[derive(Debug)]
pub struct BridgeResponder(std::sync::mpsc::SyncSender<BridgeResponse>);

impl BridgeResponder {
    /// Send the user's decision back to the hook subprocess. Idempotent
    /// — extra sends after the first are silently ignored.
    pub fn respond(self, response: BridgeResponse) {
        // Sync sender with capacity 1; if the receiver is gone the
        // hook already timed out, nothing we can do.
        let _ = self.0.send(response);
    }
}

impl Drop for BridgeResponder {
    fn drop(&mut self) {
        // Defensive: if the prompt path panicked or returned without
        // calling .respond(), default to deny.
        let _ = self.0.try_send(BridgeResponse::deny(
            "CodeRoom prompt path closed without a decision",
        ));
    }
}

/// Drop guard for a running listener task. On drop, removes the
/// socket file and signals the listener to exit.
#[derive(Debug)]
pub struct BridgeHandle {
    socket_path: PathBuf,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl BridgeHandle {
    /// Path to the Unix socket the bridge is listening on. Adapters
    /// export this through the [`BRIDGE_ENV_VAR`] env var so hook
    /// subprocesses can find it.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Start the listener at `socket_path`. The returned receiver yields one
/// item per inbound request; the REPL consumes them on its main task.
///
/// Removing a stale socket from a previous crashed run is best-effort —
/// if the unlink fails (permission denied, etc.) the bind will fail too
/// and the caller learns about it.
pub fn start(
    socket_path: PathBuf,
) -> std::io::Result<(BridgeHandle, tokio::sync::mpsc::Receiver<BridgeRequestSink>)> {
    let _ = std::fs::remove_file(&socket_path);
    let listener = tokio::net::UnixListener::bind(&socket_path)?;
    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<BridgeRequestSink>(8);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let req_tx = req_tx.clone();
                    tokio::spawn(handle_connection(stream, req_tx));
                }
            }
        }
    });
    Ok((
        BridgeHandle {
            socket_path,
            shutdown: Some(shutdown_tx),
        },
        req_rx,
    ))
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    req_tx: tokio::sync::mpsc::Sender<BridgeRequestSink>,
) {
    use tokio::io::{AsyncBufReadExt, BufReader as TokioBufReader};

    let (read_half, write_half) = stream.into_split();
    let mut reader = TokioBufReader::new(read_half).lines();
    let mut writer = write_half;

    let Ok(Some(line)) = reader.next_line().await else {
        return;
    };
    let request: BridgeRequest = match serde_json::from_str(&line) {
        Ok(req) => req,
        Err(error) => {
            let response = BridgeResponse::deny(format!("malformed bridge request: {error}"));
            let _ = write_response(&mut writer, &response).await;
            return;
        }
    };
    if request.v != PROTOCOL_VERSION {
        let response = BridgeResponse::deny(format!(
            "unsupported permission bridge protocol version {}",
            request.v
        ));
        let _ = write_response(&mut writer, &response).await;
        return;
    }

    let (resp_tx, resp_rx) = std::sync::mpsc::sync_channel::<BridgeResponse>(1);
    let sink = BridgeRequestSink {
        request,
        responder: BridgeResponder(resp_tx),
    };
    // try_send instead of `send().await` — if the queue is full (REPL
    // is wedged or just slow), reply deny immediately instead of
    // blocking the listener task. The previous .await meant a 9th
    // concurrent request stalled all later ones for up to READ_TIMEOUT.
    if let Err(error) = req_tx.try_send(sink) {
        let reason = match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => {
                "CodeRoom permission queue is full; rejecting request"
            }
            tokio::sync::mpsc::error::TrySendError::Closed(_) => "CodeRoom REPL is shutting down",
        };
        let response = BridgeResponse::deny(reason);
        let _ = write_response(&mut writer, &response).await;
        return;
    }

    // Block in spawn_blocking so we don't tie up an async worker for
    // the (possibly long) duration of the user thinking.
    let response = tokio::task::spawn_blocking(move || resp_rx.recv())
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_else(|| BridgeResponse::deny("CodeRoom prompt path produced no decision"));

    let _ = write_response(&mut writer, &response).await;
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &BridgeResponse,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut bytes = serde_json::to_vec(response).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}

/// Async client used by in-process adapter tasks (codex's MCP receive
/// loop, etc.) to ask the same listener for a permission decision. The
/// hook subprocess uses [`request_decision_blocking`]; both paths share
/// the wire format so the listener doesn't care which one it answered.
///
/// Pass the bridge socket path explicitly here since adapter tasks know
/// it from `RoleConfig::permission_socket_path` and shouldn't depend on
/// process-global env state.
pub async fn request_decision_async(
    socket_path: &Path,
    role: &str,
    tool: &str,
    input: &Value,
    reason: &str,
) -> Result<BridgeResponse, BridgeError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};

    let mut stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .map_err(|error| {
            BridgeError::Io(format!("connecting {}: {error}", socket_path.display()))
        })?;

    let request = BridgeRequest {
        v: PROTOCOL_VERSION,
        role: role.to_owned(),
        tool: tool.to_owned(),
        input: input.clone(),
        reason: reason.to_owned(),
    };
    let mut send_buf = serde_json::to_vec(&request)
        .map_err(|error| BridgeError::Io(format!("serializing permission request: {error}")))?;
    send_buf.push(b'\n');
    stream
        .write_all(&send_buf)
        .await
        .map_err(|error| BridgeError::Io(format!("writing request: {error}")))?;
    stream
        .flush()
        .await
        .map_err(|error| BridgeError::Io(format!("flushing request: {error}")))?;

    let (read_half, _) = stream.into_split();
    let mut reader = TokioBufReader::new(read_half).lines();
    let line = match tokio::time::timeout(READ_TIMEOUT, reader.next_line()).await {
        Ok(Ok(Some(line))) => line,
        Ok(Ok(None)) => {
            return Err(BridgeError::Protocol(
                "permission bridge closed without sending a response".to_owned(),
            ));
        }
        Ok(Err(error)) => {
            return Err(BridgeError::Io(format!("reading response: {error}")));
        }
        Err(_) => {
            return Err(BridgeError::Io(
                "permission bridge response timed out".to_owned(),
            ))
        }
    };

    let response: BridgeResponse = serde_json::from_str(line.trim_end_matches('\n'))
        .map_err(|error| BridgeError::Protocol(format!("decoding response: {error}")))?;
    if response.v != PROTOCOL_VERSION {
        return Err(BridgeError::Protocol(format!(
            "unsupported response protocol version {}",
            response.v
        )));
    }
    Ok(response)
}

/// Synchronous client used by the hook subprocess. Connects to the
/// socket pointed at by `CODEROOM_PERMISSION_SOCKET`, sends a request,
/// reads one response. Returns [`BridgeError::NoSocket`] if the env var
/// is missing.
pub fn request_decision_blocking(
    role: &str,
    tool: &str,
    input: &Value,
    reason: &str,
) -> Result<BridgeResponse, BridgeError> {
    let socket_path = match std::env::var(BRIDGE_ENV_VAR) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => return Err(BridgeError::NoSocket),
    };

    let stream = UnixStream::connect(&socket_path).map_err(|error| {
        BridgeError::Io(format!("connecting {}: {error}", socket_path.display()))
    })?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|error| BridgeError::Io(format!("setting read timeout: {error}")))?;
    stream
        .set_write_timeout(Some(CONNECT_WRITE_TIMEOUT))
        .map_err(|error| BridgeError::Io(format!("setting write timeout: {error}")))?;

    let request = BridgeRequest {
        v: PROTOCOL_VERSION,
        role: role.to_owned(),
        tool: tool.to_owned(),
        input: input.clone(),
        reason: reason.to_owned(),
    };

    let mut send_buf = serde_json::to_vec(&request)
        .map_err(|error| BridgeError::Io(format!("serializing permission request: {error}")))?;
    send_buf.push(b'\n');

    let mut writer = &stream;
    writer
        .write_all(&send_buf)
        .map_err(|error| BridgeError::Io(format!("writing request: {error}")))?;
    writer
        .flush()
        .map_err(|error| BridgeError::Io(format!("flushing request: {error}")))?;

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .map_err(|error| BridgeError::Io(format!("reading response: {error}")))?;
    if read == 0 {
        return Err(BridgeError::Protocol(
            "permission bridge closed without sending a response".to_owned(),
        ));
    }

    let response: BridgeResponse = serde_json::from_str(line.trim_end_matches('\n'))
        .map_err(|error| BridgeError::Protocol(format!("decoding response: {error}")))?;
    if response.v != PROTOCOL_VERSION {
        return Err(BridgeError::Protocol(format!(
            "unsupported response protocol version {}",
            response.v
        )));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::Mutex;

    /// Both `client_returns_no_socket_when_env_var_missing` and
    /// `end_to_end_round_trip_over_unix_socket` mutate the process-wide
    /// `CODEROOM_PERMISSION_SOCKET`. cargo runs tests in parallel; this
    /// mutex serializes them so one test's `remove_var` can't race with
    /// the other's `set_var`. Tokio's async-aware `Mutex` is held across
    /// the round-trip's `.await` calls without tripping clippy.
    static ENV_TEST_LOCK: Mutex<()> = Mutex::const_new(());

    #[test]
    fn request_round_trips_through_json() {
        let req = BridgeRequest {
            v: PROTOCOL_VERSION,
            role: "backend".into(),
            tool: "Bash".into(),
            input: json!({"command": "ls"}),
            reason: "Bash requires approval under permission_mode=ask".into(),
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: BridgeRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.v, req.v);
        assert_eq!(back.role, req.role);
        assert_eq!(back.tool, req.tool);
        assert_eq!(back.input, req.input);
        assert_eq!(back.reason, req.reason);
    }

    #[test]
    fn response_round_trips_through_json() {
        let resp = BridgeResponse {
            v: PROTOCOL_VERSION,
            decision: PermissionDecision::Allow,
            scope: DecisionScope::Session,
            reason: "approved by user".into(),
        };
        let s = serde_json::to_string(&resp).unwrap();
        let back: BridgeResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(back.v, resp.v);
        assert_eq!(back.decision, resp.decision);
        assert_eq!(back.scope, resp.scope);
    }

    #[test]
    fn deny_helper_uses_decision_once_scope() {
        let resp = BridgeResponse::deny("nope");
        assert_eq!(resp.decision, PermissionDecision::Deny);
        assert_eq!(resp.scope, DecisionScope::Once);
        assert_eq!(resp.reason, "nope");
    }

    #[test]
    fn responder_drop_yields_deny() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let responder = BridgeResponder(tx);
        drop(responder);
        let response = rx.recv().expect("dropped responder must yield a response");
        assert_eq!(response.decision, PermissionDecision::Deny);
    }

    #[tokio::test]
    async fn client_returns_no_socket_when_env_var_missing() {
        let _guard = ENV_TEST_LOCK.lock().await;
        // SAFETY: this test mutates a process-global env var. In Rust 2024
        // edition this is `unsafe`; CodeRoom builds on edition 2021 today
        // so the call is safe but stays here as documentation.
        std::env::remove_var(BRIDGE_ENV_VAR);
        let err = request_decision_blocking("host", "Bash", &json!({}), "test").unwrap_err();
        assert!(matches!(err, BridgeError::NoSocket));
    }

    #[tokio::test]
    async fn end_to_end_round_trip_over_unix_socket() {
        let _guard = ENV_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permission.sock");
        let (handle, mut rx) = start(path.clone()).unwrap();

        // SAFETY: see the env-var test above.
        std::env::set_var(BRIDGE_ENV_VAR, &path);

        // Drive the listener: accept one request, respond allow-once.
        let listener_task = tokio::spawn(async move {
            let sink = rx.recv().await.expect("one request");
            assert_eq!(sink.request.role, "backend");
            assert_eq!(sink.request.tool, "Bash");
            sink.responder.respond(BridgeResponse {
                v: PROTOCOL_VERSION,
                decision: PermissionDecision::Allow,
                scope: DecisionScope::Once,
                reason: "ok".into(),
            });
        });

        // Drive the client from a blocking thread (mimics the hook).
        let response = tokio::task::spawn_blocking(|| {
            request_decision_blocking("backend", "Bash", &json!({"command": "ls"}), "test")
        })
        .await
        .unwrap()
        .expect("bridge round trip succeeds");

        assert_eq!(response.decision, PermissionDecision::Allow);
        assert_eq!(response.scope, DecisionScope::Once);

        listener_task.await.unwrap();
        drop(handle);
        std::env::remove_var(BRIDGE_ENV_VAR);
    }
}
