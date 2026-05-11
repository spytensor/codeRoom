//! Claude Code engine adapter.
//!
//! Spawns the `claude` CLI in non-interactive stream-JSON mode, parses each
//! line of its stdout as a stream-JSON event, and re-emits a normalized
//! [`CrepEvent`] stream over an mpsc channel. User prompts and tool
//! decisions arrive on a separate mpsc channel and are written to the
//! subprocess's stdin as stream-JSON user messages.
//!
//! Spawn flags (verified by `docs/spike-2026-05-09.md`):
//!
//! ```text
//! claude --print --input-format=stream-json --output-format=stream-json \
//!        --include-hook-events --verbose --dangerously-skip-permissions \
//!        --max-budget-usd=<cap> --append-system-prompt-file=<priors> \
//!        [--model=<model>]
//! ```
//!
//! v0.1 scope:
//!
//! - emit `RoleStarted` from the engine's `system.subtype="init"` event
//! - emit `RoleSpoke` from `result` events (uses `result`, `total_cost_usd`,
//!   `usage.cache_read_input_tokens`)
//! - emit `ToolCallProposed` from `assistant.message.content[*].type="tool_use"`
//! - emit `ToolCallExecuted` from `user.message.content[*].type="tool_result"`
//! - emit `RoleStopped` on subprocess exit
//!
//! Out of v0.2 scope (deferred to follow-up PRs): routing dynamic
//! `UserMessage::ToolDecision` values back into an already-running Claude
//! native approval prompt, and parsing `terminal_reason` to detect
//! budget-cap exits separately from crashes.

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::adapter::{
    AdapterError, AdapterResult, Engine, EngineAdapter, PermissionMode, RoleConfig, RoleHandle,
    UserMessage,
};
use crate::crep::{CrepEvent, StopReason};
use crate::turn::TurnId;

/// Channel capacity for both the user-message inbound queue and the CREP
/// event outbound queue. Sized for typical interactive usage; can be
/// revisited if back-pressure becomes a real problem.
const CHANNEL_CAPACITY: usize = 64;

/// Adapter that drives the Claude Code CLI.
#[derive(Debug, Clone)]
pub struct CcAdapter {
    /// Path to the `claude` binary. Defaults to bare `"claude"` (PATH lookup);
    /// override for tests or non-standard installs.
    claude_path: PathBuf,
}

impl Default for CcAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CcAdapter {
    /// Construct an adapter that resolves `claude` via the user's `PATH`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            claude_path: PathBuf::from("claude"),
        }
    }

    /// Construct an adapter pointing at a specific `claude` binary (used
    /// by tests that want to substitute a fake or shim).
    #[must_use]
    pub fn with_path(claude_path: PathBuf) -> Self {
        Self { claude_path }
    }
}

impl EngineAdapter for CcAdapter {
    fn engine(&self) -> Engine {
        Engine::Cc
    }

    async fn start(&self, config: RoleConfig) -> AdapterResult<RoleHandle> {
        // Read priors so we can fingerprint them without hitting disk again
        // from the read/write tasks. The contents themselves are passed to
        // the engine via --append-system-prompt-file, so we don't keep the
        // string around past hash computation.
        let priors_text = tokio::fs::read_to_string(&config.priors_path)
            .await
            .map_err(|source| AdapterError::PriorsRead {
                path: config.priors_path.clone(),
                source,
            })?;
        let priors_hash = fingerprint(&priors_text);
        drop(priors_text);

        let mut tempfiles = Vec::new();
        let mut cmd = Command::new(&self.claude_path);
        cmd.arg("--print")
            .arg("--input-format=stream-json")
            .arg("--output-format=stream-json")
            .arg("--include-hook-events")
            .arg("--verbose")
            .arg("--dangerously-skip-permissions")
            .arg(format!("--max-budget-usd={}", config.budget_usd))
            .arg(format!(
                "--append-system-prompt-file={}",
                config.priors_path.display()
            ));
        // Per amendment A-006: if the REPL handed us a session id
        // saved by a previous `cr start`, ask cc to resume that
        // conversation instead of opening a fresh one. cc tracks
        // sessions under `~/.claude/projects/<hash>/sessions/`; a
        // stale id (session deleted, project moved) makes cc fail to
        // start the subprocess, which surfaces as a normal Spawn
        // error and the user can recover with `cr start --fresh`.
        if let Some(resume_id) = &config.resume_session_id {
            if !resume_id.trim().is_empty() {
                cmd.arg("--resume").arg(resume_id);
            }
        }
        if config.permission_mode != PermissionMode::Bypass {
            let settings = claude_hook_settings(
                config.permission_mode,
                config.permission_policy_path.as_deref(),
            )?;
            cmd.arg("--settings").arg(settings.path());
            tempfiles.push(settings);
            // Tell the hook subprocess where to ask the user. Without
            // this, an `ask` verdict has no UI to surface it through and
            // the hook degrades to deny.
            if let Some(socket) = &config.permission_socket_path {
                cmd.env(crate::permissions::BRIDGE_ENV_VAR, socket);
                cmd.env(crate::permissions::BRIDGE_ROLE_ENV, &config.name);
            }
        }
        if let Some(model) = &config.model {
            cmd.arg(format!("--model={model}"));
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|source| AdapterError::Spawn {
            engine: Engine::Cc.as_str(),
            source,
        })?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (tx_user, rx_user) = mpsc::channel::<UserMessage>(CHANNEL_CAPACITY);
        let (tx_events, rx_events) = mpsc::channel::<CrepEvent>(CHANNEL_CAPACITY);
        let (turn_done_tx, turn_done_rx) = mpsc::channel::<()>(CHANNEL_CAPACITY);
        let (stop_tx, stop_rx) = oneshot::channel::<StopReason>();
        let (interrupt_tx, interrupt_rx) =
            mpsc::channel::<TurnId>(crate::adapter::INTERRUPT_CHANNEL_CAPACITY);
        // Internal stop: emit_cc_interrupt_events fires this so
        // `wait_child` can terminate the subprocess on user halt. See
        // `docs/v0.2-trust-and-interrupt.md` § F.1: until the
        // stream-json `{"type":"interrupt"}` verb is verified by
        // `spike/L4-cc-interrupt.sh`, cc's cancel path is "kill +
        // user `/refresh` to wake," which guarantees the halted
        // prompt's late `RoleSpoke` cannot pollute the next turn.
        let (internal_stop_tx, internal_stop_rx) = mpsc::channel::<StopReason>(1);

        // Stdout reader: parse stream-json → CREP.
        tokio::spawn(read_stdout(
            config.name.clone(),
            priors_hash,
            stdout,
            tx_events.clone(),
            turn_done_tx,
        ));
        tokio::spawn(drain_stderr(config.name.clone(), stderr));

        // Stdin writer: serialize user messages onto the engine's stdin,
        // pacing prompts until the prior turn has produced a RoleSpoke or
        // RoleStopped boundary.
        tokio::spawn(write_stdin(
            config.name.clone(),
            rx_user,
            stdin,
            turn_done_rx,
        ));

        // Turn-interrupt drain (cc's spec § F.1 fallback): emit a
        // `TurnInterrupted` event so the REPL's drain unblocks
        // immediately, then trigger the internal-stop channel so
        // `wait_child` kills the subprocess. The user types
        // `/refresh @<role>` to bring the role back. Earlier "keep
        // subprocess alive" design caused the halted prompt's late
        // `RoleSpoke` to bleed into the next turn's drain — this
        // pathway sidesteps that race entirely.
        tokio::spawn(emit_cc_interrupt_events(
            config.name.clone(),
            interrupt_rx,
            tx_events.clone(),
            internal_stop_tx,
        ));

        // Process waiter: emit a final RoleStopped when the subprocess exits.
        tokio::spawn(wait_child(
            config.name.clone(),
            child,
            tx_events,
            stop_rx,
            internal_stop_rx,
        ));

        Ok(RoleHandle::new_with_tempfiles(
            config.name,
            Engine::Cc,
            tx_user,
            rx_events,
            stop_tx,
            interrupt_tx,
            tempfiles,
        ))
    }
}

async fn emit_cc_interrupt_events(
    role: String,
    mut rx: mpsc::Receiver<TurnId>,
    events: mpsc::Sender<CrepEvent>,
    internal_stop: mpsc::Sender<StopReason>,
) {
    // Wait for the first /halt. There is at most one meaningful halt
    // in this adapter's lifetime — once it lands we kill the cc
    // subprocess and the role goes away.
    let Some(turn_id) = rx.recv().await else {
        return;
    };
    tracing::debug!(
        role = %role,
        turn_id = %turn_id,
        "cc /halt: emit TurnInterrupted then kill subprocess (spec § F.1 fallback)"
    );
    let _ = events
        .send(CrepEvent::TurnInterrupted {
            role: role.clone(),
            turn_id: if turn_id.is_empty() {
                crate::turn::LEGACY_TURN_ID.to_owned()
            } else {
                turn_id
            },
            thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
            source: crate::crep::InterruptSource::UserHalt,
            partial_text: None,
            partial_mentions: Vec::new(),
        })
        .await;
    // Kill the subprocess so the halted prompt's eventual RoleSpoke
    // cannot leak into a future turn. Idempotent — `try_send`
    // swallows the error if `wait_child` has already taken its
    // single-slot channel.
    let _ = internal_stop.try_send(StopReason::Crashed);
    // Drain any further /halt requests so the channel doesn't fill
    // and back-pressure the REPL. The role is dying; nothing to
    // interrupt anymore.
    while rx.recv().await.is_some() {}
}

fn claude_hook_settings(
    mode: PermissionMode,
    policy_path: Option<&Path>,
) -> AdapterResult<tempfile::NamedTempFile> {
    let current_exe = std::env::current_exe().map_err(|source| AdapterError::Engine {
        engine: Engine::Cc.as_str(),
        message: format!("locating current cr binary for permission hook: {source}"),
    })?;
    let mut command = format!(
        "{} __coderoom-hook-decision --mode {}",
        shell_quote(&current_exe),
        mode.as_str()
    );
    if let Some(path) = policy_path {
        command.push_str(" --policy-file ");
        command.push_str(&shell_quote(path));
    }
    let settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "*",
                    "hooks": [
                        {"type": "command", "command": command}
                    ]
                }
            ]
        }
    });
    let mut file = tempfile::Builder::new()
        .prefix("coderoom-cc-hooks-")
        .suffix(".json")
        .tempfile()
        .map_err(|source| AdapterError::Engine {
            engine: Engine::Cc.as_str(),
            message: format!("creating hook settings tempfile: {source}"),
        })?;
    write!(file, "{settings}").map_err(|source| AdapterError::Engine {
        engine: Engine::Cc.as_str(),
        message: format!("writing hook settings tempfile: {source}"),
    })?;
    file.flush().map_err(|source| AdapterError::Engine {
        engine: Engine::Cc.as_str(),
        message: format!("flushing hook settings tempfile: {source}"),
    })?;
    Ok(file)
}

fn shell_quote(path: &Path) -> String {
    let raw = path.display().to_string();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

/// Cheap, non-cryptographic content fingerprint. Stable for the same
/// content across runs (within a single Rust release; `DefaultHasher` is
/// allowed to change algorithms across releases). Sufficient for drift
/// detection in v0.1 — replace with `sha2` if/when we publish hashes.
///
/// `pub(crate)` so sibling adapters (codex, gemini) can reuse the same
/// fingerprint format and produce comparable `priors_hash` values.
pub(crate) fn fingerprint(content: &str) -> String {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("dh1:{:016x}", hasher.finish())
}

/// Parse `@<name>` references out of a reply, deduplicated, in order of
/// first appearance. Names use the same character set as Rust identifiers
/// minus underscores at the start (mirrors Slack conventions).
///
/// `pub(crate)` so other adapters (codex, gemini) can populate
/// `RoleSpoke.mentions` with the same parsing semantics CC uses.
pub(crate) fn parse_mentions(text: &str) -> Vec<String> {
    static MENTION_RE: OnceLock<Regex> = OnceLock::new();
    let re = MENTION_RE.get_or_init(|| {
        Regex::new(r"@([A-Za-z][A-Za-z0-9_-]*)").expect("compile-time-valid regex")
    });
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for cap in re.captures_iter(text) {
        let Some(full) = cap.get(0) else {
            continue;
        };
        if !is_mention_boundary(text, full.start()) {
            continue;
        }
        let name = cap[1].to_string();
        if seen.insert(name.clone()) {
            out.push(name);
        }
    }
    out
}

fn is_mention_boundary(text: &str, at_idx: usize) -> bool {
    if at_idx == 0 {
        return true;
    }
    let Some(prev) = text[..at_idx].chars().next_back() else {
        return true;
    };
    !(prev.is_ascii_alphanumeric() || matches!(prev, '_' | '-' | '.'))
}

/// Translate one parsed stream-JSON line into zero or more CREP events.
///
/// Pure function: no I/O, no async. Easy to unit-test against canned
/// stream-json samples.
fn translate(role: &str, priors_hash: &str, line: &Value) -> Vec<CrepEvent> {
    let Some(t) = line.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };

    match t {
        "system" if line.get("subtype").and_then(Value::as_str) == Some("init") => {
            let session_id = line
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let model = line
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            vec![CrepEvent::RoleStarted {
                role: role.to_owned(),
                engine: Engine::Cc.as_str().to_owned(),
                model,
                session_id,
                priors_hash: priors_hash.to_owned(),
            }]
        }

        "result" => {
            let text = line
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let cost_usd = line
                .get("total_cost_usd")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let cache_read = line
                .get("usage")
                .and_then(|u| u.get("cache_read_input_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let mut events = extract_permission_denials(role, line);
            events.extend(crate::adapter::role_spoke_events_from_text(
                role, &text, cost_usd, cache_read,
            ));
            events
        }

        "assistant" => extract_assistant_events(role, line),

        "user" => extract_tool_results(role, line),

        // Other event types (rate_limit_event, hook events, etc.) are
        // intentionally ignored at v0.1; they show up in the transcript
        // archive but don't have CREP equivalents yet.
        _ => Vec::new(),
    }
}

fn extract_permission_denials(role: &str, line: &Value) -> Vec<CrepEvent> {
    let Some(denials) = line.get("permission_denials").and_then(Value::as_array) else {
        return Vec::new();
    };
    denials
        .iter()
        .map(|denial| {
            let tool_name = denial
                .get("tool_name")
                .or_else(|| denial.get("toolName"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            let tool_input = denial
                .get("tool_input")
                .or_else(|| denial.get("toolInput"))
                .cloned()
                .unwrap_or(Value::Null);
            let reason = denial
                .get("reason")
                .or_else(|| denial.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("denied by CodeRoom permission hook")
                .to_owned();
            CrepEvent::PermissionDenied {
                role: role.to_owned(),
                tool_name,
                tool_input,
                reason,
                turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
            }
        })
        .collect()
}

fn extract_assistant_events(role: &str, line: &Value) -> Vec<CrepEvent> {
    let Some(content) = line
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    let mut events = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if let Some(title) = crate::work::extract_cr_task(text).title {
                        events.push(CrepEvent::WorkTitle {
                            role: role.to_owned(),
                            title,
                            turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                            thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                        });
                    }
                }
            }
            Some("tool_use") => events.push(CrepEvent::ToolCallProposed {
                role: role.to_owned(),
                tool_name: block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                tool_input: block.get("input").cloned().unwrap_or(Value::Null),
                tool_use_id: block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
            }),
            _ => {}
        }
    }
    events
}

fn extract_tool_results(role: &str, line: &Value) -> Vec<CrepEvent> {
    let Some(content) = line
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .map(|block| {
            let output_summary = block
                .get("content")
                .and_then(|c| {
                    // tool_result.content can be a string or array of blocks.
                    c.as_str()
                        .map(ToOwned::to_owned)
                        .or_else(|| c.as_array().map(|_| "[structured output]".to_owned()))
                })
                .unwrap_or_default();
            let ok = !block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            CrepEvent::ToolCallExecuted {
                role: role.to_owned(),
                tool_use_id: block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                ok,
                output_summary: truncate(&output_summary, 200),
                turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
            }
        })
        .collect()
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

async fn read_stdout(
    role: String,
    priors_hash: String,
    stdout: ChildStdout,
    events: mpsc::Sender<CrepEvent>,
    turn_done: mpsc::Sender<()>,
) {
    let mut lines = BufReader::new(stdout).lines();
    // WorkTitle dedup set, keyed on turn_id. v0.1 events with no
    // turn_id share the empty-string key so the legacy "first title
    // per turn" semantics still hold; v0.2 turns get individual keys
    // so PR b's pipelined cc turns don't lose the second turn's title
    // to the first turn's dedup bool. See `docs/v0.2-trust-and-interrupt.md`
    // § F.4.
    let mut seen_titles_by_turn: HashSet<TurnId> = HashSet::new();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let value: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(error) => {
                        warn!(role, %error, line = %line, "non-JSON line on stdout");
                        continue;
                    }
                };
                for event in translate(&role, &priors_hash, &value) {
                    let Some(event) = dedupe_work_title_for_turn(event, &mut seen_titles_by_turn)
                    else {
                        continue;
                    };
                    // Pull the turn_id off the boundary event before we
                    // hand ownership to `events.send`, so we can clear
                    // the dedup set after the send. RoleStopped without
                    // a turn_id (legacy logs / between-turn clean stop)
                    // collapses to the empty-string key that legacy
                    // WorkTitles use, so the set drains correctly in
                    // both v0.1 and v0.2 wire shapes.
                    let boundary_turn_id: Option<TurnId> = match &event {
                        CrepEvent::RoleSpoke { turn_id, .. } => Some(turn_id.clone()),
                        CrepEvent::RoleStopped { turn_id, .. } => {
                            Some(turn_id.clone().unwrap_or_default())
                        }
                        _ => None,
                    };
                    if events.send(event).await.is_err() {
                        debug!(role, "event receiver dropped; stopping reader");
                        return;
                    }
                    if let Some(id) = boundary_turn_id {
                        seen_titles_by_turn.remove(&id);
                        let _ = turn_done.send(()).await;
                    }
                }
            }
            Ok(None) => {
                debug!(role, "stdout EOF");
                return;
            }
            Err(error) => {
                warn!(role, %error, "error reading stdout");
                return;
            }
        }
    }
}

fn dedupe_work_title_for_turn(
    event: CrepEvent,
    seen_titles_by_turn: &mut HashSet<TurnId>,
) -> Option<CrepEvent> {
    if let CrepEvent::WorkTitle { turn_id, .. } = &event {
        // `insert` returns true if the key was newly added → first
        // WorkTitle for this turn, let it through. False on duplicate.
        if !seen_titles_by_turn.insert(turn_id.clone()) {
            return None;
        }
    }
    Some(event)
}

async fn drain_stderr(role: String, stderr: ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => {}
            Ok(Some(line)) => debug!(role, line = %line, "claude stderr"),
            Ok(None) => return,
            Err(error) => {
                warn!(role, %error, "error reading claude stderr");
                return;
            }
        }
    }
}

async fn write_stdin<W>(
    role: String,
    mut rx: mpsc::Receiver<UserMessage>,
    mut stdin: W,
    mut turn_done: mpsc::Receiver<()>,
) where
    W: AsyncWrite + Unpin,
{
    while let Some(msg) = rx.recv().await {
        match msg {
            UserMessage::Prompt(text) => {
                let envelope = serde_json::json!({
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": text}],
                    },
                });
                let line = format!("{envelope}\n");
                if let Err(error) = stdin.write_all(line.as_bytes()).await {
                    warn!(role, %error, "failed to write prompt to stdin; closing");
                    return;
                }
                if let Err(error) = stdin.flush().await {
                    warn!(role, %error, "failed to flush stdin");
                    return;
                }
                if turn_done.recv().await.is_none() {
                    debug!(role, "turn boundary channel closed; stopping stdin writer");
                    return;
                }
            }
            UserMessage::ToolDecision { tool_use_id, .. } => {
                // Hook decisions are made synchronously by the configured
                // PreToolUse command. This async message remains reserved
                // for a future native prompt bridge.
                warn!(
                    role,
                    tool_use_id, "ToolDecision delivered but no live prompt bridge is attached"
                );
            }
        }
    }
    debug!(role, "user-message channel closed; closing stdin");
}

async fn wait_child(
    role: String,
    mut child: Child,
    events: mpsc::Sender<CrepEvent>,
    stop_rx: oneshot::Receiver<StopReason>,
    mut internal_stop_rx: mpsc::Receiver<StopReason>,
) {
    let reason = tokio::select! {
        status = child.wait() => match status {
            Ok(status) if status.success() => StopReason::Completed,
            Ok(_) => StopReason::Crashed,
            Err(error) => {
                warn!(role, %error, "error waiting on subprocess");
                StopReason::Crashed
            }
        },
        requested = stop_rx => {
            let reason = requested.unwrap_or(StopReason::Crashed);
            terminate_child(&role, &mut child).await;
            reason
        }
        Some(reason) = internal_stop_rx.recv() => {
            terminate_child(&role, &mut child).await;
            reason
        }
    };
    let _ = events
        .send(CrepEvent::RoleStopped {
            role,
            reason,
            turn_id: None,
        })
        .await;
}

async fn terminate_child(role: &str, child: &mut Child) {
    if let Err(error) = child.start_kill() {
        warn!(role, %error, "failed to start subprocess kill");
        return;
    }
    match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => warn!(role, %error, "error waiting after subprocess kill"),
        Err(_) => {
            warn!(role, "subprocess did not exit promptly after kill signal");
            let _ = child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests;
