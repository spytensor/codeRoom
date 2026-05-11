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
pub mod gemini;

use std::path::PathBuf;

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::crep::CrepEvent;
use crate::turn::TurnId;

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

    /// Work-card tracing capability for this engine adapter.
    #[must_use]
    pub const fn work_trace(self) -> WorkTraceCapability {
        match self {
            Self::Cc => WorkTraceCapability::from_bits(
                WorkTraceCapability::CR_TASK_TITLES
                    | WorkTraceCapability::EARLY_WORK_TITLES
                    | WorkTraceCapability::LIVE_TOOL_STEPS,
            ),
            Self::Codex | Self::Gemini => WorkTraceCapability::from_bits(
                WorkTraceCapability::CR_TASK_TITLES
                    | WorkTraceCapability::LIVE_TOOL_STEPS
                    | WorkTraceCapability::PARTIAL_TRACE,
            ),
        }
    }

    /// Session model exposed by this adapter. See [`SessionKind`].
    #[must_use]
    pub const fn session_kind(self) -> SessionKind {
        match self {
            Self::Cc => SessionKind::SessionBound,
            Self::Codex | Self::Gemini => SessionKind::StatelessDispatch,
        }
    }
}

/// Engine session model.
///
/// v0.2 makes the distinction explicit because per-role queue
/// semantics differ between the two: a queued turn against
/// [`SessionKind::SessionBound`] reuses cached priors and conversation
/// state; a queued turn against [`SessionKind::StatelessDispatch`]
/// forks a fresh engine invocation with no inter-turn state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// One long-lived engine session per role. Multiple turns share
    /// session state (priors-cache, conversation memory). cc fits here.
    SessionBound,
    /// Each turn is a fresh engine invocation. No inter-turn state.
    /// codex (single-turn MCP today) and gemini (per-turn `gemini -p`)
    /// fit here.
    StatelessDispatch,
}

/// Adapter-level capability flags for WorkCard rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkTraceCapability {
    bits: u8,
}

impl WorkTraceCapability {
    const CR_TASK_TITLES: u8 = 1 << 0;
    const LIVE_TOOL_STEPS: u8 = 1 << 1;
    const NATIVE_DELEGATES: u8 = 1 << 2;
    const PARTIAL_TRACE: u8 = 1 << 3;
    const EARLY_WORK_TITLES: u8 = 1 << 4;

    const fn from_bits(bits: u8) -> Self {
        Self { bits }
    }

    /// Whether the adapter extracts `cr-task` title blocks into WorkTitle.
    #[must_use]
    pub const fn cr_task_titles(self) -> bool {
        self.bits & Self::CR_TASK_TITLES != 0
    }

    /// Whether work titles can arrive before live tool-step events.
    #[must_use]
    pub const fn early_work_titles(self) -> bool {
        self.bits & Self::EARLY_WORK_TITLES != 0
    }

    /// Whether tool steps can arrive while the role is still running.
    #[must_use]
    pub const fn live_tool_steps(self) -> bool {
        self.bits & Self::LIVE_TOOL_STEPS != 0
    }

    /// Whether native nested task/subagent events are exposed as work units.
    #[must_use]
    pub const fn native_delegates(self) -> bool {
        self.bits & Self::NATIVE_DELEGATES != 0
    }

    /// Whether traces may be incomplete compared with the engine's internal work.
    #[must_use]
    pub const fn partial_trace(self) -> bool {
        self.bits & Self::PARTIAL_TRACE != 0
    }
}

/// Wrapper permission mode for engine tool calls.
///
/// Wire format in `config.toml` is the lower-case variant name
/// (`"ask"`, `"auto"`, `"bypass"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    /// Ask before tools that are not explicitly allowed.
    Ask,
    /// Allow low-risk read-only tools; ask for risky or unknown tools.
    Auto,
    /// Let the engine run with its native bypass/yolo mode.
    Bypass,
}

impl PermissionMode {
    /// Stable string id for config files, CLI flags, and hook command args.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::Bypass => "bypass",
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
    /// Permission mode applied to this role's tool calls.
    pub permission_mode: PermissionMode,
    /// Optional path to the session policy file used by hook-backed
    /// adapters. `None` means no session-level `/allow` or `/deny`
    /// overrides are available.
    pub permission_policy_path: Option<PathBuf>,
    /// Optional Unix-domain-socket path the adapter should expose to
    /// hook subprocesses via `CODEROOM_PERMISSION_SOCKET`. Set when the
    /// REPL has a live permission bridge listening; `None` for headless
    /// `cr show` / smoke tests where no user is available to prompt.
    pub permission_socket_path: Option<PathBuf>,
    /// Optional prior-session id to resume. When set, the adapter wires
    /// the engine-native resume flag (cc: `--resume <id>`; codex /
    /// gemini equivalents added separately) so the role picks up its
    /// previous conversation. `None` starts a fresh session.
    ///
    /// Per amendment A-006, the REPL populates this from
    /// `.coderoom/sessions/ids/<role>.id` so `cr start` behaves like
    /// `claude --continue` / `codex --resume` — the user does not lose
    /// their working context between invocations.
    pub resume_session_id: Option<String>,
}

/// Channel capacity for the per-role interrupt mailbox. Multiple
/// successive `/halt` requests from the user (or repeated Ctrl-C) must
/// not block; the adapter drains this whenever it picks up a new turn
/// or finishes the current one.
pub(crate) const INTERRUPT_CHANNEL_CAPACITY: usize = 8;

/// A live handle to a running role: send the role new prompts, observe its
/// CREP event stream, request turn-level cancellation, and stop the role
/// when done.
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
    /// One-shot shutdown request consumed by the adapter's process
    /// waiter. REPL commands use this instead of relying on channel
    /// close / `kill_on_drop` side effects.
    pub stop_tx: oneshot::Sender<crate::crep::StopReason>,
    /// Channel for **turn-level** cancellation. Sending a [`TurnId`]
    /// asks the adapter to abort that turn while keeping the role
    /// process alive, so the next dispatch reaches a working role.
    /// See `docs/v0.2-trust-and-interrupt.md` § C.2.
    pub interrupt_tx: mpsc::Sender<TurnId>,
    tempfiles: Vec<tempfile::NamedTempFile>,
}

/// Owned pieces of a [`RoleHandle`] after the REPL takes over event
/// forwarding and role lifetime management.
#[derive(Debug)]
pub struct RoleHandleParts {
    /// Role's display name.
    pub role: String,
    /// Engine driving this role.
    pub engine: Engine,
    /// Channel to send new user prompts into the live session.
    pub tx_user: mpsc::Sender<UserMessage>,
    /// Channel of CREP events emitted by this role.
    pub rx_events: mpsc::Receiver<CrepEvent>,
    /// One-shot shutdown request consumed by the adapter's process waiter.
    pub stop_tx: oneshot::Sender<crate::crep::StopReason>,
    /// Channel for turn-level cancellation. See [`RoleHandle::interrupt_tx`].
    pub interrupt_tx: mpsc::Sender<TurnId>,
    /// Adapter-owned tempfiles that must remain alive for the role lifetime.
    pub tempfiles: Vec<tempfile::NamedTempFile>,
}

impl RoleHandle {
    /// Construct a role handle without extra adapter-owned tempfiles.
    #[must_use]
    pub fn new(
        role: String,
        engine: Engine,
        tx_user: mpsc::Sender<UserMessage>,
        rx_events: mpsc::Receiver<CrepEvent>,
        stop_tx: oneshot::Sender<crate::crep::StopReason>,
        interrupt_tx: mpsc::Sender<TurnId>,
    ) -> Self {
        Self::new_with_tempfiles(
            role,
            engine,
            tx_user,
            rx_events,
            stop_tx,
            interrupt_tx,
            Vec::new(),
        )
    }

    /// Construct a role handle and keep adapter-owned tempfiles alive for
    /// the handle's lifetime.
    #[must_use]
    pub fn new_with_tempfiles(
        role: String,
        engine: Engine,
        tx_user: mpsc::Sender<UserMessage>,
        rx_events: mpsc::Receiver<CrepEvent>,
        stop_tx: oneshot::Sender<crate::crep::StopReason>,
        interrupt_tx: mpsc::Sender<TurnId>,
        tempfiles: Vec<tempfile::NamedTempFile>,
    ) -> Self {
        Self {
            role,
            engine,
            tx_user,
            rx_events,
            stop_tx,
            interrupt_tx,
            tempfiles,
        }
    }

    /// Consume the handle and return all owned runtime pieces, including
    /// adapter tempfiles that must be kept alive by the REPL.
    #[must_use]
    pub fn into_parts(self) -> RoleHandleParts {
        RoleHandleParts {
            role: self.role,
            engine: self.engine,
            tx_user: self.tx_user,
            rx_events: self.rx_events,
            stop_tx: self.stop_tx,
            interrupt_tx: self.interrupt_tx,
            tempfiles: self.tempfiles,
        }
    }
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

pub(crate) fn role_spoke_events_from_text(
    role: &str,
    text: &str,
    cost_usd: f64,
    cache_read: u64,
) -> Vec<CrepEvent> {
    let extracted = crate::work::extract_cr_task(text);
    let mut events = Vec::new();
    if let Some(title) = extracted.title {
        events.push(CrepEvent::WorkTitle {
            role: role.to_owned(),
            title,
            turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
            thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
        });
    }
    let body = extracted.body.trim().to_owned();
    let mentions = cc::parse_mentions(&body);
    events.push(CrepEvent::RoleSpoke {
        role: role.to_owned(),
        text: body,
        mentions,
        cost_usd,
        cache_read,
        turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
        thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
    });
    events
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
    fn work_trace_capabilities_reflect_adapter_modes() {
        let cc = Engine::Cc.work_trace();
        assert!(cc.cr_task_titles());
        assert!(cc.early_work_titles());
        assert!(cc.live_tool_steps());
        assert!(!cc.native_delegates());
        assert!(!cc.partial_trace());

        let codex = Engine::Codex.work_trace();
        assert!(codex.cr_task_titles());
        assert!(!codex.early_work_titles());
        assert!(codex.live_tool_steps());
        assert!(Engine::Codex.work_trace().partial_trace());

        let gemini = Engine::Gemini.work_trace();
        assert!(gemini.cr_task_titles());
        assert!(!gemini.early_work_titles());
        assert!(gemini.live_tool_steps());
        assert!(gemini.partial_trace());
    }

    #[test]
    fn permission_mode_lowercase_in_config_toml_form() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Sample {
            permission_mode: PermissionMode,
        }
        let sample = Sample {
            permission_mode: PermissionMode::Auto,
        };
        let toml_text = toml::to_string(&sample).unwrap();
        assert!(
            toml_text.contains("permission_mode = \"auto\""),
            "got: {toml_text}"
        );
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

    #[test]
    fn role_spoke_events_extract_work_title_and_clean_text() {
        let events = role_spoke_events_from_text(
            "qa",
            "```cr-task\nReview adapter timeout paths\n```\n\nI checked with @backend.",
            0.5,
            42,
        );
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            CrepEvent::WorkTitle {
                role: "qa".into(),
                title: "Review adapter timeout paths".into(),
                turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
            }
        );
        assert_eq!(
            events[1],
            CrepEvent::RoleSpoke {
                role: "qa".into(),
                text: "I checked with @backend.".into(),
                mentions: vec!["backend".into()],
                cost_usd: 0.5,
                cache_read: 42,
                turn_id: crate::turn::LEGACY_TURN_ID.to_owned(),
                thread_id: crate::turn::LEGACY_TURN_ID.to_owned(),
            }
        );
    }

    #[test]
    fn role_handle_into_parts_transfers_tempfiles() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.path().to_owned();
        let (tx_user, _rx_user) = mpsc::channel::<UserMessage>(1);
        let (_tx_events, rx_events) = mpsc::channel::<CrepEvent>(1);
        let (stop_tx, _stop_rx) = oneshot::channel();
        let (interrupt_tx, _interrupt_rx) = mpsc::channel(1);

        let handle = RoleHandle::new_with_tempfiles(
            "qa".into(),
            Engine::Cc,
            tx_user,
            rx_events,
            stop_tx,
            interrupt_tx,
            vec![temp],
        );
        let parts = handle.into_parts();

        assert!(path.exists(), "tempfile should stay alive in handle parts");
        drop(parts.tempfiles);
        assert!(!path.exists(), "tempfile should be cleaned up after drop");
    }
}
