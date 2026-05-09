//! Engine adapter abstraction.
//!
//! Each engine CLI (Claude Code, Codex, Gemini) has a small adapter that
//! knows how to spawn its subprocess, translate its native event stream
//! into [`crate::crep::CrepEvent`]s, and accept commands (send-user,
//! deny-tool, etc.) routed back from the wrapper.
//!
//! The trait is async via `async_trait`-free style: each method returns a
//! concrete future-bearing type that callers can `await`. This keeps the
//! trait object-safe for boxed dispatch (`Box<dyn EngineAdapter>`) and
//! avoids the indirection cost of `async_trait`'s `Pin<Box<...>>` wrapper.

pub mod cc;
pub mod codex;

use std::path::PathBuf;

use thiserror::Error;
use tokio::sync::mpsc;

use crate::crep::CrepEvent;

/// Which engine drives a given role.
///
/// Wire format in `config.toml` is the lower-case variant name
/// (`"cc"`, `"codex"`, `"gemini"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    /// Anthropic Claude Code CLI (`claude` binary).
    Cc,
    /// OpenAI Codex CLI (`codex` binary).
    Codex,
    /// Google Gemini CLI (`gemini` binary).
    Gemini,
}

impl Engine {
    /// Stable string id for this engine, used in CREP `engine` fields and
    /// in user-facing config files.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cc => "cc",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }
}

/// Static configuration for a role, supplied to an adapter at spawn time.
#[derive(Debug, Clone)]
pub struct RoleConfig {
    /// The role's display name (e.g. `"backend"`). Forms part of every
    /// CREP event the adapter emits.
    pub name: String,
    /// Engine to drive this role.
    pub engine: Engine,
    /// Optional model override (e.g. `"opus"`, `"sonnet"`). When `None`,
    /// the adapter uses the engine's default.
    pub model: Option<String>,
    /// Path to the composed system prompt file. The adapter is expected
    /// to load this verbatim into the engine's session.
    pub priors_path: PathBuf,
    /// Maximum dollar amount the engine may spend on this session before
    /// the adapter forces a `RoleStopped { reason: Budget }`.
    pub budget_usd: f64,
}

/// A live handle to a running role: send the role new prompts, observe its
/// CREP event stream, and stop it when done.
///
/// The adapter owns the underlying subprocess; dropping a `RoleHandle`
/// triggers a graceful `stop()` via the adapter's `Drop` implementation
/// (TBD per-adapter).
#[derive(Debug)]
pub struct RoleHandle {
    /// Role's display name (matches `RoleConfig::name`).
    pub role: String,
    /// Engine driving this role.
    pub engine: Engine,
    /// Channel to send new user prompts into the live session. Each
    /// message is delivered to the engine subprocess; the adapter is
    /// responsible for pacing (waiting for a `RoleSpoke` event before
    /// sending the next prompt).
    pub tx_user: mpsc::Sender<UserMessage>,
    /// Channel of CREP events emitted by this role.
    pub rx_events: mpsc::Receiver<CrepEvent>,
}

/// A user-originated message routed to a role's session.
#[derive(Debug, Clone)]
pub enum UserMessage {
    /// Free-form prompt text. The role decides how to respond.
    Prompt(String),
    /// Wrapper's verdict on a previously-proposed tool call.
    /// Sent in response to `CrepEvent::ToolCallProposed`.
    ToolDecision {
        /// Engine-issued tool-use id from the matching `ToolCallProposed`.
        tool_use_id: String,
        /// Whether to allow the tool call.
        allow: bool,
        /// Reason shown to the engine if denied.
        reason: Option<String>,
    },
}

/// Errors an adapter may surface during start/teardown.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// The engine binary could not be found on `PATH`.
    #[error("engine `{0}` binary not found on PATH")]
    BinaryNotFound(&'static str),
    /// The engine subprocess could not be spawned.
    #[error("failed to spawn `{engine}` subprocess: {source}")]
    Spawn {
        /// The engine that failed to start.
        engine: &'static str,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The role's priors file was missing or unreadable.
    #[error("priors file `{path}` could not be read: {source}")]
    PriorsRead {
        /// The unreadable path.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The wrapper's stdin channel to the engine is closed.
    #[error("user-message channel for role `{0}` is closed")]
    SendClosed(String),
    /// Catch-all for engine-specific failures.
    #[error("engine `{engine}` error: {message}")]
    Engine {
        /// The engine that produced the error.
        engine: &'static str,
        /// Adapter-supplied message.
        message: String,
    },
}

/// Convenience alias for adapter results.
pub type AdapterResult<T> = Result<T, AdapterError>;

/// The engine-adapter contract.
///
/// Implementations live in `adapter::cc`, `adapter::codex`, `adapter::gemini`.
/// The wrapper holds them as `Box<dyn EngineAdapter>` and dispatches by
/// the role's configured [`Engine`].
#[allow(async_fn_in_trait)] // intentional: keep trait object-safe and avoid async_trait Pin<Box<...>>
pub trait EngineAdapter: Send + Sync {
    /// The engine this adapter drives. Returned by-value because it's
    /// used in error messages and event-tagging hot paths.
    fn engine(&self) -> Engine;

    /// Spawn a fresh role session.
    ///
    /// On success, returns a [`RoleHandle`] whose `rx_events` channel is
    /// pre-populated with at least a `CrepEvent::RoleStarted` once the
    /// engine has loaded the system prompt and reported its session id.
    async fn start(&self, config: RoleConfig) -> AdapterResult<RoleHandle>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn engine_as_str_matches_serde() {
        for engine in [Engine::Cc, Engine::Codex, Engine::Gemini] {
            let wire = serde_json::to_string(&engine).unwrap();
            // Serialized form is a JSON string ("cc"), strip quotes:
            let bare = wire.trim_matches('"');
            assert_eq!(bare, engine.as_str(), "{engine:?}");
        }
    }

    #[test]
    fn engine_round_trips_via_serde() {
        for engine in [Engine::Cc, Engine::Codex, Engine::Gemini] {
            let wire = serde_json::to_string(&engine).unwrap();
            let parsed: Engine = serde_json::from_str(&wire).unwrap();
            assert_eq!(engine, parsed);
        }
    }

    #[test]
    fn engine_lowercase_in_config_toml_form() {
        // toml roundtrip — config.toml will use this form.
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Sample {
            engine: Engine,
        }
        let sample = Sample {
            engine: Engine::Codex,
        };
        let toml_text = toml::to_string(&sample).unwrap();
        assert!(toml_text.contains("engine = \"codex\""), "got: {toml_text}");
        let parsed: Sample = toml::from_str(&toml_text).unwrap();
        assert_eq!(sample, parsed);
    }

    #[test]
    fn user_message_variants_construct() {
        let _prompt = UserMessage::Prompt("hello".into());
        let _decision = UserMessage::ToolDecision {
            tool_use_id: "toolu_x".into(),
            allow: false,
            reason: Some("denied by hook".into()),
        };
    }
}
