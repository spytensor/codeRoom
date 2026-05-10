//! Gemini engine adapter.
//!
//! v0.1 implementation is intentionally one-shot per turn: for each user
//! message we spawn `gemini -p`, parse its `stream-json` stdout, emit any
//! tool lifecycle events, then finish with a single [`CrepEvent::RoleSpoke`].
//! There is no long-lived subprocess.
//!
//! Why so simple at v0.1:
//!
//! - Gemini must expose `--system-instruction-file`; otherwise CodeRoom
//!   refuses to start the role instead of concatenating priors into the
//!   user prompt and losing system-prompt isolation.
//! - `-y` (yolo) skips approval prompts, mirroring CC's
//!   `--dangerously-skip-permissions`. The wrapper-side gate over Gemini
//!   tool calls lands once we plumb its hook system (Gemini ships
//!   `gemini hooks migrate` that imports CC hook configs — same payload
//!   format).
//! - No wrapper-side permission gate or multi-turn cache reuse. Gemini
//!   exposes tool events in `stream-json`, but not a hook protocol that lets
//!   CodeRoom approve or deny them before execution.
//!
//! `priors_hash` reuses [`crate::adapter::cc::fingerprint`] so the value
//! is comparable across engines.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

#[cfg(unix)]
use nix::sys::signal::{kill, Signal};
#[cfg(unix)]
use nix::unistd::Pid;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};

use crate::adapter::{
    AdapterError, AdapterResult, Engine, EngineAdapter, PermissionMode, RoleConfig, RoleHandle,
    UserMessage,
};
use crate::crep::{CrepEvent, StopReason};
use crate::turn::TurnId;

const CHANNEL_CAPACITY: usize = 32;
const GEMINI_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const GEMINI_UNTRUSTED_PRIORS_ENV: &str = "CODEROOM_GEMINI_UNTRUSTED_PRIORS";
const TOOL_OUTPUT_SUMMARY_MAX_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeminiPromptMode {
    SystemInstructionFile,
    InlineUntrusted,
}

/// Adapter that drives the Gemini CLI in one-shot per-turn mode.
#[derive(Debug, Clone)]
pub struct GeminiAdapter {
    gemini_path: PathBuf,
}

impl Default for GeminiAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GeminiAdapter {
    /// Construct an adapter that resolves `gemini` via the user's `PATH`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            gemini_path: PathBuf::from("gemini"),
        }
    }

    /// Construct an adapter pointing at a specific `gemini` binary.
    #[must_use]
    pub fn with_path(gemini_path: PathBuf) -> Self {
        Self { gemini_path }
    }
}

impl EngineAdapter for GeminiAdapter {
    fn engine(&self) -> Engine {
        Engine::Gemini
    }

    async fn start(&self, config: RoleConfig) -> AdapterResult<RoleHandle> {
        if config.permission_mode != PermissionMode::Bypass {
            return Err(AdapterError::Engine {
                engine: Engine::Gemini.as_str(),
                message: format!(
                    "Gemini roles require permission_mode=\"bypass\"; \
                     CodeRoom cannot yet supervise Gemini tool approvals \
                     in permission_mode=\"{}\"",
                    config.permission_mode.as_str()
                ),
            });
        }

        let prompt_mode =
            probe_gemini(&self.gemini_path)
                .await
                .map_err(|message| AdapterError::Engine {
                    engine: Engine::Gemini.as_str(),
                    message,
                })?;
        let priors_text = tokio::fs::read_to_string(&config.priors_path)
            .await
            .map_err(|source| AdapterError::PriorsRead {
                path: config.priors_path.clone(),
                source,
            })?;
        let priors_hash = crate::adapter::cc::fingerprint(&priors_text);

        let (tx_user, rx_user) = mpsc::channel::<UserMessage>(CHANNEL_CAPACITY);
        let (tx_events, rx_events) = mpsc::channel::<CrepEvent>(CHANNEL_CAPACITY);
        let (stop_tx, stop_rx) = oneshot::channel::<StopReason>();
        let (interrupt_tx, interrupt_rx) =
            mpsc::channel::<TurnId>(crate::adapter::INTERRUPT_CHANNEL_CAPACITY);

        // Synthetic session id — Gemini's CLI offers --resume by index
        // for sessions persisted to disk, but we treat each turn as
        // independent at v0.1.
        let session_id = format!("gemini-{}", config.name);
        let _ = tx_events
            .send(CrepEvent::RoleStarted {
                role: config.name.clone(),
                engine: Engine::Gemini.as_str().to_owned(),
                model: config.model.clone().unwrap_or_else(|| "gemini".to_owned()),
                session_id,
                priors_hash,
            })
            .await;

        tokio::spawn(
            GeminiLoop {
                gemini_path: self.gemini_path.clone(),
                role: config.name.clone(),
                model: config.model.clone(),
                priors_path: config.priors_path.clone(),
                priors_text,
                prompt_mode,
                rx: rx_user,
                events: tx_events,
                stop_rx,
                interrupt_rx,
            }
            .run(),
        );

        Ok(RoleHandle::new(
            config.name,
            Engine::Gemini,
            tx_user,
            rx_events,
            stop_tx,
            interrupt_tx,
        ))
    }
}

async fn probe_gemini(gemini_path: &PathBuf) -> Result<GeminiPromptMode, String> {
    let mut cmd = Command::new(gemini_path);
    cmd.arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(GEMINI_PROBE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| "gemini --help timed out".to_owned())?
        .map_err(|error| format!("gemini probe failed: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "gemini --help exited with {}: {stderr}",
            output.status
        ));
    }
    let help = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !help.contains("stream-json") {
        return Err(
            "installed gemini CLI does not advertise --output-format stream-json; \
             tool-event replay requires Gemini CLI stream-json support"
                .to_owned(),
        );
    }
    if !help.contains("--system-instruction-file") {
        if std::env::var(GEMINI_UNTRUSTED_PRIORS_ENV).as_deref() == Ok("1") {
            return Ok(GeminiPromptMode::InlineUntrusted);
        }
        return Err(
            "installed gemini CLI does not advertise --system-instruction-file; \
             refusing to inline priors into user prompts. Set \
             CODEROOM_GEMINI_UNTRUSTED_PRIORS=1 to opt into the unsafe fallback"
                .to_owned(),
        );
    }
    Ok(GeminiPromptMode::SystemInstructionFile)
}

struct GeminiLoop {
    gemini_path: PathBuf,
    role: String,
    model: Option<String>,
    priors_path: PathBuf,
    priors_text: String,
    prompt_mode: GeminiPromptMode,
    events: mpsc::Sender<CrepEvent>,
    rx: mpsc::Receiver<UserMessage>,
    stop_rx: oneshot::Receiver<StopReason>,
    /// Turn-level cancellation. PR a wires the channel; the REPL does
    /// not yet send TurnIds through it (PR b is the consumer). When
    /// PR b lands, an arriving TurnId aborts the in-flight `gemini -p`
    /// child and the loop emits a `TurnInterrupted` event with the
    /// partial bytes captured by the streaming accumulator.
    interrupt_rx: mpsc::Receiver<TurnId>,
}

impl GeminiLoop {
    async fn run(mut self) {
        let stop_reason = loop {
            // Drain any cancel requests that arrived between turns
            // (e.g. a /halt fired after the last reply finished but
            // before the next dispatch) so they don't haunt the next
            // turn. We do not emit TurnInterrupted here — there is no
            // turn to interrupt.
            while self.interrupt_rx.try_recv().is_ok() {}

            let msg = tokio::select! {
                biased;
                requested = &mut self.stop_rx => break requested.unwrap_or(StopReason::Crashed),
                msg = self.rx.recv() => msg,
            };
            let Some(msg) = msg else {
                break StopReason::Completed;
            };
            let UserMessage::Prompt(prompt) = msg else {
                continue;
            };
            let outcome = run_one_turn(
                GeminiTurnRequest {
                    gemini_path: &self.gemini_path,
                    model: self.model.as_deref(),
                    priors_path: &self.priors_path,
                    priors_text: &self.priors_text,
                    prompt_mode: self.prompt_mode,
                    user_prompt: &prompt,
                    role: &self.role,
                },
                &mut self.stop_rx,
                &mut self.interrupt_rx,
            )
            .await;
            match outcome {
                GeminiTurnOutcome::Stopped(reason) => break reason,
                GeminiTurnOutcome::Interrupted { partial_text } => {
                    // Emit a turn-boundary event so the REPL's drain
                    // unblocks when the user halts. We don't yet know
                    // the dispatched turn_id (UserMessage will carry
                    // it once parallel dispatch lands), so tag with
                    // the legacy empty-string id; drain matches on
                    // role + variant, not on id.
                    let trimmed = partial_text.trim().to_owned();
                    let partial_mentions = if trimmed.is_empty() {
                        Vec::new()
                    } else {
                        crate::adapter::cc::parse_mentions(&trimmed)
                    };
                    let _ = self
                        .events
                        .send(CrepEvent::TurnInterrupted {
                            role: self.role.clone(),
                            turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                            thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                            source: crate::crep::InterruptSource::UserHalt,
                            partial_text: if trimmed.is_empty() {
                                None
                            } else {
                                Some(trimmed)
                            },
                            partial_mentions,
                        })
                        .await;
                }
                GeminiTurnOutcome::Completed(Ok(turn)) => {
                    for event in turn.events {
                        let _ = self.events.send(event).await;
                    }
                    for event in
                        crate::adapter::role_spoke_events_from_text(&self.role, &turn.text, 0.0, 0)
                    {
                        let _ = self.events.send(event).await;
                    }
                }
                GeminiTurnOutcome::Completed(Err(error)) => {
                    warn!(role = %self.role, %error, "gemini turn failed");
                    let _ = self
                        .events
                        .send(CrepEvent::RoleSpoke {
                            role: self.role.clone(),
                            text: format!("[gemini error: {error}]"),
                            mentions: Vec::new(),
                            cost_usd: 0.0,
                            cache_read: 0,
                            turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                            thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                        })
                        .await;
                }
            }
        };
        let _ = self
            .events
            .send(CrepEvent::RoleStopped {
                role: self.role,
                reason: stop_reason,
                turn_id: None,
            })
            .await;
        debug!("gemini per-turn loop exiting");
    }
}

enum GeminiTurnOutcome {
    Completed(std::io::Result<GeminiTurn>),
    /// Role-level stop requested via `stop_tx`. Loop exits.
    Stopped(StopReason),
    /// Turn-level halt requested via `interrupt_rx`. Loop continues
    /// for the next dispatch; `partial_text` is whatever the
    /// streaming accumulator captured before the child was killed.
    /// PR b will plumb this into `CrepEvent::TurnInterrupted`; PR a
    /// only proves the capture path works, so the field is held
    /// without being emitted yet.
    Interrupted {
        #[allow(
            dead_code,
            reason = "fed into CrepEvent::TurnInterrupted by PR b once UserMessage carries turn_id"
        )]
        partial_text: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct GeminiTurn {
    text: String,
    events: Vec<CrepEvent>,
}

struct GeminiTurnRequest<'a> {
    gemini_path: &'a PathBuf,
    model: Option<&'a str>,
    priors_path: &'a PathBuf,
    priors_text: &'a str,
    prompt_mode: GeminiPromptMode,
    user_prompt: &'a str,
    role: &'a str,
}

/// Run a single `gemini -p` invocation and translate its stream-json stdout.
///
/// Three exit paths:
///
/// - Child completes naturally → [`GeminiTurnOutcome::Completed`].
/// - `stop_rx` fires (role kill) → [`GeminiTurnOutcome::Stopped`].
/// - `interrupt_rx` fires (turn-only halt) → [`GeminiTurnOutcome::Interrupted`]
///   with whatever bytes the streaming accumulator captured. The role
///   stays alive for the next dispatch.
async fn run_one_turn(
    request: GeminiTurnRequest<'_>,
    stop_rx: &mut oneshot::Receiver<StopReason>,
    interrupt_rx: &mut mpsc::Receiver<TurnId>,
) -> GeminiTurnOutcome {
    let mut cmd = Command::new(request.gemini_path);
    cmd.arg("-p")
        .arg(match request.prompt_mode {
            GeminiPromptMode::SystemInstructionFile => request.user_prompt.to_owned(),
            GeminiPromptMode::InlineUntrusted => {
                format!("{}\n\n---\n\n{}", request.priors_text, request.user_prompt)
            }
        })
        .arg("--output-format")
        .arg("stream-json")
        .arg("-y")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    isolate_process_group(&mut cmd);
    if request.prompt_mode == GeminiPromptMode::SystemInstructionFile {
        cmd.arg("--system-instruction-file")
            .arg(request.priors_path);
    }
    if let Some(model) = request.model {
        cmd.arg("--model").arg(model);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => return GeminiTurnOutcome::Completed(Err(error)),
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    // Streaming accumulators. The reader tasks append every chunk into
    // these `Arc<Mutex<Vec<u8>>>` buffers. On natural completion the
    // reader returns the full bytes; on `stop_rx` / `interrupt_rx` we
    // abort the reader and drain the accumulator for whatever was
    // captured before the kill landed. v0.1's `read_pipe` discarded
    // partial bytes via `task::abort()` — that's why early gemini
    // cancellations produced empty WorkCards.
    let stdout_accum: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stderr_accum: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stdout_task = tokio::spawn(read_pipe_streaming(stdout, Arc::clone(&stdout_accum)));
    let stderr_task = tokio::spawn(read_pipe_streaming(stderr, Arc::clone(&stderr_accum)));

    let status = tokio::select! {
        biased;
        requested = stop_rx => {
            let reason = requested.unwrap_or(StopReason::Crashed);
            terminate_child(request.role, &mut child).await;
            stdout_task.abort();
            stderr_task.abort();
            return GeminiTurnOutcome::Stopped(reason);
        }
        Some(turn_id) = interrupt_rx.recv() => {
            debug!(role = %request.role, turn_id = %turn_id, "gemini turn cancellation requested");
            terminate_child(request.role, &mut child).await;
            stdout_task.abort();
            stderr_task.abort();
            let partial = stdout_accum.lock().await.clone();
            let partial_text = String::from_utf8_lossy(&partial).into_owned();
            return GeminiTurnOutcome::Interrupted { partial_text };
        }
        status = child.wait() => status,
    };
    let status = match status {
        Ok(status) => status,
        Err(error) => return GeminiTurnOutcome::Completed(Err(error)),
    };
    let stdout = stdout_task
        .await
        .map_err(|error| std::io::Error::other(format!("joining stdout reader: {error}")))
        .and_then(|result| result);
    let stderr = stderr_task
        .await
        .map_err(|error| std::io::Error::other(format!("joining stderr reader: {error}")))
        .and_then(|result| result);
    let (stdout, stderr) = match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) => (stdout, stderr),
        (Err(error), _) | (_, Err(error)) => return GeminiTurnOutcome::Completed(Err(error)),
    };
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return GeminiTurnOutcome::Completed(Err(std::io::Error::other(format!(
            "gemini exited with {status}: {stderr}",
        ))));
    }
    GeminiTurnOutcome::Completed(Ok(parse_stream_json_turn(
        request.role,
        &String::from_utf8_lossy(&stdout),
    )))
}

/// Read a pipe to EOF, appending each chunk into `accum` so partial
/// bytes survive a `task::abort()`. Returns the cumulative bytes on
/// natural completion; aborted callers drain `accum` directly.
async fn read_pipe_streaming(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    accum: Arc<Mutex<Vec<u8>>>,
) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; 4096];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => accum.lock().await.extend_from_slice(&buf[..n]),
            Err(error) => return Err(error),
        }
    }
    Ok(accum.lock().await.clone())
}

#[cfg(unix)]
fn isolate_process_group(cmd: &mut Command) {
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn isolate_process_group(_cmd: &mut Command) {}

async fn terminate_child(role: &str, child: &mut Child) {
    if !signal_process_group(role, child, SignalKind::Terminate) {
        if let Err(error) = child.start_kill() {
            warn!(role, %error, "failed to start gemini subprocess kill");
            return;
        }
    }
    match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => warn!(role, %error, "error waiting after gemini subprocess kill"),
        Err(_) => {
            warn!(
                role,
                "gemini subprocess did not exit promptly after kill signal"
            );
            signal_process_group(role, child, SignalKind::Kill);
            let _ = child.kill().await;
        }
    }
}

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
        warn!(role, pid, "gemini subprocess pid does not fit i32");
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
                "failed to signal gemini subprocess group"
            );
            false
        }
    }
}

#[cfg(not(unix))]
fn signal_process_group(_role: &str, _child: &Child, _signal: SignalKind) -> bool {
    false
}

fn parse_stream_json_turn(role: &str, stdout: &str) -> GeminiTurn {
    let mut text = String::new();
    let mut events = Vec::new();
    let mut fallback_lines = Vec::new();
    let mut synthetic_tool_id = 0usize;

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            fallback_lines.push(line.to_owned());
            continue;
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("message") => {
                if value.get("role").and_then(serde_json::Value::as_str) == Some("assistant") {
                    if let Some(content) = value.get("content").and_then(serde_json::Value::as_str)
                    {
                        text.push_str(content);
                    }
                }
            }
            Some("tool_use") => {
                synthetic_tool_id += 1;
                events.push(gemini_tool_use_to_event(role, &value, synthetic_tool_id));
            }
            Some("tool_result") => {
                synthetic_tool_id += 1;
                events.push(gemini_tool_result_to_event(role, &value, synthetic_tool_id));
            }
            _ => {}
        }
    }

    if text.is_empty() && !fallback_lines.is_empty() {
        text = fallback_lines.join("\n");
    }

    GeminiTurn {
        text: text.trim().to_owned(),
        events,
    }
}

fn gemini_tool_use_to_event(
    role: &str,
    value: &serde_json::Value,
    synthetic_tool_id: usize,
) -> CrepEvent {
    CrepEvent::ToolCallProposed {
        role: role.to_owned(),
        tool_name: value
            .get("tool_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        tool_input: value
            .get("parameters")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        tool_use_id: gemini_tool_id(value, synthetic_tool_id),
        turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
        thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
    }
}

fn gemini_tool_result_to_event(
    role: &str,
    value: &serde_json::Value,
    synthetic_tool_id: usize,
) -> CrepEvent {
    let ok = value
        .get("status")
        .and_then(serde_json::Value::as_str)
        .is_none_or(|status| status != "error");
    let summary = value
        .get("output")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or("gemini tool completed");
    CrepEvent::ToolCallExecuted {
        role: role.to_owned(),
        tool_use_id: gemini_tool_id(value, synthetic_tool_id),
        ok,
        output_summary: truncate(summary, TOOL_OUTPUT_SUMMARY_MAX_CHARS),
        turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
        thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
    }
}

fn gemini_tool_id(value: &serde_json::Value, synthetic_tool_id: usize) -> String {
    value
        .get("tool_id")
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            || format!("gemini-tool-{synthetic_tool_id}"),
            ToOwned::to_owned,
        )
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn engine_id() {
        let adapter = GeminiAdapter::new();
        assert_eq!(adapter.engine(), Engine::Gemini);
    }

    #[test]
    fn with_path_overrides_default() {
        let adapter = GeminiAdapter::with_path(PathBuf::from("/opt/gemini"));
        assert_eq!(adapter.gemini_path, PathBuf::from("/opt/gemini"));
    }

    #[test]
    fn stream_json_turn_accumulates_assistant_text_and_tool_events() {
        let stdout = r#"{"type":"init","timestamp":"2026-05-10T00:00:00Z","session_id":"s","model":"gemini"}
{"type":"message","timestamp":"2026-05-10T00:00:00Z","role":"user","content":"read"}
{"type":"tool_use","timestamp":"2026-05-10T00:00:00Z","tool_name":"Read","tool_id":"read-1","parameters":{"file_path":"README.md"}}
{"type":"tool_result","timestamp":"2026-05-10T00:00:01Z","tool_id":"read-1","status":"success","output":"hello"}
{"type":"message","timestamp":"2026-05-10T00:00:02Z","role":"assistant","content":"Done","delta":true}
{"type":"result","timestamp":"2026-05-10T00:00:03Z","status":"success","stats":{"tool_calls":1}}"#;

        let turn = parse_stream_json_turn("backend", stdout);

        assert_eq!(turn.text, "Done");
        assert_eq!(
            turn.events,
            vec![
                CrepEvent::ToolCallProposed {
                    role: "backend".into(),
                    tool_name: "Read".into(),
                    tool_input: serde_json::json!({"file_path": "README.md"}),
                    tool_use_id: "read-1".into(),
                    turn_id: String::new(),
                    thread_id: String::new(),
                },
                CrepEvent::ToolCallExecuted {
                    role: "backend".into(),
                    tool_use_id: "read-1".into(),
                    ok: true,
                    output_summary: "hello".into(),
                    turn_id: String::new(),
                    thread_id: String::new(),
                },
            ]
        );
    }

    #[test]
    fn stream_json_tool_result_uses_error_message() {
        let stdout = r#"{"type":"tool_result","timestamp":"2026-05-10T00:00:01Z","tool_id":"bad-1","status":"error","error":{"type":"X","message":"failed"}}
{"type":"message","timestamp":"2026-05-10T00:00:02Z","role":"assistant","content":"Nope","delta":true}"#;

        let turn = parse_stream_json_turn("backend", stdout);

        assert_eq!(
            turn.events,
            vec![CrepEvent::ToolCallExecuted {
                role: "backend".into(),
                tool_use_id: "bad-1".into(),
                ok: false,
                output_summary: "failed".into(),
                turn_id: String::new(),
                thread_id: String::new(),
            }]
        );
    }
}
