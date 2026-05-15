//! Gemini engine adapter.
//!
//! The implementation is intentionally one-shot per turn: for each user
//! message we spawn `gemini -p`, parse its `stream-json` stdout, stream
//! assistant text and tool lifecycle events, then finish with a final
//! [`CrepEvent::RoleSpoke`]. There is no long-lived subprocess; Gemini's
//! native session id is persisted and passed back through `--resume`.
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

use std::collections::VecDeque;
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

        let session_id = config.resume_session_id.clone().unwrap_or_default();
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
                resume_session_id: config.resume_session_id.clone(),
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
    resume_session_id: Option<String>,
    events: mpsc::Sender<CrepEvent>,
    rx: mpsc::Receiver<UserMessage>,
    stop_rx: oneshot::Receiver<StopReason>,
    /// Turn-level cancellation. An arriving TurnId aborts the
    /// in-flight `gemini -p` child and the loop emits a
    /// `TurnInterrupted` event with the partial bytes captured by the
    /// streaming accumulator.
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
            let turn_id = prompt.turn_id.clone();
            let thread_id = prompt.thread_id.clone();
            let outcome = run_one_turn(
                GeminiTurnRequest {
                    gemini_path: &self.gemini_path,
                    model: self.model.as_deref(),
                    priors_path: &self.priors_path,
                    priors_text: &self.priors_text,
                    prompt_mode: self.prompt_mode,
                    user_prompt: &prompt.text,
                    role: &self.role,
                    events: self.events.clone(),
                    resume_session_id: self.resume_session_id.as_deref(),
                    turn_id: &turn_id,
                    thread_id: &thread_id,
                },
                &mut self.stop_rx,
                &mut self.interrupt_rx,
            )
            .await;
            match outcome {
                GeminiTurnOutcome::Stopped(reason) => break reason,
                GeminiTurnOutcome::Interrupted {
                    partial_text,
                    session_id,
                } => {
                    self.record_session_id(session_id).await;
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
                            turn_id: turn_id.clone(),
                            thread_id: thread_id.clone(),
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
                    self.record_session_id(turn.session_id.clone()).await;
                    for event in crate::adapter::role_spoke_events_from_text_with_ids(
                        &self.role, &turn.text, 0.0, 0, &turn_id, &thread_id,
                    ) {
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
                            turn_id,
                            thread_id,
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

    async fn record_session_id(&mut self, session_id: Option<String>) {
        let Some(session_id) = session_id else {
            return;
        };
        if self.resume_session_id.as_deref() == Some(session_id.as_str()) {
            return;
        }
        self.resume_session_id = Some(session_id.clone());
        let _ = self
            .events
            .send(CrepEvent::RoleSessionUpdated {
                role: self.role.clone(),
                session_id,
            })
            .await;
    }
}

enum GeminiTurnOutcome {
    Completed(std::io::Result<GeminiTurn>),
    /// Role-level stop requested via `stop_tx`. Loop exits.
    Stopped(StopReason),
    /// Turn-level halt requested via `interrupt_rx`. Loop continues
    /// for the next dispatch; `partial_text` is whatever the
    /// streaming accumulator captured before the child was killed.
    Interrupted {
        partial_text: String,
        session_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct GeminiTurn {
    text: String,
    session_id: Option<String>,
}

struct GeminiTurnRequest<'a> {
    gemini_path: &'a PathBuf,
    model: Option<&'a str>,
    priors_path: &'a PathBuf,
    priors_text: &'a str,
    prompt_mode: GeminiPromptMode,
    user_prompt: &'a str,
    role: &'a str,
    events: mpsc::Sender<CrepEvent>,
    resume_session_id: Option<&'a str>,
    turn_id: &'a str,
    thread_id: &'a str,
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
    if let Some(session_id) = request.resume_session_id {
        cmd.arg("--resume").arg(session_id);
    }
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
    let assistant_text: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let session_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let stream_state = GeminiStreamState {
        assistant_text: Arc::clone(&assistant_text),
        session_id: Arc::clone(&session_id),
    };
    let stdout_task = tokio::spawn(read_stdout_streaming(
        request.role.to_owned(),
        stdout,
        Arc::clone(&stdout_accum),
        stream_state,
        request.events.clone(),
        GeminiTurnIds {
            turn_id: request.turn_id.to_owned(),
            thread_id: request.thread_id.to_owned(),
        },
    ));
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
            let parsed_partial = assistant_text.lock().await.clone();
            let partial_text = if parsed_partial.trim().is_empty() {
                String::from_utf8_lossy(&partial).into_owned()
            } else {
                parsed_partial
            };
            let session_id = session_id.lock().await.clone();
            return GeminiTurnOutcome::Interrupted {
                partial_text,
                session_id,
            };
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
    let (turn, stderr) = match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) => (stdout, stderr),
        (Err(error), _) | (_, Err(error)) => return GeminiTurnOutcome::Completed(Err(error)),
    };
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return GeminiTurnOutcome::Completed(Err(std::io::Error::other(format!(
            "gemini exited with {status}: {stderr}",
        ))));
    }
    GeminiTurnOutcome::Completed(Ok(turn))
}

async fn read_stdout_streaming(
    role: String,
    mut stdout: impl tokio::io::AsyncRead + Unpin,
    raw_accum: Arc<Mutex<Vec<u8>>>,
    stream_state: GeminiStreamState,
    events: mpsc::Sender<CrepEvent>,
    turn_ids: GeminiTurnIds,
) -> std::io::Result<GeminiTurn> {
    let mut parse_state = GeminiParseState::default();
    let mut pending = Vec::<u8>::new();
    let mut buf = vec![0u8; 4096];
    loop {
        let n = stdout.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        raw_accum.lock().await.extend_from_slice(&buf[..n]);
        pending.extend_from_slice(&buf[..n]);
        while let Some(pos) = pending.iter().position(|byte| *byte == b'\n') {
            let mut line = pending.drain(..=pos).collect::<Vec<_>>();
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            process_gemini_stream_line(
                &role,
                &line,
                &stream_state,
                &events,
                &mut parse_state,
                &turn_ids,
            )
            .await;
        }
    }
    if !pending.is_empty() {
        process_gemini_stream_line(
            &role,
            &pending,
            &stream_state,
            &events,
            &mut parse_state,
            &turn_ids,
        )
        .await;
    }
    let mut text = stream_state.assistant_text.lock().await.clone();
    if text.is_empty() && !parse_state.fallback_lines.is_empty() {
        text = parse_state.fallback_lines.join("\n");
    }
    Ok(GeminiTurn {
        text: text.trim().to_owned(),
        session_id: stream_state.session_id.lock().await.clone(),
    })
}

#[derive(Clone, Default)]
struct GeminiStreamState {
    assistant_text: Arc<Mutex<String>>,
    session_id: Arc<Mutex<Option<String>>>,
}

#[derive(Debug, Clone)]
struct GeminiTurnIds {
    turn_id: String,
    thread_id: String,
}

#[derive(Default)]
struct GeminiParseState {
    fallback_lines: Vec<String>,
    tool_ids: GeminiSyntheticToolIds,
    delta_sequence: u64,
}

async fn process_gemini_stream_line(
    role: &str,
    line: &[u8],
    stream_state: &GeminiStreamState,
    events: &mpsc::Sender<CrepEvent>,
    parse_state: &mut GeminiParseState,
    turn_ids: &GeminiTurnIds,
) {
    let line = String::from_utf8_lossy(line);
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        parse_state.fallback_lines.push(line.to_owned());
        return;
    };
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("init") => {
            if let Some(id) = gemini_session_id(&value) {
                *stream_state.session_id.lock().await = Some(id);
            }
        }
        Some("message")
            if value.get("role").and_then(serde_json::Value::as_str) == Some("assistant") =>
        {
            if let Some(content) = value.get("content").and_then(serde_json::Value::as_str) {
                stream_state.assistant_text.lock().await.push_str(content);
                parse_state.delta_sequence = value
                    .get("sequence")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_else(|| parse_state.delta_sequence.saturating_add(1));
                let _ = events.try_send(CrepEvent::RoleOutputDelta {
                    role: role.to_owned(),
                    text_delta: content.to_owned(),
                    sequence: parse_state.delta_sequence,
                    turn_id: turn_ids.turn_id.clone(),
                    thread_id: turn_ids.thread_id.clone(),
                });
            }
        }
        Some("tool_use") => {
            let tool_use_id = parse_state.tool_ids.for_use(&value);
            let _ = events
                .send(gemini_tool_use_to_event(
                    role,
                    &value,
                    tool_use_id,
                    &turn_ids.turn_id,
                    &turn_ids.thread_id,
                ))
                .await;
        }
        Some("tool_result") => {
            let tool_use_id = parse_state.tool_ids.for_result(&value);
            let _ = events
                .send(gemini_tool_result_to_event(
                    role,
                    &value,
                    tool_use_id,
                    &turn_ids.turn_id,
                    &turn_ids.thread_id,
                ))
                .await;
        }
        _ => {}
    }
}

fn gemini_session_id(value: &serde_json::Value) -> Option<String> {
    value
        .get("session_id")
        .or_else(|| value.get("sessionId"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
}

#[derive(Default)]
struct GeminiSyntheticToolIds {
    next: usize,
    pending: VecDeque<String>,
}

impl GeminiSyntheticToolIds {
    fn for_use(&mut self, value: &serde_json::Value) -> String {
        if let Some(id) = native_gemini_tool_id(value) {
            return id;
        }
        self.next = self.next.saturating_add(1);
        let id = format!("gemini-tool-{}", self.next);
        self.pending.push_back(id.clone());
        id
    }

    fn for_result(&mut self, value: &serde_json::Value) -> String {
        native_gemini_tool_id(value).unwrap_or_else(|| {
            self.pending.pop_front().unwrap_or_else(|| {
                self.next = self.next.saturating_add(1);
                format!("gemini-tool-{}", self.next)
            })
        })
    }
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

#[cfg(test)]
fn parse_stream_json_turn(_role: &str, stdout: &str) -> GeminiTurn {
    let mut text = String::new();
    let mut session_id = None;
    let mut fallback_lines = Vec::new();

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
            Some("init") => {
                if let Some(id) = gemini_session_id(&value) {
                    session_id = Some(id);
                }
            }
            Some("message")
                if value.get("role").and_then(serde_json::Value::as_str) == Some("assistant") =>
            {
                if let Some(content) = value.get("content").and_then(serde_json::Value::as_str) {
                    text.push_str(content);
                }
            }
            _ => {}
        }
    }

    if text.is_empty() && !fallback_lines.is_empty() {
        text = fallback_lines.join("\n");
    }

    GeminiTurn {
        text: text.trim().to_owned(),
        session_id,
    }
}

fn gemini_tool_use_to_event(
    role: &str,
    value: &serde_json::Value,
    tool_use_id: String,
    turn_id: &str,
    thread_id: &str,
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
        tool_use_id,
        turn_id: turn_id.to_owned(),
        thread_id: thread_id.to_owned(),
    }
}

fn gemini_tool_result_to_event(
    role: &str,
    value: &serde_json::Value,
    tool_use_id: String,
    turn_id: &str,
    thread_id: &str,
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
        tool_use_id,
        ok,
        output_summary: truncate(summary, TOOL_OUTPUT_SUMMARY_MAX_CHARS),
        turn_id: turn_id.to_owned(),
        thread_id: thread_id.to_owned(),
    }
}

fn native_gemini_tool_id(value: &serde_json::Value) -> Option<String> {
    value
        .get("tool_id")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
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
    fn stream_json_turn_accumulates_assistant_text() {
        let stdout = r#"{"type":"init","timestamp":"2026-05-10T00:00:00Z","session_id":"s","model":"gemini"}
{"type":"message","timestamp":"2026-05-10T00:00:00Z","role":"user","content":"read"}
{"type":"tool_use","timestamp":"2026-05-10T00:00:00Z","tool_name":"Read","tool_id":"read-1","parameters":{"file_path":"README.md"}}
{"type":"tool_result","timestamp":"2026-05-10T00:00:01Z","tool_id":"read-1","status":"success","output":"hello"}
{"type":"message","timestamp":"2026-05-10T00:00:02Z","role":"assistant","content":"Done","delta":true}
{"type":"result","timestamp":"2026-05-10T00:00:03Z","status":"success","stats":{"tool_calls":1}}"#;

        let turn = parse_stream_json_turn("backend", stdout);

        assert_eq!(turn.text, "Done");
        assert_eq!(turn.session_id.as_deref(), Some("s"));
    }

    #[tokio::test]
    async fn stdout_streaming_emits_assistant_delta_and_tool_events() {
        let stdout = r#"{"type":"tool_use","timestamp":"2026-05-10T00:00:00Z","tool_name":"Read","tool_id":"read-1","parameters":{"file_path":"README.md"}}
{"type":"tool_result","timestamp":"2026-05-10T00:00:01Z","tool_id":"read-1","status":"success","output":"hello"}
{"type":"message","timestamp":"2026-05-10T00:00:02Z","role":"assistant","content":"Done","delta":true}"#;
        let (tx, mut rx) = mpsc::channel(8);
        let turn = read_stdout_streaming(
            "backend".to_owned(),
            stdout.as_bytes(),
            Arc::new(Mutex::new(Vec::new())),
            GeminiStreamState::default(),
            tx,
            GeminiTurnIds {
                turn_id: "tu-1".to_owned(),
                thread_id: "th-1".to_owned(),
            },
        )
        .await
        .unwrap();

        assert_eq!(turn.text, "Done");
        assert_eq!(turn.session_id, None);
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert_eq!(
            events,
            vec![
                CrepEvent::ToolCallProposed {
                    role: "backend".into(),
                    tool_name: "Read".into(),
                    tool_input: serde_json::json!({"file_path": "README.md"}),
                    tool_use_id: "read-1".into(),
                    turn_id: "tu-1".into(),
                    thread_id: "th-1".into(),
                },
                CrepEvent::ToolCallExecuted {
                    role: "backend".into(),
                    tool_use_id: "read-1".into(),
                    ok: true,
                    output_summary: "hello".into(),
                    turn_id: "tu-1".into(),
                    thread_id: "th-1".into(),
                },
                CrepEvent::RoleOutputDelta {
                    role: "backend".into(),
                    text_delta: "Done".into(),
                    sequence: 1,
                    turn_id: "tu-1".into(),
                    thread_id: "th-1".into(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn stdout_streaming_pairs_missing_tool_ids() {
        let stdout = r#"{"type":"tool_use","tool_name":"Read","parameters":{"file_path":"README.md"}}
{"type":"tool_result","status":"success","output":"hello"}"#;
        let (tx, mut rx) = mpsc::channel(8);
        let turn = read_stdout_streaming(
            "backend".to_owned(),
            stdout.as_bytes(),
            Arc::new(Mutex::new(Vec::new())),
            GeminiStreamState::default(),
            tx,
            GeminiTurnIds {
                turn_id: "tu-1".to_owned(),
                thread_id: "th-1".to_owned(),
            },
        )
        .await
        .unwrap();

        assert!(turn.text.is_empty());
        let proposed = rx.try_recv().expect("tool use");
        let executed = rx.try_recv().expect("tool result");
        match (proposed, executed) {
            (
                CrepEvent::ToolCallProposed {
                    tool_use_id: proposed,
                    ..
                },
                CrepEvent::ToolCallExecuted {
                    tool_use_id: executed,
                    ..
                },
            ) => assert_eq!(proposed, executed),
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[tokio::test]
    async fn stdout_streaming_tool_result_uses_error_message() {
        let stdout = r#"{"type":"tool_result","timestamp":"2026-05-10T00:00:01Z","tool_id":"bad-1","status":"error","error":{"type":"X","message":"failed"}}
{"type":"message","timestamp":"2026-05-10T00:00:02Z","role":"assistant","content":"Nope","delta":true}"#;
        let (tx, mut rx) = mpsc::channel(8);
        let turn = read_stdout_streaming(
            "backend".to_owned(),
            stdout.as_bytes(),
            Arc::new(Mutex::new(Vec::new())),
            GeminiStreamState::default(),
            tx,
            GeminiTurnIds {
                turn_id: "tu-1".to_owned(),
                thread_id: "th-1".to_owned(),
            },
        )
        .await
        .unwrap();

        assert_eq!(turn.text, "Nope");
        match rx.try_recv().expect("tool result event") {
            CrepEvent::ToolCallExecuted {
                tool_use_id,
                ok,
                output_summary,
                ..
            } => {
                assert_eq!(tool_use_id, "bad-1");
                assert!(!ok);
                assert_eq!(output_summary, "failed");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn stdout_streaming_captures_init_session_id() {
        let stdout = r#"{"type":"init","session_id":"gemini-session-uuid","model":"gemini"}
{"type":"message","role":"assistant","content":"Hi"}"#;
        let (tx, _rx) = mpsc::channel(1);
        let turn = read_stdout_streaming(
            "backend".to_owned(),
            stdout.as_bytes(),
            Arc::new(Mutex::new(Vec::new())),
            GeminiStreamState::default(),
            tx,
            GeminiTurnIds {
                turn_id: "tu-1".to_owned(),
                thread_id: "th-1".to_owned(),
            },
        )
        .await
        .unwrap();

        assert_eq!(turn.text, "Hi");
        assert_eq!(turn.session_id.as_deref(), Some("gemini-session-uuid"));
    }
}
