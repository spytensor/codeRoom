//! Gemini engine adapter.
//!
//! v0.1 implementation is intentionally the simplest of the three: per
//! user message we spawn `gemini -p` once, capture its stdout, and emit
//! a single [`CrepEvent::RoleSpoke`]. There is no long-lived subprocess.
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
//! - No streaming, no tool-call lifecycle, no multi-turn cache reuse.
//!   All three arrive in a follow-up PR (`feat/adapter-gemini-v2`)
//!   either via `gemini -o stream-json` (CC-shape) or `--experimental-acp`.
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
    AdapterError, AdapterResult, Engine, EngineAdapter, RoleConfig, RoleHandle, UserMessage,
};
use crate::crep::{CrepEvent, StopReason};

const CHANNEL_CAPACITY: usize = 32;
const GEMINI_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const GEMINI_UNTRUSTED_PRIORS_ENV: &str = "CODEROOM_GEMINI_UNTRUSTED_PRIORS";

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

        Ok(RoleHandle {
            role: config.name,
            engine: Engine::Gemini,
            tx_user,
            rx_events,
            stop_tx,
        })
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
            )
            .await
            {
                Ok(text) => {
                    let mentions = crate::adapter::cc::parse_mentions(&text);
                    let _ = self
                        .events
                        .send(CrepEvent::RoleSpoke {
                            role: self.role.clone(),
                            text,
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

/// Run a single `gemini -p` invocation with priors prepended and return
/// its stdout (text mode).
async fn run_one_turn(
    gemini_path: &PathBuf,
    model: Option<&str>,
    priors_path: &PathBuf,
    priors_text: &str,
    prompt_mode: GeminiPromptMode,
    user_prompt: &str,
) -> std::io::Result<String> {
    let mut cmd = Command::new(gemini_path);
    cmd.arg("-p")
        .arg(match prompt_mode {
            GeminiPromptMode::SystemInstructionFile => user_prompt.to_owned(),
            GeminiPromptMode::InlineUntrusted => {
                format!("{priors_text}\n\n---\n\n{user_prompt}")
            }
        })
        .arg("--output-format")
        .arg("text")
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
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
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
}
