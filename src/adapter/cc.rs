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
//! Out of v0.1 scope (deferred to follow-up PRs): wiring `UserMessage::ToolDecision`
//! into a real PreToolUse hook script (currently logged as a warning), and
//! parsing `terminal_reason` to detect budget-cap exits separately from crashes.

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::adapter::{
    AdapterError, AdapterResult, Engine, EngineAdapter, RoleConfig, RoleHandle, UserMessage,
};
use crate::crep::{CrepEvent, StopReason};

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

        let (tx_user, rx_user) = mpsc::channel::<UserMessage>(CHANNEL_CAPACITY);
        let (tx_events, rx_events) = mpsc::channel::<CrepEvent>(CHANNEL_CAPACITY);

        // Stdout reader: parse stream-json → CREP.
        tokio::spawn(read_stdout(
            config.name.clone(),
            priors_hash,
            stdout,
            tx_events.clone(),
        ));

        // Stdin writer: serialize user messages onto the engine's stdin.
        tokio::spawn(write_stdin(config.name.clone(), rx_user, stdin));

        // Process waiter: emit a final RoleStopped when the subprocess exits.
        tokio::spawn(wait_child(config.name.clone(), child, tx_events));

        Ok(RoleHandle {
            role: config.name,
            engine: Engine::Cc,
            tx_user,
            rx_events,
        })
    }
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
        let name = cap[1].to_string();
        if seen.insert(name.clone()) {
            out.push(name);
        }
    }
    out
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
            let mentions = parse_mentions(&text);
            vec![CrepEvent::RoleSpoke {
                role: role.to_owned(),
                text,
                mentions,
                cost_usd,
                cache_read,
            }]
        }

        "assistant" => extract_tool_uses(role, line),

        "user" => extract_tool_results(role, line),

        // Other event types (rate_limit_event, hook events, etc.) are
        // intentionally ignored at v0.1; they show up in the transcript
        // archive but don't have CREP equivalents yet.
        _ => Vec::new(),
    }
}

fn extract_tool_uses(role: &str, line: &Value) -> Vec<CrepEvent> {
    let Some(content) = line
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .map(|block| CrepEvent::ToolCallProposed {
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
        })
        .collect()
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
) {
    let mut lines = BufReader::new(stdout).lines();
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
                    if events.send(event).await.is_err() {
                        debug!(role, "event receiver dropped; stopping reader");
                        return;
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

async fn write_stdin(role: String, mut rx: mpsc::Receiver<UserMessage>, mut stdin: ChildStdin) {
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
            }
            UserMessage::ToolDecision { tool_use_id, .. } => {
                // PreToolUse hook integration arrives in a follow-up PR.
                // For v0.1 the wrapper isn't yet capable of producing tool
                // decisions, so this branch is dead in practice — but logged
                // loudly so we notice if it ever gets exercised by accident.
                warn!(
                    role,
                    tool_use_id, "ToolDecision delivered but not yet wired through to a hook"
                );
            }
        }
    }
    debug!(role, "user-message channel closed; closing stdin");
}

async fn wait_child(role: String, mut child: Child, events: mpsc::Sender<CrepEvent>) {
    let reason = match child.wait().await {
        Ok(status) if status.success() => StopReason::Completed,
        Ok(_) => StopReason::Crashed,
        Err(error) => {
            warn!(role, %error, "error waiting on subprocess");
            StopReason::Crashed
        }
    };
    let _ = events.send(CrepEvent::RoleStopped { role, reason }).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn fingerprint_is_stable_for_same_input() {
        let a = fingerprint("hello world");
        let b = fingerprint("hello world");
        assert_eq!(a, b);
        assert!(a.starts_with("dh1:"));
    }

    #[test]
    fn fingerprint_changes_with_content() {
        assert_ne!(fingerprint("a"), fingerprint("b"));
    }

    #[test]
    fn parse_mentions_picks_up_simple_names() {
        let text = "Will check with @security and @frontend.";
        assert_eq!(
            parse_mentions(text),
            vec!["security".to_owned(), "frontend".to_owned()]
        );
    }

    #[test]
    fn parse_mentions_dedupes_in_order() {
        let text = "@a needs @b which needs @a again.";
        assert_eq!(parse_mentions(text), vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn parse_mentions_allows_dashes_and_digits() {
        let text = "ping @data-team-7 about it";
        assert_eq!(parse_mentions(text), vec!["data-team-7".to_owned()]);
    }

    #[test]
    fn parse_mentions_ignores_emails_and_punctuation() {
        // @foo.bar — match stops at the dot
        let text = "send to user@example.com and ping @ops!";
        assert_eq!(
            parse_mentions(text),
            vec!["example".to_owned(), "ops".to_owned()]
        );
    }

    #[test]
    fn translate_system_init_yields_role_started() {
        let line = json!({
            "type": "system",
            "subtype": "init",
            "session_id": "abc-123",
            "model": "claude-opus-4-7",
            "tools": ["Bash", "Edit"],
        });
        let events = translate("backend", "dh1:0000", &line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CrepEvent::RoleStarted {
                role,
                engine,
                model,
                session_id,
                priors_hash,
            } => {
                assert_eq!(role, "backend");
                assert_eq!(engine, "cc");
                assert_eq!(model, "claude-opus-4-7");
                assert_eq!(session_id, "abc-123");
                assert_eq!(priors_hash, "dh1:0000");
            }
            other => panic!("expected RoleStarted, got {other:?}"),
        }
    }

    #[test]
    fn translate_result_yields_role_spoke_with_cost_and_cache() {
        let line = json!({
            "type": "result",
            "subtype": "success",
            "result": "Will defer to @security on rate limits.",
            "total_cost_usd": 0.0625,
            "usage": {
                "cache_read_input_tokens": 17889,
                "cache_creation_input_tokens": 8584,
            },
        });
        let events = translate("backend", "dh1:0", &line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CrepEvent::RoleSpoke {
                role,
                text,
                mentions,
                cost_usd,
                cache_read,
            } => {
                assert_eq!(role, "backend");
                assert!(text.contains("@security"));
                assert_eq!(mentions, &vec!["security".to_owned()]);
                assert!((*cost_usd - 0.0625).abs() < 1e-9);
                assert_eq!(*cache_read, 17_889);
            }
            other => panic!("expected RoleSpoke, got {other:?}"),
        }
    }

    #[test]
    fn translate_assistant_with_tool_use_yields_tool_call_proposed() {
        let line = json!({
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "text", "text": "I'll list the files."},
                    {
                        "type": "tool_use",
                        "id": "toolu_01abc",
                        "name": "Bash",
                        "input": {"command": "ls -la", "description": "list files"}
                    }
                ]
            }
        });
        let events = translate("backend", "h", &line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CrepEvent::ToolCallProposed {
                role,
                tool_name,
                tool_use_id,
                tool_input,
            } => {
                assert_eq!(role, "backend");
                assert_eq!(tool_name, "Bash");
                assert_eq!(tool_use_id, "toolu_01abc");
                assert_eq!(tool_input["command"], "ls -la");
            }
            other => panic!("expected ToolCallProposed, got {other:?}"),
        }
    }

    #[test]
    fn translate_user_with_tool_result_yields_tool_call_executed() {
        let line = json!({
            "type": "user",
            "message": {
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_01abc",
                        "content": "total 12\ndrwxr-xr-x ...",
                        "is_error": false,
                    }
                ]
            }
        });
        let events = translate("backend", "h", &line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CrepEvent::ToolCallExecuted {
                role,
                tool_use_id,
                ok,
                output_summary,
            } => {
                assert_eq!(role, "backend");
                assert_eq!(tool_use_id, "toolu_01abc");
                assert!(ok);
                assert!(output_summary.starts_with("total 12"));
            }
            other => panic!("expected ToolCallExecuted, got {other:?}"),
        }
    }

    #[test]
    fn translate_unknown_type_yields_nothing() {
        let line = json!({"type": "rate_limit_event", "rate_limit_info": {}});
        assert!(translate("r", "h", &line).is_empty());
    }

    #[test]
    fn translate_missing_type_yields_nothing() {
        let line = json!({"some": "noise"});
        assert!(translate("r", "h", &line).is_empty());
    }

    #[test]
    fn truncate_under_limit_is_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_over_limit_appends_ellipsis() {
        let out = truncate("0123456789abcdef", 8);
        assert_eq!(out.chars().count(), 8);
        assert!(out.ends_with('…'));
    }
}
