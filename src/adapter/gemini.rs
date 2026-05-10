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
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::adapter::{
    AdapterError, AdapterResult, Engine, EngineAdapter, PermissionMode, RoleConfig, RoleHandle,
    UserMessage,
};
use crate::crep::{CrepEvent, StopReason};

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
            }
            .run(),
        );

        Ok(RoleHandle::new(
            config.name,
            Engine::Gemini,
            tx_user,
            rx_events,
            stop_tx,
        ))
    }
}

async fn probe_gemini(gemini_path: &PathBuf) -> Result<GeminiPromptMode, String> {
    let mut cmd = Command::new(gemini_path);
    cmd.arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
}

impl GeminiLoop {
    async fn run(mut self) {
        let stop_reason = loop {
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
            match run_one_turn(
                &self.gemini_path,
                self.model.as_deref(),
                &self.priors_path,
                &self.priors_text,
                self.prompt_mode,
                &prompt,
                &self.role,
            )
            .await
            {
                Ok(turn) => {
                    for event in turn.events {
                        let _ = self.events.send(event).await;
                    }
                    let mentions = crate::adapter::cc::parse_mentions(&turn.text);
                    let _ = self
                        .events
                        .send(CrepEvent::RoleSpoke {
                            role: self.role.clone(),
                            text: turn.text,
                            mentions,
                            cost_usd: 0.0,
                            cache_read: 0,
                        })
                        .await;
                }
                Err(error) => {
                    warn!(role = %self.role, %error, "gemini turn failed");
                    let _ = self
                        .events
                        .send(CrepEvent::RoleSpoke {
                            role: self.role.clone(),
                            text: format!("[gemini error: {error}]"),
                            mentions: Vec::new(),
                            cost_usd: 0.0,
                            cache_read: 0,
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
            })
            .await;
        debug!("gemini per-turn loop exiting");
    }
}

#[derive(Debug, Clone, PartialEq)]
struct GeminiTurn {
    text: String,
    events: Vec<CrepEvent>,
}

/// Run a single `gemini -p` invocation and translate its stream-json stdout.
async fn run_one_turn(
    gemini_path: &PathBuf,
    model: Option<&str>,
    priors_path: &PathBuf,
    priors_text: &str,
    prompt_mode: GeminiPromptMode,
    user_prompt: &str,
    role: &str,
) -> std::io::Result<GeminiTurn> {
    let mut cmd = Command::new(gemini_path);
    cmd.arg("-p")
        .arg(match prompt_mode {
            GeminiPromptMode::SystemInstructionFile => user_prompt.to_owned(),
            GeminiPromptMode::InlineUntrusted => {
                format!("{priors_text}\n\n---\n\n{user_prompt}")
            }
        })
        .arg("--output-format")
        .arg("stream-json")
        .arg("-y")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if prompt_mode == GeminiPromptMode::SystemInstructionFile {
        cmd.arg("--system-instruction-file").arg(priors_path);
    }
    if let Some(model) = model {
        cmd.arg("--model").arg(model);
    }

    let output = cmd.output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(std::io::Error::other(format!(
            "gemini exited with {}: {stderr}",
            output.status
        )));
    }
    Ok(parse_stream_json_turn(
        role,
        &String::from_utf8_lossy(&output.stdout),
    ))
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
                },
                CrepEvent::ToolCallExecuted {
                    role: "backend".into(),
                    tool_use_id: "read-1".into(),
                    ok: true,
                    output_summary: "hello".into(),
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
            }]
        );
    }
}
